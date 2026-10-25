//! JSON-RPC 2.0 messages and the `Content-Length` framing used by LSP over stdio.
//!
//! Everything here is a pure function over [`BufRead`] / [`Write`], so it can be
//! used with pipes, sockets or in-memory buffers alike.

use std::io::{BufRead, Write};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Map, Value};

use crate::error::TransportError;

/// Largest message body accepted by [`read_message`] (64 MiB).
pub const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// JSON-RPC error code: the method is not supported.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// Reads one framed message body from `reader`.
///
/// Header names are matched case-insensitively, both `\r\n` and bare `\n`
/// line endings are accepted, unknown headers (such as `Content-Type`) are
/// ignored and stray blank lines between messages are skipped.
///
/// Returns `Ok(None)` when the stream ends cleanly between messages.
pub fn read_message<R: BufRead + ?Sized>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, TransportError> {
    let mut content_length: Option<usize> = None;
    let mut saw_header = false;
    let mut line = Vec::new();

    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return if saw_header {
                Err(TransportError::UnexpectedEof)
            } else {
                Ok(None)
            };
        }
        let trimmed = strip_line_ending(&line);
        if trimmed.is_empty() {
            if saw_header {
                break;
            }
            continue;
        }
        saw_header = true;

        let text = std::str::from_utf8(trimmed)
            .map_err(|_| TransportError::InvalidHeader(String::from_utf8_lossy(trimmed).into()))?;
        let (name, value) = text
            .split_once(':')
            .ok_or_else(|| TransportError::InvalidHeader(text.to_owned()))?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            let value = value.trim();
            let len = value
                .parse::<usize>()
                .map_err(|_| TransportError::InvalidContentLength(value.to_owned()))?;
            if len > MAX_MESSAGE_SIZE {
                return Err(TransportError::MessageTooLarge(len));
            }
            content_length = Some(len);
        }
    }

    let len = content_length.ok_or(TransportError::MissingContentLength)?;
    let mut body = vec![0; len];
    reader.read_exact(&mut body).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            TransportError::UnexpectedEof
        } else {
            TransportError::Io(e)
        }
    })?;
    Ok(Some(body))
}

/// Writes `body` to `writer` with a `Content-Length` header and flushes it.
pub fn write_message<W: Write + ?Sized>(writer: &mut W, body: &[u8]) -> std::io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)?;
    writer.flush()
}

fn strip_line_ending(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// A JSON-RPC request id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// A numeric id (what this client always sends).
    Number(i64),
    /// A string id (some servers use these for their own requests).
    String(String),
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestId::Number(n) => write!(f, "{n}"),
            RequestId::String(s) => write!(f, "{s:?}"),
        }
    }
}

/// The `error` member of a failed JSON-RPC response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseError {
    /// JSON-RPC error code (for example [`METHOD_NOT_FOUND`]).
    pub code: i64,
    /// Human-readable message.
    pub message: String,
    /// Optional extra data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A parsed JSON-RPC 2.0 message.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// A call that expects a response.
    Request {
        /// Correlation id.
        id: RequestId,
        /// Method name, e.g. `textDocument/hover`.
        method: String,
        /// Parameters (`Value::Null` when absent).
        params: Value,
    },
    /// The answer to an earlier request.
    Response {
        /// Id of the request being answered (`None` only for some error replies).
        id: Option<RequestId>,
        /// The result or the error.
        result: Result<Value, ResponseError>,
    },
    /// A one-way message.
    Notification {
        /// Method name, e.g. `textDocument/publishDiagnostics`.
        method: String,
        /// Parameters (`Value::Null` when absent).
        params: Value,
    },
}

#[derive(Deserialize)]
struct RawMessage {
    #[serde(default)]
    id: Option<RequestId>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default, deserialize_with = "present")]
    params: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    result: Option<Value>,
    #[serde(default)]
    error: Option<ResponseError>,
}

/// Distinguishes `"result": null` (present) from a missing member.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

