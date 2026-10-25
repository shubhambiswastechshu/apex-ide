//! Client tests against an in-process fake server connected by in-memory pipes.

use super::*;
use std::io::{self, BufReader};
use std::thread;

use lsp_types::{MessageType, Range};

// ---------------------------------------------------------------- pipes

/// Write half of an in-memory pipe.
struct PipeWriter(Sender<Vec<u8>>);

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !buf.is_empty() {
            self.0
                .send(buf.to_vec())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"))?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Read half of an in-memory pipe; returns EOF once the writer is dropped.
struct PipeReader {
    rx: Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    pos: usize,
}

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.chunk.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.chunk.len() - self.pos);
        out[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn pipe() -> (PipeReader, PipeWriter) {
    let (tx, rx) = crossbeam_channel::unbounded();
    (
        PipeReader {
            rx,
            chunk: Vec::new(),
            pos: 0,
        },
        PipeWriter(tx),
    )
}

// ---------------------------------------------------------- fake server

const STEP: Duration = Duration::from_secs(5);

/// Custom request types used to drive the fake server.
macro_rules! test_request {
    ($name:ident, $method:literal) => {
        enum $name {}
        impl Request for $name {
            type Params = Value;
            type Result = Value;
            const METHOD: &'static str = $method;
        }
    };
}
test_request!(Hold, "test/hold");
test_request!(Echo, "test/echo");
test_request!(Slow, "test/slow");
test_request!(Fail, "test/fail");
test_request!(Die, "test/die");

fn send(out: &mut PipeWriter, message: &Message) {
    transport::write_message(out, &message.to_bytes()).expect("fake server write");
}

fn respond(out: &mut PipeWriter, id: RequestId, result: Value) {
    send(
        out,
        &Message::Response {
            id: Some(id),
            result: Ok(result),
        },
    );
}

/// Runs a scripted server; every message it receives is also reported on `seen`.
fn fake_server(input: PipeReader, mut out: PipeWriter, seen: Sender<Message>) {
    let mut input = BufReader::new(input);
    let mut held: Option<RequestId> = None;
    while let Ok(Some(body)) = transport::read_message(&mut input) {
        let message = Message::parse(&body).expect("client sent valid JSON-RPC");
        let _ = seen.send(message.clone());
        match message {
            Message::Request { id, method, params } => match method.as_str() {
                "initialize" => {
                    // Ask the client things before answering, like real servers do.
                    send(
                        &mut out,
                        &Message::Request {
                            id: RequestId::String("cfg-1".into()),
                            method: "workspace/configuration".into(),
                            params: json!({"items": [{"section": "python"}, {"section": "python.analysis"}]}),
                        },
                    );
                    send(
                        &mut out,
                        &Message::Request {
                            id: RequestId::Number(99),
                            method: "window/workDoneProgress/create".into(),
                            params: json!({"token": "indexing"}),
                        },
                    );
                    send(
                        &mut out,
                        &Message::Request {
                            id: RequestId::Number(100),
                            method: "custom/unknown".into(),
                            params: Value::Null,
                        },
                    );
                    send(
                        &mut out,
                        &Message::Notification {
                            method: "window/logMessage".into(),
                            params: json!({"type": 3, "message": "fake server ready"}),
                        },
                    );
                    respond(
                        &mut out,
                        id,
                        json!({
                            "capabilities": {
                                "textDocumentSync": 1,
                                "completionProvider": {"triggerCharacters": ["."]},
                                "hoverProvider": true,
                                "definitionProvider": true
                            },
                            "serverInfo": {"name": "fake", "version": "1.0"}
                        }),
                    );
                }
                "textDocument/completion" => respond(
                    &mut out,
                    id,
                    json!({"isIncomplete": false, "items": [
                        {"label": "beta", "sortText": "2"},
                        {"label": "alpha", "sortText": "1", "kind": 3, "detail": "def alpha()"}
                    ]}),
                ),
                "textDocument/hover" => respond(
                    &mut out,
                    id,
                    json!({"contents": {"kind": "markdown", "value": "**doc**"}}),
                ),
                "textDocument/definition" => {
                    let uri = params["textDocument"]["uri"].clone();
                    respond(
                        &mut out,
                        id,
                        json!([{
                            "targetUri": uri,
                            "targetRange": {"start": {"line": 0, "character": 0}, "end": {"line": 2, "character": 0}},
                            "targetSelectionRange": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 9}}
                        }]),
                    );
                }
                "test/hold" => held = Some(id),
                "test/echo" => {
                    // Answer out of order: the echo first, then the held request.
                    respond(&mut out, id, params);
                    if let Some(held_id) = held.take() {
                        respond(&mut out, held_id, json!("held"));
                    }
                }
                "test/fail" => send(
                    &mut out,
                    &Message::Response {
                        id: Some(id),
                        result: Err(ResponseError {
                            code: -32000,
                            message: "boom".into(),
                            data: None,
                        }),
                    },
                ),
                "test/die" => return,
                "shutdown" => respond(&mut out, id, Value::Null),
                _ => {} // e.g. test/slow: never answered
            },
            Message::Notification { method, params } => match method.as_str() {
                "textDocument/didOpen" => send(
                    &mut out,
                    &Message::Notification {
                        method: "textDocument/publishDiagnostics".into(),
                        params: json!({
                            "uri": params["textDocument"]["uri"],
                            "version": params["textDocument"]["version"],
                            "diagnostics": [{
                                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
                                "severity": 1,
                                "message": "undefined name"
                            }]
                        }),
                    },
                ),
                "exit" => return,
                _ => {}
            },
            Message::Response { .. } => {}
        }
    }
}

