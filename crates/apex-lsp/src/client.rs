//! The synchronous, thread-based LSP client.
//!
//! Each client owns three background threads:
//! - a **writer** that serialises every outgoing message onto the server's stdin,
//! - a **reader** that parses the server's stdout, completes pending requests,
//!   answers server-to-client requests and forwards notifications as [`LspEvent`]s,
//! - (for spawned servers) a **stderr** drain that forwards lines as [`LspEvent::Stderr`].
//!
//! Because writes go through a queue, the reader never blocks on the server's
//! stdin, which rules out the classic pipe deadlock where both sides wait on
//! each other's full buffers.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use lsp_types::notification::{self as notif, Notification};
use lsp_types::request::{self as req, Request};
use lsp_types::{
    ClientCapabilities, ClientInfo, CompletionClientCapabilities, CompletionContext,
    CompletionItemCapability, CompletionParams, CompletionResponse, CompletionTriggerKind,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, GeneralClientCapabilities, GotoCapability, GotoDefinitionParams,
    HoverClientCapabilities, HoverParams, InitializeParams, InitializeResult, InitializedParams,
    Location, MarkupKind, Position, PositionEncodingKind, PublishDiagnosticsClientCapabilities,
    ServerCapabilities, ShowMessageParams, ShowMessageRequestParams,
    TextDocumentClientCapabilities, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, TextDocumentSyncClientCapabilities, Uri,
    VersionedTextDocumentIdentifier, WindowClientCapabilities, WorkspaceClientCapabilities,
    WorkspaceFolder,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::LspError;
use crate::server::ServerSpec;
use crate::transport::{self, Message, RequestId, ResponseError, METHOD_NOT_FOUND};
use crate::types::{completions_from, locations_from, Completion, HoverInfo, LspEvent};
use crate::uri::uri_to_path;

/// Tunables for an [`LspClient`].
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Timeout for ordinary requests such as completion and hover.
    pub request_timeout: Duration,
    /// Timeout for `initialize` (servers may index the workspace first).
    pub initialize_timeout: Duration,
    /// How long to wait for `shutdown`, for the process to exit and for threads to stop.
    pub shutdown_timeout: Duration,
    /// Client name reported to the server.
    pub client_name: String,
    /// Client version reported to the server.
    pub client_version: Option<String>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(10),
            initialize_timeout: Duration::from_secs(60),
            shutdown_timeout: Duration::from_secs(3),
            client_name: "Apex IDE".into(),
            client_version: Some(env!("CARGO_PKG_VERSION").into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Uninitialized,
    Initializing,
    Ready,
    ShutDown,
}

enum Outgoing {
    Data(Vec<u8>),
    Close,
}

type Reply = Result<Value, ResponseError>;

struct Pending {
    map: HashMap<RequestId, Sender<Reply>>,
    closed: bool,
}

/// State shared between the client handle and its reader thread.
struct Shared {
    outgoing: Sender<Outgoing>,
    pending: Mutex<Pending>,
    events: Sender<LspEvent>,
    root_uri: Option<Uri>,
}

/// A connection to one language server.
///
/// All methods take `&self`, and the client is `Send + Sync`, so it can be
/// shared through an `Arc` and used from worker threads. Requests block the
/// calling thread until the response arrives or the timeout elapses, so call
/// them off the UI thread.
///
/// Lifecycle: [`LspClient::start`] (or [`LspClient::connect`]) →
/// [`LspClient::initialize`] → document methods → [`LspClient::shutdown`].
/// Dropping the client without `shutdown` kills the server process.
pub struct LspClient {
    shared: Arc<Shared>,
    events: Receiver<LspEvent>,
    next_id: AtomicI64,
    state: Mutex<State>,
    capabilities: Mutex<Option<ServerCapabilities>>,
    child: Mutex<Option<Child>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    options: ClientOptions,
}

impl std::fmt::Debug for LspClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspClient")
            .field("state", &*lock(&self.state))
            .field("root_uri", &self.shared.root_uri)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

impl LspClient {
    /// Spawns the server described by `spec` with default [`ClientOptions`].
    pub fn start(spec: &ServerSpec) -> Result<Self, LspError> {
        Self::start_with_options(spec, ClientOptions::default())
    }

    /// Spawns the server described by `spec`.
    ///
    /// The server is not initialized yet; call [`LspClient::initialize`] next.
    pub fn start_with_options(spec: &ServerSpec, options: ClientOptions) -> Result<Self, LspError> {
        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        if let Some(dir) = spec
            .root_uri
            .as_ref()
            .and_then(|u| uri_to_path(u).ok())
            .filter(|p| p.is_dir())
        {
            cmd.current_dir(dir);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let spawn_err = |source| LspError::Spawn {
            command: spec.command.clone(),
            source,
        };
        let mut child = cmd.spawn().map_err(spawn_err)?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            kill_process_tree(&mut child);
            return Err(spawn_err(std::io::Error::other("missing stdio pipe")));
        };

        let client = match Self::connect(stdout, stdin, spec.root_uri.clone(), options) {
            Ok(client) => client,
            Err(e) => {
                kill_process_tree(&mut child);
                return Err(e);
            }
        };
        *lock(&client.child) = Some(child);

        // From here on, an early return drops `client`, which kills the child.
        let events = client.shared.events.clone();
        let handle = spawn_thread("apex-lsp-stderr", move || stderr_loop(stderr, &events))?;
        lock(&client.threads).push(handle);
        tracing::info!(command = ?spec.command, "language server started");
        Ok(client)
    }

    /// Connects to a server over arbitrary streams (a socket, or pipes in tests).
    ///
    /// `reader` is the server's output and `writer` its input.
    pub fn connect<R, W>(
        reader: R,
        writer: W,
        root_uri: Option<Uri>,
        options: ClientOptions,
    ) -> Result<Self, LspError>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let (out_tx, out_rx) = crossbeam_channel::unbounded();
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared {
            outgoing: out_tx,
            pending: Mutex::new(Pending {
                map: HashMap::new(),
                closed: false,
            }),
            events: ev_tx,
            root_uri,
        });

        let writer_handle = spawn_thread("apex-lsp-writer", move || writer_loop(writer, &out_rx))?;
        let reader_shared = Arc::clone(&shared);
        let reader_handle = spawn_thread("apex-lsp-reader", move || {
            reader_loop(reader, &reader_shared)
        })?;

        Ok(Self {
            shared,
            events: ev_rx,
            next_id: AtomicI64::new(1),
            state: Mutex::new(State::Uninitialized),
            capabilities: Mutex::new(None),
            child: Mutex::new(None),
            threads: Mutex::new(vec![writer_handle, reader_handle]),
            options,
        })
    }

    /// A receiver for server-pushed [`LspEvent`]s (diagnostics, logs, ...).
    ///
    /// All clones share one queue, so each event is delivered to exactly one receiver.
    pub fn events(&self) -> Receiver<LspEvent> {
        self.events.clone()
    }

    /// The workspace root given at start-up.
    pub fn root_uri(&self) -> Option<&Uri> {
        self.shared.root_uri.as_ref()
    }

    /// The capabilities the server announced in its `initialize` result.
    pub fn server_capabilities(&self) -> Option<ServerCapabilities> {
        lock(&self.capabilities).clone()
    }

    /// Whether `initialize` has completed and `shutdown` has not been called.
    pub fn is_ready(&self) -> bool {
        *lock(&self.state) == State::Ready
    }

    /// Whether the server's output stream is still open.
    pub fn is_connected(&self) -> bool {
        !lock(&self.shared.pending).closed
    }

    /// Performs the `initialize` / `initialized` handshake.
    pub fn initialize(&self) -> Result<InitializeResult, LspError> {
        {
            let mut state = lock(&self.state);
            match *state {
                State::Uninitialized => *state = State::Initializing,
                State::Initializing | State::Ready => return Err(LspError::AlreadyInitialized),
                State::ShutDown => return Err(LspError::ShutDown),
            }
        }
        let outcome = self.handshake();
        let mut state = lock(&self.state);
        if *state == State::Initializing {
            *state = if outcome.is_ok() {
                State::Ready
            } else {
                State::Uninitialized
            };
        }
        outcome
    }

    fn handshake(&self) -> Result<InitializeResult, LspError> {
        let root = self.shared.root_uri.clone();
        // `root_uri` is deprecated in favour of `workspace_folders`, but pylsp
        // and other older servers still read it, so both are sent.
        #[allow(deprecated)]
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: root.clone(),
            workspace_folders: root.map(|uri| {
                vec![WorkspaceFolder {
                    name: folder_name(&uri),
                    uri,
                }]
            }),
            capabilities: client_capabilities(),
            client_info: Some(ClientInfo {
                name: self.options.client_name.clone(),
                version: self.options.client_version.clone(),
            }),
            ..Default::default()
        };
        let value = self.raw_request(
            req::Initialize::METHOD,
            serde_json::to_value(params)?,
            self.options.initialize_timeout,
        )?;
        let result: InitializeResult = serde_json::from_value(value)?;
        *lock(&self.capabilities) = Some(result.capabilities.clone());
        self.raw_notify(
            notif::Initialized::METHOD,
            serde_json::to_value(InitializedParams {})?,
        )?;
        Ok(result)
    }

    /// Sends any typed request with the default timeout.
    pub fn request<R: Request>(&self, params: R::Params) -> Result<R::Result, LspError> {
        self.request_with_timeout::<R>(params, self.options.request_timeout)
    }

    /// Sends any typed request with an explicit timeout.
    ///
    /// On timeout the request is cancelled with `$/cancelRequest`.
    pub fn request_with_timeout<R: Request>(
        &self,
        params: R::Params,
        timeout: Duration,
    ) -> Result<R::Result, LspError> {
        self.ensure_ready()?;
        let value = self.raw_request(R::METHOD, serde_json::to_value(params)?, timeout)?;
        Ok(serde_json::from_value(value)?)
    }

    /// Sends any typed notification.
    pub fn notify<N: Notification>(&self, params: N::Params) -> Result<(), LspError> {
        self.ensure_ready()?;
        self.raw_notify(N::METHOD, serde_json::to_value(params)?)
    }

    /// `textDocument/didOpen`. `language_id` is usually `"python"`.
    pub fn did_open(
        &self,
        uri: &Uri,
        language_id: &str,
        version: i32,
        text: &str,
    ) -> Result<(), LspError> {
        self.notify::<notif::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                uri.clone(),
                language_id.to_owned(),
                version,
                text.to_owned(),
            ),
        })
    }

    /// `textDocument/didChange` with the full new text (full document sync).
    ///
    /// `version` must increase with every change to the document.
    pub fn did_change(&self, uri: &Uri, version: i32, text: &str) -> Result<(), LspError> {
        self.notify::<notif::DidChangeTextDocument>(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier::new(uri.clone(), version),
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: text.to_owned(),
            }],
        })
    }

    /// `textDocument/didSave`, optionally including the saved text.
    pub fn did_save(&self, uri: &Uri, text: Option<&str>) -> Result<(), LspError> {
        self.notify::<notif::DidSaveTextDocument>(DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier::new(uri.clone()),
            text: text.map(str::to_owned),
        })
    }

    /// `textDocument/didClose`.
    pub fn did_close(&self, uri: &Uri) -> Result<(), LspError> {
        self.notify::<notif::DidCloseTextDocument>(DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier::new(uri.clone()),
        })
    }

    /// `textDocument/completion` at `position`, sorted by the server's sort keys.
    pub fn completion(&self, uri: &Uri, position: Position) -> Result<Vec<Completion>, LspError> {
        let response = self.request::<req::Completion>(CompletionParams {
            text_document_position: position_params(uri, position),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: Some(CompletionContext {
                trigger_kind: CompletionTriggerKind::INVOKED,
                trigger_character: None,
            }),
        })?;
        let items = match response {
            None => Vec::new(),
            Some(CompletionResponse::Array(items)) => items,
            Some(CompletionResponse::List(list)) => list.items,
        };
        Ok(completions_from(items))
    }

    /// `textDocument/hover` at `position`.
    pub fn hover(&self, uri: &Uri, position: Position) -> Result<Option<HoverInfo>, LspError> {
        let hover = self.request::<req::HoverRequest>(HoverParams {
            text_document_position_params: position_params(uri, position),
            work_done_progress_params: Default::default(),
        })?;
        Ok(hover.map(HoverInfo::from))
    }

    /// `textDocument/definition` at `position`; links are flattened to locations.
    pub fn goto_definition(
        &self,
        uri: &Uri,
        position: Position,
    ) -> Result<Vec<Location>, LspError> {
        let response = self.request::<req::GotoDefinition>(GotoDefinitionParams {
            text_document_position_params: position_params(uri, position),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })?;
        Ok(locations_from(response))
    }

    /// Gracefully stops the server: `shutdown`, `exit`, then waits for the
    /// process (killing it after [`ClientOptions::shutdown_timeout`]).
    ///
    /// Calling it again is a no-op.
    pub fn shutdown(&self) -> Result<(), LspError> {
        let previous = std::mem::replace(&mut *lock(&self.state), State::ShutDown);
        if previous == State::ShutDown {
            return Ok(());
        }
        let timeout = self.options.shutdown_timeout;
        let result = if previous == State::Ready {
            self.raw_request(req::Shutdown::METHOD, Value::Null, timeout)
                .map(drop)
        } else {
            Ok(())
        };
        // Best effort from here on: the server may already be gone.
        let _ = self.raw_notify(notif::Exit::METHOD, Value::Null);
        let _ = self.shared.outgoing.send(Outgoing::Close);
        self.reap_child(timeout);
        self.join_threads(timeout);
        match result {
            Err(LspError::ConnectionClosed) => Ok(()),
            other => other,
        }
    }

    fn ensure_ready(&self) -> Result<(), LspError> {
        match *lock(&self.state) {
            State::Ready => Ok(()),
            State::Uninitialized | State::Initializing => Err(LspError::NotInitialized),
            State::ShutDown => Err(LspError::ShutDown),
        }
    }

    fn raw_request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = crossbeam_channel::bounded(1);
        {
            let mut pending = lock(&self.shared.pending);
            if pending.closed {
                return Err(LspError::ConnectionClosed);
            }
            pending.map.insert(id.clone(), tx);
        }
        let message = Message::Request {
            id: id.clone(),
            method: method.to_owned(),
            params,
        };
        if let Err(e) = self.shared.send(&message) {
            lock(&self.shared.pending).map.remove(&id);
            return Err(e);
        }
        tracing::trace!(%id, method, "request sent");

        match rx.recv_timeout(timeout) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(LspError::Server {
                method: method.to_owned(),
                code: error.code,
                message: error.message,
            }),
            Err(RecvTimeoutError::Timeout) => {
                lock(&self.shared.pending).map.remove(&id);
                let _ = self.raw_notify(notif::Cancel::METHOD, json!({ "id": id }));
                Err(LspError::Timeout {
                    method: method.to_owned(),
                    timeout,
                })
            }
            Err(RecvTimeoutError::Disconnected) => Err(LspError::ConnectionClosed),
        }
    }

    fn raw_notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        self.shared.send(&Message::Notification {
            method: method.to_owned(),
            params,
        })
    }

    fn reap_child(&self, timeout: Duration) {
        let Some(mut child) = lock(&self.child).take() else {
            return;
        };
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    tracing::info!(%status, "language server exited");
                    return;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => break,
            }
        }
        tracing::warn!("language server did not exit in time; killing it");
        kill_process_tree(&mut child);
    }

    /// Joins the background threads, detaching any that are still blocked after `timeout`.
    fn join_threads(&self, timeout: Duration) {
        let handles = std::mem::take(&mut *lock(&self.threads));
        let deadline = Instant::now() + timeout;
        while handles.iter().any(|h| !h.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        for handle in handles {
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.shared.outgoing.send(Outgoing::Close);
        if let Some(mut child) = lock(&self.child).take() {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                kill_process_tree(&mut child);
            }
        }
    }
}

