//! Error types for the LSP client.

use std::path::PathBuf;
use std::time::Duration;

/// Errors produced while framing or parsing JSON-RPC messages.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The underlying stream failed.
    #[error("I/O error on LSP stream: {0}")]
    Io(#[from] std::io::Error),
    /// The stream ended in the middle of a message.
    #[error("stream ended in the middle of a message")]
    UnexpectedEof,
    /// A header line was not of the form `Name: value`.
    #[error("malformed header line: {0:?}")]
    InvalidHeader(String),
    /// The header block had no `Content-Length` header.
    #[error("message header has no Content-Length")]
    MissingContentLength,
    /// The `Content-Length` value was not a non-negative integer.
    #[error("invalid Content-Length value: {0:?}")]
    InvalidContentLength(String),
    /// The announced body is larger than [`crate::transport::MAX_MESSAGE_SIZE`].
    #[error("message of {0} bytes exceeds the size limit")]
    MessageTooLarge(usize),
    /// The body was not valid JSON.
    #[error("message body is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The body was JSON but not a JSON-RPC request, response or notification.
    #[error("invalid JSON-RPC message: {0}")]
    InvalidMessage(String),
}

/// Errors returned by [`crate::LspClient`] and the helper functions of this crate.
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    /// The language server process could not be started.
    #[error("failed to start language server {command:?}: {source}")]
    Spawn {
        /// The command that was executed.
        command: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// A local I/O operation (such as spawning a thread) failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Framing or JSON-RPC parsing failed.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// Parameters or results could not be (de)serialized.
    #[error("failed to (de)serialize LSP payload: {0}")]
    Json(#[from] serde_json::Error),
    /// The server did not answer within the allowed time.
    #[error("request {method} timed out after {timeout:?}")]
    Timeout {
        /// The LSP method that timed out.
        method: String,
        /// The timeout that elapsed.
        timeout: Duration,
    },
    /// The server answered a request with a JSON-RPC error.
    #[error("server returned error {code} for {method}: {message}")]
    Server {
        /// The LSP method of the failed request.
        method: String,
        /// The JSON-RPC error code.
        code: i64,
        /// The error message sent by the server.
        message: String,
    },
    /// The connection to the server is closed (the server exited or its output ended).
    #[error("connection to the language server is closed")]
    ConnectionClosed,
    /// A document method was called before [`crate::LspClient::initialize`] succeeded.
    #[error("the language server has not been initialized")]
    NotInitialized,
    /// [`crate::LspClient::initialize`] was called more than once.
    #[error("the language server is already initialized")]
    AlreadyInitialized,
    /// The client has been shut down.
    #[error("the language server client has been shut down")]
    ShutDown,
    /// A filesystem path could not be turned into a `file://` URI.
    #[error("cannot convert path {0:?} to a file URI")]
    InvalidPath(PathBuf),
    /// A URI could not be parsed or is not a local `file://` URI.
    #[error("invalid or non-file URI: {0}")]
    InvalidUri(String),
}
