//! Language server integration: finding and starting a Python server for
//! the open project, and turning its diagnostics into Problems rows.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use apex_lsp::lsp_types::{DiagnosticSeverity, PublishDiagnosticsParams};
use apex_lsp::{
    find_python_servers_in, path_to_uri, uri_to_path, venv_bin_dir, LspClient, LspError, LspEvent,
    ServerSpec,
};

use crate::events::{AppEvent, EventSink};

/// The command that installs the recommended server.
pub const INSTALL_HINT: &str = "uv tool install basedpyright";

/// Why no language server is running.
#[derive(Debug, thiserror::Error)]
pub enum LspStartError {
    /// None of the supported servers is installed.
    #[error("no Python language server found")]
    NotFound,
    /// A server was found but failed to start or initialize.
    #[error("{server} failed to start: {source}")]
    Failed {
        /// Display name of the server.
        server: String,
        /// The underlying error.
        #[source]
        source: LspError,
    },
}

/// Language server state of the open project.
pub enum LspState {
    /// Being looked for or started on a worker thread.
    Starting,
    /// Running and initialized.
    Ready {
        /// The client; requests block, so call them off the UI thread.
        client: Arc<LspClient>,
        /// Server display name.
        name: String,
    },
    /// Not installed.
    Missing,
    /// Found but broken, or exited.
    Failed(String),
}

impl LspState {
    /// Short text for the status bar.
    pub fn status_text(&self) -> String {
        match self {
            LspState::Starting => "LSP: starting…".to_owned(),
            LspState::Ready { name, .. } => format!("LSP: {name}"),
            LspState::Missing => "LSP: not installed".to_owned(),
            LspState::Failed(_) => "LSP: failed".to_owned(),
        }
    }

    /// The client, if ready.
    pub fn client(&self) -> Option<&Arc<LspClient>> {
        match self {
            LspState::Ready { client, .. } => Some(client),
            _ => None,
        }
    }
}

/// Picks the server to use: the pyright family first (completion and
/// diagnostics), `ruff server` only as a last resort.
pub fn pick_server(mut servers: Vec<ServerSpec>) -> Option<ServerSpec> {
    let preferred = servers
        .iter()
        .position(|s| !s.display_name().to_ascii_lowercase().contains("ruff"));
    match preferred {
        Some(i) => Some(servers.swap_remove(i)),
        None => servers.into_iter().next(),
    }
}

/// `PATH` with `dir` in front, so a venv's tools win.
fn path_with(dir: &Path) -> Option<OsString> {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&current));
    std::env::join_paths(dirs).ok()
}

/// Starts a server for `root` on a worker thread. Sends
/// [`AppEvent::LspStarted`] once, then every server notification as
/// [`AppEvent::Lsp`], all tagged with `generation`.
pub fn start(root: PathBuf, venv: Option<PathBuf>, generation: u64, sink: EventSink) {
    let thread_sink = sink.clone();
    let spawned = std::thread::Builder::new()
        .name("lsp-start".into())
        .spawn(move || run_server(root, venv, generation, thread_sink));
    if let Err(err) = spawned {
        sink.send(AppEvent::LspStarted {
            generation,
            result: Err(LspStartError::Failed {
                server: "language server".into(),
                source: LspError::Io(err),
            }),
        });
    }
}

fn run_server(root: PathBuf, venv: Option<PathBuf>, generation: u64, sink: EventSink) {
    let dirs: Vec<PathBuf> = venv.iter().map(|v| venv_bin_dir(v)).collect();
    let Some(mut spec) = pick_server(find_python_servers_in(&dirs)) else {
        sink.send(AppEvent::LspStarted {
            generation,
            result: Err(LspStartError::NotFound),
        });
        return;
    };
    let name = spec.display_name();
    let started = (|| -> Result<LspClient, LspError> {
        spec = spec.with_root_uri(path_to_uri(&root)?);
        if let Some(venv) = &venv {
            spec = spec.with_env("VIRTUAL_ENV", venv.as_os_str());
            if let Some(path) = path_with(&venv_bin_dir(venv)) {
                spec = spec.with_env("PATH", path);
            }
        }
        let client = LspClient::start(&spec)?;
        client.initialize()?;
        Ok(client)
    })();
    match started {
        Ok(client) => {
            let client = Arc::new(client);
            let events = client.events();
            sink.send(AppEvent::LspStarted {
                generation,
                result: Ok((client, name)),
            });
            for event in events.iter() {
                let done = matches!(event, LspEvent::Disconnected);
                sink.send(AppEvent::Lsp { generation, event });
                if done {
                    break;
                }
            }
        }
        Err(source) => sink.send(AppEvent::LspStarted {
            generation,
            result: Err(LspStartError::Failed {
                server: name,
                source,
            }),
        }),
    }
}

/// Stops a server without blocking the UI thread.
pub fn shutdown_in_background(client: Arc<LspClient>) {
    let _ = std::thread::Builder::new()
        .name("lsp-shutdown".into())
        .spawn(move || {
            if let Err(err) = client.shutdown() {
                tracing::debug!(%err, "language server shutdown");
            }
        });
}