impl Shared {
    fn send(&self, message: &Message) -> Result<(), LspError> {
        self.outgoing
            .send(Outgoing::Data(message.to_bytes()))
            .map_err(|_| LspError::ConnectionClosed)
    }

    fn emit(&self, event: LspEvent) {
        // The receiver lives as long as the client; a send error only means
        // the client is being dropped.
        let _ = self.events.send(event);
    }

    fn dispatch(&self, message: Message) {
        match message {
            Message::Response {
                id: Some(id),
                result,
            } => {
                let waiter = lock(&self.pending).map.remove(&id);
                match waiter {
                    Some(tx) => {
                        let _ = tx.send(result);
                    }
                    None => tracing::debug!(%id, "response for unknown or timed-out request"),
                }
            }
            Message::Response { id: None, result } => {
                tracing::warn!(?result, "server sent a response without an id");
            }
            Message::Request { id, method, params } => {
                let result = self.answer_server_request(&method, &params);
                if let Err(e) = self.send(&Message::Response {
                    id: Some(id),
                    result,
                }) {
                    tracing::debug!(%e, method, "could not answer server request");
                }
            }
            Message::Notification { method, params } => {
                self.emit(notification_event(method, params))
            }
        }
    }

    fn answer_server_request(&self, method: &str, params: &Value) -> Result<Value, ResponseError> {
        match method {
            // One `null` per requested section: "use your defaults".
            req::WorkspaceConfiguration::METHOD => {
                let count = params
                    .get("items")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                Ok(Value::Array(vec![Value::Null; count]))
            }
            req::WorkDoneProgressCreate::METHOD
            | req::RegisterCapability::METHOD
            | req::UnregisterCapability::METHOD => Ok(Value::Null),
            req::ShowMessageRequest::METHOD => {
                if let Ok(p) = ShowMessageRequestParams::deserialize(params) {
                    self.emit(LspEvent::ShowMessage(ShowMessageParams {
                        typ: p.typ,
                        message: p.message,
                    }));
                }
                Ok(Value::Null)
            }
            req::WorkspaceFoldersRequest::METHOD => Ok(match &self.root_uri {
                Some(uri) => json!([{ "uri": uri.as_str(), "name": folder_name(uri) }]),
                None => Value::Null,
            }),
            _ => Err(ResponseError {
                code: METHOD_NOT_FOUND,
                message: format!("unsupported method: {method}"),
                data: None,
            }),
        }
    }