struct Harness {
    client: LspClient,
    seen: Receiver<Message>,
    server: Option<thread::JoinHandle<()>>,
}

impl Harness {
    fn new() -> Self {
        let (client_in, server_out) = pipe();
        let (server_in, client_out) = pipe();
        let (seen_tx, seen) = crossbeam_channel::unbounded();
        let server = thread::spawn(move || fake_server(server_in, server_out, seen_tx));
        let root: Uri = "file:///workspace/proj".parse().unwrap();
        let client =
            LspClient::connect(client_in, client_out, Some(root), ClientOptions::default())
                .unwrap();
        Harness {
            client,
            seen,
            server: Some(server),
        }
    }

    fn ready() -> Self {
        let h = Self::new();
        h.client.initialize().unwrap();
        h
    }

    /// Waits until the server has received a message matching `pred`.
    fn wait_seen(&self, pred: impl Fn(&Message) -> bool) -> Message {
        let deadline = Instant::now() + STEP;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let m = self
                .seen
                .recv_timeout(left)
                .expect("expected message never reached the server");
            if pred(&m) {
                return m;
            }
        }
    }

    fn wait_event(&self, pred: impl Fn(&LspEvent) -> bool) -> LspEvent {
        let events = self.client.events();
        let deadline = Instant::now() + STEP;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let e = events
                .recv_timeout(left)
                .expect("expected event never arrived");
            if pred(&e) {
                return e;
            }
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.client.shutdown();
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn response_to(m: &Message, want: &RequestId) -> bool {
    matches!(m, Message::Response { id: Some(id), .. } if id == want)
}

fn file_uri() -> Uri {
    "file:///workspace/proj/main.py".parse().unwrap()
}

// ---------------------------------------------------------------- tests

#[test]
fn client_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<LspClient>();
}

#[test]
fn initialize_handshake_and_server_requests() {
    let h = Harness::new();
    let result = h.client.initialize().unwrap();
    assert_eq!(result.server_info.map(|s| s.name).as_deref(), Some("fake"));
    assert!(h.client.is_ready());
    assert!(h
        .client
        .server_capabilities()
        .unwrap()
        .hover_provider
        .is_some());

    // The initialize request carried our root and capabilities.
    let init =
        h.wait_seen(|m| matches!(m, Message::Request { method, .. } if method == "initialize"));
    let Message::Request { params, .. } = init else {
        unreachable!()
    };
    assert_eq!(params["rootUri"], "file:///workspace/proj");
    assert_eq!(params["workspaceFolders"][0]["name"], "proj");
    assert_eq!(params["clientInfo"]["name"], "Apex IDE");
    assert_eq!(
        params["capabilities"]["textDocument"]["synchronization"]["didSave"],
        true
    );

    // workspace/configuration is answered with one null per item.
    let cfg = h.wait_seen(|m| response_to(m, &RequestId::String("cfg-1".into())));
    assert_eq!(
        cfg,
        Message::Response {
            id: Some(RequestId::String("cfg-1".into())),
            result: Ok(json!([null, null]))
        }
    );
    let progress = h.wait_seen(|m| response_to(m, &RequestId::Number(99)));
    assert!(matches!(
        progress,
        Message::Response {
            result: Ok(Value::Null),
            ..
        }
    ));
    let unknown = h.wait_seen(|m| response_to(m, &RequestId::Number(100)));
    assert!(
        matches!(unknown, Message::Response { result: Err(ref e), .. } if e.code == METHOD_NOT_FOUND)
    );

    // `initialized` follows the response.
    h.wait_seen(|m| matches!(m, Message::Notification { method, .. } if method == "initialized"));

    let log = h.wait_event(|e| matches!(e, LspEvent::Log(_)));
    let LspEvent::Log(log) = log else {
        unreachable!()
    };
    assert_eq!(log.typ, MessageType::INFO);
    assert_eq!(log.message, "fake server ready");

    assert!(matches!(
        h.client.initialize(),
        Err(LspError::AlreadyInitialized)
    ));
}