/// One row of the Problems panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProblemRow {
    /// 1 error, 2 warning, 3 information, 4 hint.
    pub severity: i32,
    /// First line of the message.
    pub message: String,
    /// `file.py:12:5`, relative to the project root.
    pub location: String,
    /// Which tool reported it.
    pub source: String,
    /// The file.
    pub path: PathBuf,
    /// 0-based line.
    pub line: usize,
    /// 0-based column (UTF-16 units, as LSP reports it).
    pub col: usize,
}

/// The latest diagnostics, per file.
#[derive(Debug, Default)]
pub struct Diagnostics {
    by_file: BTreeMap<PathBuf, PublishDiagnosticsParams>,
}

impl Diagnostics {
    /// Replaces the diagnostics of one file. Returns `false` (and ignores
    /// the update) for URIs that are not local files.
    pub fn update(&mut self, params: PublishDiagnosticsParams) -> bool {
        let Ok(path) = uri_to_path(&params.uri) else {
            return false;
        };
        if params.diagnostics.is_empty() {
            self.by_file.remove(&path);
        } else {
            self.by_file.insert(path, params);
        }
        true
    }

    /// Rows sorted by severity, then file, then position.
    pub fn rows(&self, root: &Path) -> Vec<ProblemRow> {
        let mut rows: Vec<ProblemRow> = self
            .by_file
            .iter()
            .flat_map(|(path, params)| {
                params.diagnostics.iter().map(move |d| {
                    let line = d.range.start.line as usize;
                    let col = d.range.start.character as usize;
                    let rel = path.strip_prefix(root).unwrap_or(path);
                    ProblemRow {
                        severity: severity_rank(d.severity),
                        message: d.message.lines().next().unwrap_or_default().to_owned(),
                        location: format!("{}:{}:{}", rel.display(), line + 1, col + 1),
                        source: d.source.clone().unwrap_or_default(),
                        path: path.clone(),
                        line,
                        col,
                    }
                })
            })
            .collect();
        rows.sort_by(|a, b| {
            (a.severity, &a.path, a.line, a.col).cmp(&(b.severity, &b.path, b.line, b.col))
        });
        rows
    }
}

/// One diagnostic range of a file, for squiggles (UTF-16 positions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Squiggle {
    /// Start line.
    pub line: usize,
    /// Start column (UTF-16 units).
    pub col: usize,
    /// End line.
    pub end_line: usize,
    /// End column (UTF-16 units).
    pub end_col: usize,
    /// Error (otherwise warning). Hints and information are not shown.
    pub error: bool,
}

impl Diagnostics {
    /// Diagnostics of `path` that should be underlined.
    pub fn squiggles_for(&self, path: &Path) -> Vec<Squiggle> {
        let Some(params) = self.by_file.get(path) else {
            return Vec::new();
        };
        params
            .diagnostics
            .iter()
            .filter_map(|d| {
                let rank = severity_rank(d.severity);
                (rank <= 2).then_some(Squiggle {
                    line: d.range.start.line as usize,
                    col: d.range.start.character as usize,
                    end_line: d.range.end.line as usize,
                    end_col: d.range.end.character as usize,
                    error: rank == 1,
                })
            })
            .collect()
    }

    /// Modules `path` imports that the server could not resolve
    /// (`reportMissingImports` / `reportMissingModuleSource`).
    pub fn missing_imports(&self, path: &Path) -> std::collections::BTreeSet<String> {
        let Some(params) = self.by_file.get(path) else {
            return Default::default();
        };
        params
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(&d.code, Some(apex_lsp::lsp_types::NumberOrString::String(c))
                    if c == "reportMissingImports" || c == "reportMissingModuleSource")
            })
            .filter_map(|d| crate::editor::packages::module_from_diagnostic(&d.message))
            .collect()
    }

    /// The local files that have diagnostics.
    pub fn files(&self) -> impl Iterator<Item = &PathBuf> {
        self.by_file.keys()
    }
}

/// Squiggles for `path` (none without a path).
pub fn squiggles(diagnostics: &Diagnostics, path: Option<&Path>) -> Vec<Squiggle> {
    path.map(|p| diagnostics.squiggles_for(p))
        .unwrap_or_default()
}

/// Work for the language server that must not block the UI thread and
/// must reach the server in order (a change before the completion request
/// that depends on it).
pub enum LspJob {
    /// `textDocument/didChange` with the full text.
    Change {
        /// The document.
        uri: apex_lsp::lsp_types::Uri,
        /// Its new version.
        version: i32,
        /// Its text.
        text: String,
    },
    /// Runs on the worker after earlier jobs.
    Run(Box<dyn FnOnce(&LspClient) + Send>),
}

/// A thread that talks to one language server in order.
pub struct LspWorker {
    client: Arc<LspClient>,
    tx: crossbeam_channel::Sender<LspJob>,
}