    /// Marks the connection closed and fails every pending request.
    fn close(&self) {
        {
            let mut pending = lock(&self.pending);
            pending.closed = true;
            pending.map.clear();
        }
        let _ = self.outgoing.send(Outgoing::Close);
        self.emit(LspEvent::Disconnected);
    }
}

fn notification_event(method: String, params: Value) -> LspEvent {
    fn parse<T: DeserializeOwned>(params: &Value) -> Option<T> {
        T::deserialize(params).ok()
    }
    let event = match method.as_str() {
        notif::PublishDiagnostics::METHOD => parse(&params).map(LspEvent::Diagnostics),
        notif::LogMessage::METHOD => parse(&params).map(LspEvent::Log),
        notif::ShowMessage::METHOD => parse(&params).map(LspEvent::ShowMessage),
        notif::Progress::METHOD => parse(&params).map(LspEvent::Progress),
        _ => None,
    };
    event.unwrap_or(LspEvent::Notification { method, params })
}

/// The capabilities this client announces in `initialize`.
pub fn client_capabilities() -> ClientCapabilities {
    let formats = vec![MarkupKind::Markdown, MarkupKind::PlainText];
    ClientCapabilities {
        workspace: Some(WorkspaceClientCapabilities {
            configuration: Some(true),
            workspace_folders: Some(true),
            ..Default::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            synchronization: Some(TextDocumentSyncClientCapabilities {
                did_save: Some(true),
                ..Default::default()
            }),
            completion: Some(CompletionClientCapabilities {
                completion_item: Some(CompletionItemCapability {
                    snippet_support: Some(false),
                    documentation_format: Some(formats.clone()),
                    ..Default::default()
                }),
                context_support: Some(true),
                ..Default::default()
            }),
            hover: Some(HoverClientCapabilities {
                dynamic_registration: None,
                content_format: Some(formats),
            }),
            definition: Some(GotoCapability {
                dynamic_registration: None,
                link_support: Some(true),
            }),
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                related_information: Some(true),
                version_support: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }),
        window: Some(WindowClientCapabilities {
            work_done_progress: Some(true),
            ..Default::default()
        }),
        general: Some(GeneralClientCapabilities {
            position_encodings: Some(vec![PositionEncodingKind::UTF16]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn position_params(uri: &Uri, position: Position) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier::new(uri.clone()),
        position,
    }
}

/// The last path component of `uri`, used as the workspace folder name.
fn folder_name(uri: &Uri) -> String {
    uri_to_path(uri)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .or_else(|| {
            // Not a local path on this OS (e.g. a POSIX URI on Windows): use the raw segment.
            url::Url::parse(uri.as_str())
                .ok()?
                .path_segments()?
                .rfind(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "workspace".to_owned())
}

fn spawn_thread(name: &str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>, LspError> {
    Ok(std::thread::Builder::new().name(name.to_owned()).spawn(f)?)
}

fn writer_loop<W: Write>(mut writer: W, outgoing: &Receiver<Outgoing>) {
    for item in outgoing {
        match item {
            Outgoing::Data(bytes) => {
                if let Err(e) = transport::write_message(&mut writer, &bytes) {
                    tracing::debug!(%e, "language server stdin closed");
                    break;
                }
            }
            Outgoing::Close => break,
        }
    }
    // Dropping `writer` closes the server's stdin.
}

fn reader_loop<R: Read>(reader: R, shared: &Shared) {
    let mut reader = BufReader::new(reader);
    loop {
        match transport::read_message(&mut reader) {
            Ok(Some(body)) => match Message::parse(&body) {
                Ok(message) => shared.dispatch(message),
                Err(e) => tracing::warn!(%e, "ignoring malformed message from language server"),
            },
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(%e, "language server stream broken");
                break;
            }
        }
    }
    shared.close();
}

fn stderr_loop<R: Read>(stderr: R, events: &Sender<LspEvent>) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&line).trim_end().to_owned();
                tracing::debug!(target: "apex_lsp::stderr", "{text}");
                // Keep draining even if nobody listens, so the server never blocks.
                let _ = events.send(LspEvent::Stderr(text));
            }
        }
    }
}

/// Kills the process and, on Windows, its children (`.cmd` shims such as
/// `pyright-langserver.cmd` run the real server as a child of `cmd.exe`).
fn kill_process_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests;
