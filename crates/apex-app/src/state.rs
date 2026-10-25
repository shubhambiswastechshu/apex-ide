//! UI-thread application state. Everything here lives on the Slint thread
//! inside `App::state`; workers only ever see clones of the data they need.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use apex_core::{FileTree, Project, Settings, TreeRow};
use apex_editor::{Buffer, Language};
use apex_plugins::{PluginId, PluginManager};
use apex_python::Interpreter;
use apex_themes::ThemeRegistry;

use crate::db_panel::DbWorker;
use crate::lsp::{Diagnostics, LspState, ProblemRow};
use crate::project_flow::Step;
use crate::run::RunSession;

/// The whole application state.
pub struct State {
    /// User settings, saved when they change.
    pub settings: Settings,
    /// Where settings are saved; `None` if the config folder is unknown.
    pub settings_path: Option<PathBuf>,
    /// Built-in and user themes.
    pub themes: ThemeRegistry,
    /// Theme ids in picker order.
    pub theme_ids: Vec<String>,
    /// The New Project screen.
    pub new_project: NewProjectState,
    /// The open project, if any.
    pub workspace: Option<Workspace>,
    /// Bumped whenever a project opens or closes, so late results from
    /// workers for an old project are ignored.
    pub generation: u64,
    /// Next run session id.
    pub next_run_id: u64,
    /// Plugin manager (`None` if it failed to start).
    pub plugins: Option<PluginManager>,
    /// Plugin ids in panel order.
    pub plugin_ids: Vec<PluginId>,
    /// Command ids in panel order.
    pub command_ids: Vec<String>,
    /// Database worker (`None` if its thread could not start).
    pub db: Option<DbWorker>,
    /// Table names in panel order.
    pub db_tables: Vec<String>,
    /// Last known "folder is gone" flag per recent project, filled in by a
    /// worker (a stat on an offline network drive can block for a long time).
    pub recent_missing: HashMap<PathBuf, bool>,
    /// The code editor (panes, completion, clipboard).
    pub editor: crate::editor::EditorUi,
    /// Files listed by the open Ctrl+Tab switcher, in row order.
    pub switcher: Vec<PathBuf>,
}

/// Interpreter discovery progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Discovery {
    /// Not started yet.
    #[default]
    NotStarted,
    /// Running on a worker thread.
    Running,
    /// Finished.
    Done,
}

/// State of the New Project screen.
#[derive(Default)]
pub struct NewProjectState {
    /// Discovered interpreters, in drop-down order.
    pub interpreters: Vec<Interpreter>,
    /// Discovery progress.
    pub discovery: Discovery,
    /// A project is being created.
    pub creating: bool,
    /// Cancels the running creation (set while `creating`).
    pub cancel: Option<apex_python::CancelToken>,
    /// Per-step state shown in the progress list (see `StepRow.state`).
    pub steps: Vec<(Step, i32, String)>,
}

/// An open editor tab.
pub struct Tab {
    /// The file.
    pub path: PathBuf,
    /// Its text and history.
    pub buffer: Buffer,
    /// Editor view state: caret, scroll, highlight cache, folds.
    pub view: crate::editor::TabView,
    /// Version last sent to the language server.
    pub lsp_version: i32,
    /// Whether the language server knows this file is open.
    pub lsp_open: bool,
}

impl Tab {
    /// A tab for a freshly opened buffer.
    pub fn new(path: PathBuf, buffer: Buffer) -> Self {
        Self {
            path,
            buffer,
            view: crate::editor::TabView::default(),
            lsp_version: 0,
            lsp_open: false,
        }
    }

    /// Whether the file goes to the Python language server.
    pub fn is_python(&self) -> bool {
        self.buffer.language() == Language::Python
    }

    /// The tab caption.
    pub fn title(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

/// An open project.
pub struct Workspace {
    /// The project and its configuration.
    pub project: Project,
    /// Latest tree snapshot.
    pub tree: Option<FileTree>,
    /// Expanded folders.
    pub expanded: HashSet<PathBuf>,
    /// Rows currently shown, parallel to the UI tree model.
    pub rows: Vec<TreeRow>,
    /// Selected tree path.
    pub selected: Option<PathBuf>,
    /// Explorer search text; non-empty shows only matching entries.
    pub tree_filter: String,
    /// Open tabs.
    pub tabs: Vec<Tab>,
    /// Index of the active tab.
    pub active: Option<usize>,
    /// Tab waiting for a save/discard answer.
    pub pending_close: Option<usize>,
    /// Tab whose file changed on disk, waiting for overwrite/reload/cancel.
    pub pending_conflict: Option<usize>,
    /// The running program.
    pub run: Option<RunSession>,
    /// Selected entry of the run drop-down.
    pub run_choice: usize,
    /// Language server state.
    pub lsp: LspState,
    /// Latest diagnostics.
    pub diagnostics: Diagnostics,
    /// Problems rows, parallel to the UI model.
    pub problems: Vec<ProblemRow>,
    /// Missing-package detection.
    pub packages: crate::editor::packages::PackageState,
    /// Interpreters offered by the toolbar drop-down (`None`: not scanned).
    pub interpreters: Option<Vec<apex_python::Interpreter>>,
    /// Files in the order they were last active, most recent first (for
    /// the Ctrl+Tab switcher).
    pub mru: Vec<PathBuf>,
}

impl Workspace {
    /// A freshly opened project.
    pub fn new(project: Project) -> Self {
        Self {
            project,
            tree: None,
            expanded: HashSet::new(),
            rows: Vec::new(),
            selected: None,
            tree_filter: String::new(),
            tabs: Vec::new(),
            active: None,
            pending_close: None,
            pending_conflict: None,
            run: None,
            run_choice: 0,
            lsp: LspState::Starting,
            diagnostics: Diagnostics::default(),
            problems: Vec::new(),
            packages: Default::default(),
            interpreters: None,
            mru: Vec::new(),
        }
    }

    /// The active tab.
    pub fn active_tab(&self) -> Option<&Tab> {
        self.active.and_then(|i| self.tabs.get(i))
    }

    /// The active tab, mutably.
    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.active.and_then(|i| self.tabs.get_mut(i))
    }

    /// Index of the tab showing `path`.
    pub fn tab_index(&self, path: &std::path::Path) -> Option<usize> {
        self.tabs.iter().position(|t| t.path == path)
    }

    /// Whether any tab has unsaved changes.
    pub fn has_unsaved(&self) -> bool {
        self.tabs.iter().any(|t| t.buffer.is_dirty())
    }
}