impl LspWorker {
    /// Starts a worker for `client`.
    pub fn spawn(client: Arc<LspClient>) -> Option<Self> {
        let (tx, rx) = crossbeam_channel::unbounded::<LspJob>();
        let c = client.clone();
        std::thread::Builder::new()
            .name("lsp-worker".into())
            .spawn(move || {
                for job in rx {
                    match job {
                        LspJob::Change { uri, version, text } => {
                            if let Err(e) = c.did_change(&uri, version, &text) {
                                tracing::debug!(%e, "didChange");
                            }
                        }
                        LspJob::Run(f) => f(&c),
                    }
                }
            })
            .ok()?;
        Some(Self { client, tx })
    }

    /// Whether this worker serves `client`.
    pub fn serves(&self, client: &Arc<LspClient>) -> bool {
        Arc::ptr_eq(&self.client, client)
    }

    /// Queues a job.
    pub fn send(&self, job: LspJob) {
        let _ = self.tx.send(job);
    }
}

/// Queues the full text of `tab` for the server if it changed since the
/// last sync (the text is copied here; serialising and writing happen on
/// the worker).
pub fn queue_change(worker: &LspWorker, tab: &mut crate::state::Tab) {
    if !tab.lsp_open || !tab.view.lsp_dirty {
        return;
    }
    let Ok(uri) = path_to_uri(&tab.path) else {
        return;
    };
    tab.view.lsp_dirty = false;
    tab.lsp_version += 1;
    worker.send(LspJob::Change {
        uri,
        version: tab.lsp_version,
        text: tab.buffer.text(),
    });
}

/// Tells the server a Python tab is open.
pub fn send_open(client: &LspClient, tab: &mut crate::state::Tab) {
    if !tab.is_python() || tab.lsp_open {
        return;
    }
    let Ok(uri) = path_to_uri(&tab.path) else {
        return;
    };
    tab.lsp_version += 1;
    match client.did_open(&uri, "python", tab.lsp_version, &tab.buffer.text()) {
        Ok(()) => tab.lsp_open = true,
        Err(e) => tracing::debug!(%e, "didOpen"),
    }
}

/// Sends the full new text of a tab.
pub fn send_change(client: &LspClient, tab: &mut crate::state::Tab) {
    if !tab.lsp_open {
        return;
    }
    let Ok(uri) = path_to_uri(&tab.path) else {
        return;
    };
    tab.lsp_version += 1;
    if let Err(e) = client.did_change(&uri, tab.lsp_version, &tab.buffer.text()) {
        tracing::debug!(%e, "didChange");
    }
}

fn severity_rank(severity: Option<DiagnosticSeverity>) -> i32 {
    match severity {
        Some(DiagnosticSeverity::ERROR) => 1,
        Some(DiagnosticSeverity::WARNING) => 2,
        Some(DiagnosticSeverity::INFORMATION) => 3,
        Some(DiagnosticSeverity::HINT) => 4,
        // Servers that omit it usually mean an error.
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_lsp::lsp_types::{Diagnostic, Position, Range};

    fn diag(line: u32, severity: DiagnosticSeverity, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, 4), Position::new(line, 8)),
            severity: Some(severity),
            message: message.to_owned(),
            source: Some("basedpyright".into()),
            ..Diagnostic::default()
        }
    }

    #[test]
    fn rows_are_sorted_and_relative() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.py");
        let uri = path_to_uri(&file).unwrap();
        let mut d = Diagnostics::default();
        assert!(d.update(PublishDiagnosticsParams::new(
            uri.clone(),
            vec![
                diag(9, DiagnosticSeverity::WARNING, "unused\nmore detail"),
                diag(2, DiagnosticSeverity::ERROR, "undefined name"),
            ],
            None,
        )));
        let rows = d.rows(dir.path());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].severity, 1);
        assert_eq!(rows[0].location, "main.py:3:5");
        assert_eq!(rows[1].message, "unused");
        assert_eq!(rows[1].source, "basedpyright");

        let sq = d.squiggles_for(&file);
        assert_eq!(sq.len(), 2);
        assert!(sq.iter().any(|s| s.error && s.line == 2));
        assert!(d.missing_imports(&file).is_empty());

        // An empty publish clears the file.
        d.update(PublishDiagnosticsParams::new(uri, Vec::new(), None));
        assert!(d.rows(dir.path()).is_empty());
    }

    #[test]
    fn pick_prefers_pyright_family_over_ruff() {
        let ruff = ServerSpec::new("ruff").with_args(["server"]);
        let pyright = ServerSpec::new("basedpyright-langserver").with_args(["--stdio"]);
        let picked = pick_server(vec![ruff.clone(), pyright]).unwrap();
        assert!(picked.display_name().contains("basedpyright"));
        let picked = pick_server(vec![ruff]).unwrap();
        assert!(picked.display_name().contains("ruff"));
        assert!(pick_server(Vec::new()).is_none());
    }

    #[test]
    fn status_texts() {
        assert_eq!(LspState::Missing.status_text(), "LSP: not installed");
        assert!(LspState::Starting.client().is_none());
    }
}