impl Message {
    /// Parses a message body as produced by [`read_message`].
    pub fn parse(body: &[u8]) -> Result<Self, TransportError> {
        let raw: RawMessage = serde_json::from_slice(body)?;
        let params = raw.params.unwrap_or(Value::Null);
        match (raw.method, raw.id) {
            (Some(method), Some(id)) => Ok(Message::Request { id, method, params }),
            (Some(method), None) => Ok(Message::Notification { method, params }),
            (None, id) => match (raw.error, raw.result) {
                (Some(error), _) => Ok(Message::Response {
                    id,
                    result: Err(error),
                }),
                (None, Some(result)) if id.is_some() => Ok(Message::Response {
                    id,
                    result: Ok(result),
                }),
                _ => Err(TransportError::InvalidMessage(
                    "message has neither a method nor a result/error with an id".into(),
                )),
            },
        }
    }

    /// Serializes the message to its JSON body (without framing).
    ///
    /// `params` equal to `null` are omitted, which is what servers expect for
    /// parameterless methods such as `shutdown` and `exit`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut obj = Map::new();
        obj.insert("jsonrpc".into(), json!("2.0"));
        match self {
            Message::Request { id, method, params } => {
                obj.insert("id".into(), id_value(id));
                obj.insert("method".into(), Value::String(method.clone()));
                if !params.is_null() {
                    obj.insert("params".into(), params.clone());
                }
            }
            Message::Notification { method, params } => {
                obj.insert("method".into(), Value::String(method.clone()));
                if !params.is_null() {
                    obj.insert("params".into(), params.clone());
                }
            }
            Message::Response { id, result } => {
                obj.insert("id".into(), id.as_ref().map_or(Value::Null, id_value));
                match result {
                    Ok(value) => {
                        obj.insert("result".into(), value.clone());
                    }
                    Err(error) => {
                        let mut err = Map::new();
                        err.insert("code".into(), json!(error.code));
                        err.insert("message".into(), json!(error.message));
                        if let Some(data) = &error.data {
                            err.insert("data".into(), data.clone());
                        }
                        obj.insert("error".into(), Value::Object(err));
                    }
                }
            }
        }
        Value::Object(obj).to_string().into_bytes()
    }
}