#[test]
fn document_methods_require_initialize() {
    let h = Harness::new();
    let uri = file_uri();
    assert!(matches!(
        h.client.did_open(&uri, "python", 1, ""),
        Err(LspError::NotInitialized)
    ));
    assert!(matches!(
        h.client.completion(&uri, Position::new(0, 0)),
        Err(LspError::NotInitialized)
    ));
}

#[test]
fn document_sync_and_diagnostics() {
    let h = Harness::ready();
    let uri = file_uri();
    h.client.did_open(&uri, "python", 1, "foo\n").unwrap();
    let event = h.wait_event(|e| matches!(e, LspEvent::Diagnostics(_)));
    let LspEvent::Diagnostics(d) = event else {
        unreachable!()
    };
    assert_eq!(d.uri, uri);
    assert_eq!(d.version, Some(1));
    assert_eq!(d.diagnostics[0].message, "undefined name");

    h.client.did_change(&uri, 2, "foo = 1\n").unwrap();
    let change = h.wait_seen(
        |m| matches!(m, Message::Notification { method, .. } if method == "textDocument/didChange"),
    );
    let Message::Notification { params, .. } = change else {
        unreachable!()
    };
    assert_eq!(params["textDocument"]["version"], 2);
    assert_eq!(params["contentChanges"], json!([{"text": "foo = 1\n"}]));

    h.client.did_save(&uri, Some("foo = 1\n")).unwrap();
    h.wait_seen(
        |m| matches!(m, Message::Notification { method, params } if method == "textDocument/didSave" && params["text"] == "foo = 1\n"),
    );
    h.client.did_close(&uri).unwrap();
    h.wait_seen(
        |m| matches!(m, Message::Notification { method, .. } if method == "textDocument/didClose"),
    );
}

#[test]
fn completion_hover_and_definition() {
    let h = Harness::ready();
    let uri = file_uri();

    let items = h.client.completion(&uri, Position::new(0, 1)).unwrap();
    let labels: Vec<_> = items.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["alpha", "beta"]);
    assert_eq!(items[0].kind, Some(lsp_types::CompletionItemKind::FUNCTION));
    assert_eq!(items[0].detail.as_deref(), Some("def alpha()"));

    let hover = h.client.hover(&uri, Position::new(0, 1)).unwrap().unwrap();
    assert_eq!(hover.contents, "**doc**");

    let defs = h.client.goto_definition(&uri, Position::new(0, 1)).unwrap();
    assert_eq!(
        defs,
        vec![Location::new(
            uri,
            Range::new(Position::new(0, 4), Position::new(0, 9))
        )]
    );
}

#[test]
fn responses_are_correlated_out_of_order() {
    let h = Harness::ready();
    thread::scope(|s| {
        let held = s.spawn(|| h.client.request::<Hold>(json!({"n": 1})));
        // Only send the echo once the server is holding the first request.
        h.wait_seen(|m| matches!(m, Message::Request { method, .. } if method == "test/hold"));
        let echoed = h.client.request::<Echo>(json!({"n": 2})).unwrap();
        assert_eq!(echoed, json!({"n": 2}));
        assert_eq!(held.join().unwrap().unwrap(), json!("held"));
    });
}

#[test]
fn timeout_cancels_the_request() {
    let h = Harness::ready();
    let err = h
        .client
        .request_with_timeout::<Slow>(Value::Null, Duration::from_millis(100))
        .unwrap_err();
    assert!(matches!(err, LspError::Timeout { ref method, .. } if method == "test/slow"));

    let slow =
        h.wait_seen(|m| matches!(m, Message::Request { method, .. } if method == "test/slow"));
    let Message::Request { id, .. } = slow else {
        unreachable!()
    };
    h.wait_seen(|m| {
        matches!(m, Message::Notification { method, params }
            if method == "$/cancelRequest" && params["id"] == json!(id))
    });
    // The client still works afterwards.
    assert_eq!(h.client.request::<Echo>(json!(3)).unwrap(), json!(3));
}

#[test]
fn server_errors_are_reported() {
    let h = Harness::ready();
    let err = h.client.request::<Fail>(Value::Null).unwrap_err();
    assert!(matches!(
        err,
        LspError::Server { code: -32000, ref message, .. } if message == "boom"
    ));
}

#[test]
fn server_exit_fails_pending_requests_promptly() {
    let h = Harness::ready();
    let started = Instant::now();
    let err = h
        .client
        .request_with_timeout::<Die>(Value::Null, Duration::from_secs(30))
        .unwrap_err();
    assert!(matches!(err, LspError::ConnectionClosed), "{err:?}");
    assert!(started.elapsed() < STEP);
    h.wait_event(|e| matches!(e, LspEvent::Disconnected));
    assert!(!h.client.is_connected());
    assert!(matches!(
        h.client.request::<Echo>(Value::Null),
        Err(LspError::ConnectionClosed)
    ));
}

#[test]
fn shutdown_is_graceful_and_idempotent() {
    let h = Harness::ready();
    h.client.shutdown().unwrap();
    h.wait_seen(|m| matches!(m, Message::Request { method, .. } if method == "shutdown"));
    h.wait_seen(|m| matches!(m, Message::Notification { method, .. } if method == "exit"));
    h.wait_event(|e| matches!(e, LspEvent::Disconnected));
    assert!(!h.client.is_ready());
    assert!(matches!(
        h.client.hover(&file_uri(), Position::new(0, 0)),
        Err(LspError::ShutDown)
    ));
    assert!(matches!(h.client.initialize(), Err(LspError::ShutDown)));
    h.client.shutdown().unwrap();
}

#[test]
fn unknown_notifications_pass_through() {
    let shared = notification_event("custom/thing".into(), json!({"a": 1}));
    assert!(
        matches!(shared, LspEvent::Notification { ref method, ref params } if method == "custom/thing" && params["a"] == 1)
    );
    // A known method with malformed params is passed through instead of dropped.
    let bad = notification_event("textDocument/publishDiagnostics".into(), json!({"x": 1}));
    assert!(matches!(bad, LspEvent::Notification { .. }));
}

// ------------------------------------------------------- real processes

#[test]
fn start_reports_missing_executable() {
    let spec = ServerSpec::new("apex-lsp-definitely-not-installed-xyz");
    assert!(matches!(
        LspClient::start(&spec),
        Err(LspError::Spawn { .. })
    ));
}

#[test]
fn start_detects_a_server_that_exits_immediately() {
    let spec = if cfg!(windows) {
        ServerSpec::new("cmd").with_args(["/C", "exit 0"])
    } else {
        ServerSpec::new("sh").with_args(["-c", "exit 0"])
    };
    let client = LspClient::start(&spec).unwrap();
    let err = client.initialize().unwrap_err();
    assert!(matches!(err, LspError::ConnectionClosed), "{err:?}");
    let events = client.events();
    let deadline = Instant::now() + STEP;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if matches!(events.recv_timeout(left), Ok(LspEvent::Disconnected)) {
            break;
        }
    }
    client.shutdown().unwrap();
}

#[test]
fn start_forwards_stderr_lines() {
    let spec = if cfg!(windows) {
        ServerSpec::new("cmd").with_args(["/C", "echo hello-stderr 1>&2"])
    } else {
        ServerSpec::new("sh").with_args(["-c", "echo hello-stderr 1>&2"])
    };
    let client = LspClient::start(&spec).unwrap();
    let events = client.events();
    let deadline = Instant::now() + STEP;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(LspEvent::Stderr(line)) => {
                assert_eq!(line.trim(), "hello-stderr");
                break;
            }
            Ok(_) => {}
            Err(e) => panic!("no stderr line: {e}"),
        }
    }
    client.shutdown().unwrap();
}