fn id_value(id: &RequestId) -> Value {
    match id {
        RequestId::Number(n) => json!(n),
        RequestId::String(s) => json!(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor, Read};

    /// A reader that hands out at most `chunk` bytes per `read` call.
    struct Trickle {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.chunk.min(buf.len()).min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn framed(body: &str) -> Vec<u8> {
        let mut out = Vec::new();
        write_message(&mut out, body.as_bytes()).expect("write to Vec");
        out
    }

    fn read(raw: &[u8]) -> Result<Option<Vec<u8>>, TransportError> {
        read_message(&mut Cursor::new(raw.to_vec()))
    }

    #[test]
    fn write_produces_exact_frame() {
        assert_eq!(framed("{}"), b"Content-Length: 2\r\n\r\n{}".to_vec());
    }

    #[test]
    fn round_trips_multiple_messages_then_clean_eof() {
        let mut bytes = framed(r#"{"a":1}"#);
        bytes.extend(framed(r#"{"b":2}"#));
        bytes.extend(framed("\u{e9}\u{6f22}"));
        let mut r = Cursor::new(bytes);
        assert_eq!(read_message(&mut r).unwrap().unwrap(), br#"{"a":1}"#);
        assert_eq!(read_message(&mut r).unwrap().unwrap(), br#"{"b":2}"#);
        // Content-Length counts bytes, not characters.
        assert_eq!(
            read_message(&mut r).unwrap().unwrap(),
            "\u{e9}\u{6f22}".as_bytes()
        );
        assert!(read_message(&mut r).unwrap().is_none());
    }

    #[test]
    fn handles_one_byte_partial_reads() {
        let mut data = framed(r#"{"jsonrpc":"2.0","method":"x"}"#);
        data.extend(framed("[1,2,3]"));
        let trickle = Trickle {
            data,
            pos: 0,
            chunk: 1,
        };
        let mut r = BufReader::with_capacity(1, trickle);
        assert_eq!(
            read_message(&mut r).unwrap().unwrap(),
            br#"{"jsonrpc":"2.0","method":"x"}"#
        );
        assert_eq!(read_message(&mut r).unwrap().unwrap(), b"[1,2,3]");
        assert!(read_message(&mut r).unwrap().is_none());
    }

    #[test]
    fn headers_are_case_insensitive_and_extra_headers_ignored() {
        let raw = b"content-type: application/vscode-jsonrpc; charset=utf-8\r\nCONTENT-LENGTH:   4 \r\n\r\nnull\nContent-length: 2\n\n{}";
        let mut r = Cursor::new(raw.to_vec());
        assert_eq!(read_message(&mut r).unwrap().unwrap(), b"null");
        assert_eq!(read_message(&mut r).unwrap().unwrap(), b"{}");
        assert!(read_message(&mut r).unwrap().is_none());
    }

    #[test]
    fn framing_errors() {
        assert!(matches!(
            read(b"Content-Type: x\r\n\r\n{}"),
            Err(TransportError::MissingContentLength)
        ));
        assert!(matches!(
            read(b"Content-Length: abc\r\n\r\n"),
            Err(TransportError::InvalidContentLength(_))
        ));
        assert!(matches!(
            read(b"garbage\r\n\r\n"),
            Err(TransportError::InvalidHeader(_))
        ));
        assert!(matches!(
            read(b"Content-Length: 10\r\n\r\n{}"),
            Err(TransportError::UnexpectedEof)
        ));
        assert!(matches!(
            read(b"Content-Length: 10\r\n"),
            Err(TransportError::UnexpectedEof)
        ));
        let huge = format!("Content-Length: {}\r\n\r\n", MAX_MESSAGE_SIZE + 1);
        assert!(matches!(
            read(huge.as_bytes()),
            Err(TransportError::MessageTooLarge(_))
        ));
    }

    #[test]
    fn parses_message_kinds() {
        let req = Message::parse(
            br#"{"jsonrpc":"2.0","id":"a1","method":"workspace/configuration","params":{"items":[]}}"#,
        )
        .unwrap();
        assert!(matches!(req, Message::Request { id: RequestId::String(ref s), .. } if s == "a1"));

        let note = Message::parse(br#"{"jsonrpc":"2.0","method":"exit"}"#).unwrap();
        assert_eq!(
            note,
            Message::Notification {
                method: "exit".into(),
                params: Value::Null
            }
        );

        let ok_null = Message::parse(br#"{"jsonrpc":"2.0","id":3,"result":null}"#).unwrap();
        assert_eq!(
            ok_null,
            Message::Response {
                id: Some(RequestId::Number(3)),
                result: Ok(Value::Null)
            }
        );

        let err = Message::parse(
            br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse"}}"#,
        )
        .unwrap();
        assert!(
            matches!(err, Message::Response { id: None, result: Err(ref e) } if e.code == -32700)
        );

        assert!(matches!(
            Message::parse(br#"{"jsonrpc":"2.0","id":1}"#),
            Err(TransportError::InvalidMessage(_))
        ));
        assert!(matches!(
            Message::parse(b"not json"),
            Err(TransportError::Json(_))
        ));
    }

    #[test]
    fn serialization_round_trips_and_omits_null_params() {
        let msgs = [
            Message::Request {
                id: RequestId::Number(7),
                method: "shutdown".into(),
                params: Value::Null,
            },
            Message::Notification {
                method: "textDocument/didClose".into(),
                params: json!({"textDocument": {"uri": "file:///a.py"}}),
            },
            Message::Response {
                id: Some(RequestId::String("x".into())),
                result: Ok(Value::Null),
            },
            Message::Response {
                id: Some(RequestId::Number(1)),
                result: Err(ResponseError {
                    code: METHOD_NOT_FOUND,
                    message: "nope".into(),
                    data: None,
                }),
            },
        ];
        for msg in &msgs {
            assert_eq!(&Message::parse(&msg.to_bytes()).unwrap(), msg);
        }
        let v: Value = serde_json::from_slice(&msgs[0].to_bytes()).unwrap();
        assert!(v.get("params").is_none());
        assert_eq!(v["jsonrpc"], "2.0");
        // A null result must still be sent explicitly.
        let v: Value = serde_json::from_slice(&msgs[2].to_bytes()).unwrap();
        assert!(v.get("result").is_some_and(Value::is_null));
    }
}
