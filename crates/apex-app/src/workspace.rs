//! The main IDE screen: project tree, editor tabs, Run and Problems tool
//! windows, and the language server session of the open project.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use apex_core::{FileTree, Project};
use apex_editor::{Buffer, EditorError};
use apex_lsp::{path_to_uri, LspEvent};
use apex_python::RunEvent;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::app::{append_capped, to_i32, App};
use crate::events::{AppEvent, NoticeKind};
use crate::lsp::{self, LspStartError, LspState};
use crate::run::{self, RunPlanError};
use crate::state::{Tab, Workspace};
use crate::welcome::{capitalize, SCREEN_WELCOME, SCREEN_WORKSPACE};
use crate::{AppApi, AppWindow, EditorTab, OutputLine, Problem, ToolsApi, TreeItem, WorkspaceApi};

/// Files larger than this are not opened in the editor.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Most lines kept in the Run tool window.
const OUTPUT_CAP: usize = 10_000;
/// `OutputLine.kind` values.
const OUT_STDOUT: i32 = 0;
const OUT_STDERR: i32 = 1;
const OUT_IDE: i32 = 2;

/// The folder of the virtual environment an interpreter lives in, if it is
/// one (`<venv>/Scripts/python.exe` or `<venv>/bin/python`).
pub fn venv_root_of(interpreter: &Path) -> Option<PathBuf> {
    let venv = interpreter.parent()?.parent()?;
    venv.join("pyvenv.cfg")
        .is_file()
        .then(|| venv.to_path_buf())
}

/// The toolbar's interpreter text: `Python: .venv\Scripts\python.exe`.
pub fn interpreter_label(project: &Project) -> String {
    match run::project_interpreter(project) {
        Ok(path) => format!(
            "Python: {}",
            path.strip_prefix(&project.root).unwrap_or(&path).display()
        ),
        Err(_) => "No interpreter".to_owned(),
    }
}

/// Where `path` is in `list` (paths compare case-insensitively on Windows,
/// and with `/` and `\` alike).
pub fn interpreter_position(list: &[apex_python::Interpreter], path: &Path) -> Option<usize> {
    let key = |p: &Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        if cfg!(windows) {
            s.to_lowercase()
        } else {
            s
        }
    };
    let want = key(path);
    list.iter().position(|i| key(&i.path) == want)
}

impl App {
    pub(crate) fn wire_workspace(self: &Rc<Self>, ui: &AppWindow) {
        let api = ui.global::<WorkspaceApi>();

        let app = self.clone();
        api.on_tree_clicked(move |i| app.tree_clicked(i));
        let app = self.clone();
        api.on_refresh_tree(move || app.build_tree());
        let app = self.clone();
        api.on_close_project(move || app.close_project());

        let app = self.clone();
        api.on_select_tab(move |i| {
            if let Ok(i) = usize::try_from(i) {
                app.activate_tab(i);
            }
        });
        let app = self.clone();
        api.on_close_tab(move |i| {
            if let Ok(i) = usize::try_from(i) {
                app.close_tab(i);
            }
        });
        let app = self.clone();
        api.on_confirm_close_save(move || app.answer_close(Some(true)));
        let app = self.clone();
        api.on_confirm_close_discard(move || app.answer_close(Some(false)));
        let app = self.clone();
        api.on_confirm_close_cancel(move || app.answer_close(None));
        let app = self.clone();
        api.on_conflict_overwrite(move || app.answer_conflict(Some(true)));
        let app = self.clone();
        api.on_conflict_reload(move || app.answer_conflict(Some(false)));
        let app = self.clone();
        api.on_conflict_cancel(move || app.answer_conflict(None));

        let app = self.clone();
        api.on_save(move || {
            app.save_active();
        });
        let app = self.clone();
        api.on_select_run_config(move |i| {
            if let Ok(i) = usize::try_from(i) {
                if let Some(ws) = app.state.borrow_mut().workspace.as_mut() {
                    ws.run_choice = i;
                }
                app.with_ui(|ui| ui.global::<WorkspaceApi>().set_run_config_index(to_i32(i)));
            }
        });

        let app = self.clone();
        api.on_interpreter_opening(move || app.scan_project_interpreters(false));
        let app = self.clone();
        api.on_select_interpreter(move |i| {
            if let Ok(i) = usize::try_from(i) {
                app.select_interpreter(i);
            }
        });

        let tools = ui.global::<ToolsApi>();
        let app = self.clone();
        tools.on_run(move || app.run_selected());
        let app = self.clone();
        tools.on_stop(move || app.stop_run());
        let app = self.clone();
        tools.on_clear_output(move || app.output.clear());
        let app = self.clone();
        tools.on_send_input(move |text| app.send_input(&text));
        let app = self.clone();
        tools.on_input_finished(move || app.editor_show_active());
        let app = self.clone();
        tools.on_open_problem(move |i| {
            if let Ok(i) = usize::try_from(i) {
                app.open_problem(i);
            }
        });
    }

    // ------------------------------------------------------- interpreter

    /// Looks for interpreters for the toolbar drop-down on a worker thread
    /// (once per project unless `again`).
    pub(crate) fn scan_project_interpreters(&self, again: bool) {
        let (generation, root) = {
            let st = self.state.borrow();
            let Some(ws) = st.workspace.as_ref() else {
                return;
            };
            if ws.interpreters.is_some() && !again {
                return;
            }
            (st.generation, ws.project.root.clone())
        };
        self.with_ui(|ui| {
            ui.global::<WorkspaceApi>().set_interpreters_scanning(true);
        });
        let sink = self.sink.clone();
        let spawned = std::thread::Builder::new()
            .name("project-interpreters".into())
            .spawn(move || {
                let found = apex_python::discover_interpreters(Some(&root));
                sink.send(AppEvent::ProjectInterpreters { generation, found });
            });
        if spawned.is_err() {
            self.on_project_interpreters(generation, Vec::new());
        }
    }

    /// Interpreter discovery for the open project finished.
    pub(crate) fn on_project_interpreters(
        &self,
        generation: u64,
        found: Vec<apex_python::Interpreter>,
    ) {
        {
            let mut st = self.state.borrow_mut();
            if st.generation != generation {
                return;
            }
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            ws.interpreters = Some(found);
        }
        self.push_interpreters();
    }

    /// Sends the interpreter drop-down rows and the toolbar text to the UI.
    fn push_interpreters(&self) {
        let (labels, current, text) = {
            let st = self.state.borrow();
            let Some(ws) = st.workspace.as_ref() else {
                return;
            };
            let p = &ws.project;
            let list = ws.interpreters.as_deref().unwrap_or_default();
            let labels: Vec<SharedString> = list
                .iter()
                .map(|i| format!("{}  -  {}", i.display_name, i.path.display()).into())
                .collect();
            let current = run::project_interpreter(p)
                .ok()
                .and_then(|u| interpreter_position(list, &u));
            (labels, current, interpreter_label(p))
        };
        self.with_ui(|ui| {
            let api = ui.global::<WorkspaceApi>();
            api.set_interpreter_choices(ModelRc::new(VecModel::from(labels)));
            api.set_interpreter_choice(current.map_or(-1, to_i32));
            api.set_interpreters_scanning(false);
            api.set_interpreter_text(text.into());
        });
    }

    /// Makes interpreter `index` of the drop-down the project's interpreter
    /// and saves the project configuration.
    pub(crate) fn select_interpreter(&self, index: usize) {
        let result = {
            let mut st = self.state.borrow_mut();
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            let Some(chosen) = ws
                .interpreters
                .as_ref()
                .and_then(|l| l.get(index))
                .map(|i| i.path.clone())
            else {
                return;
            };
            let root = ws.project.root.clone();
            ws.project.config.interpreter = Some(
                chosen
                    .strip_prefix(&root)
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|_| chosen.clone()),
            );
            ws.packages.rescan(Some(chosen.clone()));
            ws.project.save().map(|()| chosen)
        };
        self.push_interpreters();
        match result {
            Ok(path) => self.notify(
                NoticeKind::Success,
                format!("Interpreter set to {}", path.display()),
            ),
            Err(e) => self.notify(
                NoticeKind::Error,
                format!("Could not save the interpreter choice: {e}"),
            ),
        }
    }

    // ------------------------------------------------------------ project

    /// Opens `root` as the current project (closing any open one).
    pub fn open_project(&self, root: PathBuf) {
        let project = match Project::open(&root) {
            Ok(p) => p,
            Err(e) => {
                self.notify(
                    NoticeKind::Error,
                    format!("Cannot open {}: {e}", root.display()),
                );
                return;
            }
        };
        self.close_workspace_now();

        let (generation, venv) = {
            let mut st = self.state.borrow_mut();
            st.generation += 1;
            st.settings.add_recent_project(&project.root);
            let venv = project.interpreter_path().as_deref().and_then(venv_root_of);
            // Plugins with fs-read may read inside the open project.
            if let Some(plugins) = st.plugins.as_ref() {
                plugins.set_workspace_root(Some(project.root.clone()));
            }
            st.workspace = Some(Workspace::new(project));
            if let Some(ws) = st.workspace.as_mut() {
                let python = run::project_interpreter(&ws.project).ok();
                ws.packages.rescan(python);
            }
            (st.generation, venv)
        };
        self.save_settings();

        let (name, path, run_labels, interpreter_text, root) = {
            let st = self.state.borrow();
            let Some(ws) = st.workspace.as_ref() else {
                return;
            };
            let p = &ws.project;
            let interpreter_text = interpreter_label(p);
            (
                p.name.clone(),
                p.root.display().to_string(),
                run::choice_labels(p),
                interpreter_text,
                p.root.clone(),
            )
        };
        self.output.clear();
        self.with_ui(|ui| {
            let api = ui.global::<WorkspaceApi>();
            api.set_project_name(name.into());
            api.set_project_path(path.into());
            api.set_tree(ModelRc::default());
            api.set_tree_selected(-1);
            api.set_tabs(ModelRc::default());
            api.set_active_tab(-1);
            api.set_highlight_mode(false);
            let labels: Vec<SharedString> = run_labels.into_iter().map(Into::into).collect();
            api.set_run_configs(ModelRc::new(VecModel::from(labels)));
            api.set_run_config_index(0);
            api.set_interpreter_text(interpreter_text.into());
            api.set_interpreter_choices(ModelRc::default());
            api.set_interpreter_choice(-1);
            api.set_interpreters_scanning(false);
            api.set_lsp_text(LspState::Starting.status_text().into());
            api.set_caret_text(SharedString::new());
            api.set_file_info_text(SharedString::new());
            let tools = ui.global::<ToolsApi>();
            tools.set_running(false);
            tools.set_run_title(SharedString::new());
            tools.set_problems(ModelRc::default());
            tools.set_lsp_missing(false);
            tools.set_lsp_hint("Starting the Python language server...".into());
            ui.global::<AppApi>().set_screen(SCREEN_WORKSPACE);
        });
        self.build_tree();
        lsp::start(root, venv, generation, self.sink.clone());

        // Show the main script straight away, as PyCharm does.
        let entry = {
            let st = self.state.borrow();
            st.workspace.as_ref().and_then(|ws| {
                ws.project
                    .config
                    .run_configurations
                    .iter()
                    .find_map(|c| match &c.target {
                        apex_core::RunTarget::Script(s) => Some(ws.project.root.join(s)),
                        apex_core::RunTarget::Module(_) => None,
                    })
                    .filter(|p| p.is_file())
            })
        };
        if let Some(entry) = entry {
            self.open_file(&entry);
        }
    }

    fn close_project(&self) {
        let unsaved = self
            .state
            .borrow()
            .workspace
            .as_ref()
            .is_some_and(Workspace::has_unsaved);
        if unsaved {
            self.notify(
                NoticeKind::Warning,
                "Some files have unsaved changes: save them (Ctrl+S) or close their tabs first",
            );
            return;
        }
        self.close_workspace_now();
        self.with_ui(|ui| ui.global::<AppApi>().set_screen(SCREEN_WELCOME));
        self.refresh_recent();
    }

    /// Closes the open project without asking: stops its program and
    /// language server and discards unsaved edits.
    pub fn close_workspace_now(&self) {
        let workspace = {
            let mut st = self.state.borrow_mut();
            st.generation += 1;
            if let Some(plugins) = st.plugins.as_ref() {
                plugins.set_workspace_root(None);
            }
            // The worker thread holds the old server's client: let it end.
            st.editor.lsp_worker = None;
            st.workspace.take()
        };
        let Some(ws) = workspace else { return };
        if let Some(session) = &ws.run {
            if let Err(e) = session.handle.kill() {
                tracing::warn!(%e, "stopping program");
            }
        }
        if let Some(client) = ws.lsp.client() {
            lsp::shutdown_in_background(client.clone());
        }
        self.with_ui(|ui| {
            let api = ui.global::<WorkspaceApi>();
            api.set_project_name(SharedString::new());
            api.set_editor_text(SharedString::new());
            api.set_tabs(ModelRc::default());
            api.set_active_tab(-1);
            api.set_tree(ModelRc::default());
            api.set_confirm_close(-1);
            api.set_confirm_conflict(-1);
            ui.global::<ToolsApi>().set_running(false);
        });
        self.editor_show_active();
    }

    /// Saves every modified tab; returns whether all saves succeeded.
    pub fn save_all(&self) -> bool {
        let count = self
            .state
            .borrow()
            .workspace
            .as_ref()
            .map_or(0, |ws| ws.tabs.len());
        let mut ok = true;
        for i in 0..count {
            ok &= self.save_tab(i);
        }
        ok
    }

    // --------------------------------------------------------------- tree

    /// Rebuilds the project tree on a worker thread.
    pub fn build_tree(&self) {
        let (root, generation) = {
            let st = self.state.borrow();
            let Some(ws) = st.workspace.as_ref() else {
                return;
            };
            (ws.project.root.clone(), st.generation)
        };
        let sink = self.sink.clone();
        let spawned = std::thread::Builder::new()
            .name("file-tree".into())
            .spawn(move || {
                // Lazy: only the top level; folders are read on expand.
                let result = FileTree::build_lazy(&root, &apex_core::FileTreeOptions::default())
                    .map_err(|e| e.to_string());
                sink.send(AppEvent::TreeBuilt { generation, result });
            });
        if let Err(e) = spawned {
            self.notify(NoticeKind::Error, format!("Cannot scan the project: {e}"));
        }
    }

    pub(crate) fn on_tree_built(&self, generation: u64, result: Result<FileTree, String>) {
        {
            let mut st = self.state.borrow_mut();
            if st.generation != generation {
                return;
            }
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            match result {
                Ok(tree) => ws.tree = Some(tree),
                Err(e) => {
                    drop(st);
                    self.notify(NoticeKind::Error, format!("Cannot scan the project: {e}"));
                    return;
                }
            }
        }
        self.push_tree();
        crate::commands::invalidate_file_index();
    }

    pub(crate) fn push_tree(&self) {
        // Folders that are expanded but not read yet load on a worker and
        // call push_tree again when they arrive.
        self.schedule_tree_loads();
        let (items, selected) = {
            let mut st = self.state.borrow_mut();
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            let Some(tree) = ws.tree.as_ref() else { return };
            ws.rows = if ws.tree_filter.trim().is_empty() {
                tree.visible_rows(&ws.expanded)
            } else {
                crate::tree_view::filter_rows(tree, &ws.tree_filter)
            };
            let items: Vec<TreeItem> = ws
                .rows
                .iter()
                .map(|r| TreeItem {
                    name: r.name.as_str().into(),
                    depth: to_i32(r.depth),
                    is_dir: r.is_dir,
                    expanded: r.expanded,
                    has_children: r.has_children,
                    icon: crate::tree_view::file_icon_kind(&r.name, r.is_dir).into(),
                })
                .collect();
            let selected = ws
                .selected
                .as_ref()
                .and_then(|s| ws.rows.iter().position(|r| &r.path == s));
            (items, selected)
        };
        self.with_ui(|ui| {
            let api = ui.global::<WorkspaceApi>();
            crate::tree_view::set_tree_rows(&api, items);
            api.set_tree_selected(selected.map_or(-1, to_i32));
        });
    }

    fn tree_clicked(&self, index: i32) {
        let row = {
            let mut st = self.state.borrow_mut();
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            let Some(row) = usize::try_from(index)
                .ok()
                .and_then(|i| ws.rows.get(i))
                .cloned()
            else {
                return;
            };
            ws.selected = Some(row.path.clone());
            if row.is_dir && !ws.expanded.remove(&row.path) {
                ws.expanded.insert(row.path.clone());
            }
            row
        };
        if row.is_dir {
            self.push_tree();
        } else {
            self.with_ui(|ui| ui.global::<WorkspaceApi>().set_tree_selected(index));
            self.open_file(&row.path);
        }
    }

    // --------------------------------------------------------------- tabs

    /// Opens `path` in a tab (or switches to its tab).
    pub fn open_file(&self, path: &Path) {
        let existing = {
            let st = self.state.borrow();
            let Some(ws) = st.workspace.as_ref() else {
                return;
            };
            ws.tab_index(path)
        };
        if let Some(i) = existing {
            self.activate_tab(i);
            return;
        }
        match std::fs::metadata(path) {
            Ok(m) if m.len() > MAX_FILE_BYTES => {
                self.notify(
                    NoticeKind::Warning,
                    format!(
                        "{} is larger than {} MB and is not opened",
                        path.display(),
                        MAX_FILE_BYTES / (1024 * 1024)
                    ),
                );
                return;
            }
            Ok(_) => {}
            Err(e) => {
                self.notify(
                    NoticeKind::Error,
                    format!("Cannot open {}: {e}", path.display()),
                );
                return;
            }
        }
        let buffer = match Buffer::open(path) {
            Ok(b) => b,
            Err(e) => {
                self.notify(
                    NoticeKind::Error,
                    format!("Cannot open {}: {e}", path.display()),
                );
                return;
            }
        };
        let index = {
            let mut st = self.state.borrow_mut();
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            let mut tab = Tab::new(path.to_path_buf(), buffer);
            if let Some(client) = ws.lsp.client() {
                lsp::send_open(client, &mut tab);
            }
            ws.tabs.push(tab);
            ws.tabs.len() - 1
        };
        self.activate_tab(index);
    }

    pub(crate) fn push_tabs(&self) {
        let (tabs, active) = {
            let mut st = self.state.borrow_mut();
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            if let Some(path) = ws.active_tab().map(|t| t.path.clone()) {
                crate::switcher::touch(&mut ws.mru, &path);
            }
            let tabs: Vec<EditorTab> = ws
                .tabs
                .iter()
                .map(|t| EditorTab {
                    title: t.title().into(),
                    path: t.path.display().to_string().into(),
                    dirty: t.buffer.is_dirty(),
                })
                .collect();
            (tabs, ws.active)
        };
        self.with_ui(|ui| {
            let api = ui.global::<WorkspaceApi>();
            api.set_tabs(ModelRc::new(VecModel::from(tabs)));
            api.set_active_tab(active.map_or(-1, to_i32));
        });
    }

    fn activate_tab(&self, index: usize) {
        {
            let mut st = self.state.borrow_mut();
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            if index >= ws.tabs.len() {
                return;
            }
            if let Some(tab) = ws.active_tab_mut() {
                tab.buffer.break_undo_group();
            }
            ws.active = Some(index);
        }
        self.push_tabs();
        self.load_editor();
    }

    /// Shows the active tab in the code editor and updates the status bar.
    pub(crate) fn load_editor(&self) {
        let info = {
            let st = self.state.borrow();
            st.workspace
                .as_ref()
                .and_then(Workspace::active_tab)
                .map(|tab| {
                    let b = &tab.buffer;
                    format!(
                        "{}  {}  UTF-8{}",
                        b.language().name(),
                        b.line_ending().label(),
                        if b.has_bom() { " BOM" } else { "" }
                    )
                })
        };
        self.with_ui(|ui| {
            let api = ui.global::<WorkspaceApi>();
            api.set_file_info_text(info.unwrap_or_default().into());
            api.set_highlight_mode(false);
            api.set_can_highlight(false);
        });
        self.editor_show_active();
    }

    pub(crate) fn close_tab(&self, index: usize) {
        let dirty = {
            let mut st = self.state.borrow_mut();
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            let Some(tab) = ws.tabs.get(index) else {
                return;
            };
            let dirty = tab.buffer.is_dirty();
            if dirty {
                ws.pending_close = Some(index);
            }
            dirty
        };
        if dirty {
            self.with_ui(|ui| ui.global::<WorkspaceApi>().set_confirm_close(to_i32(index)));
        } else {
            self.remove_tab(index);
        }
    }

    /// `Some(true)` save then close, `Some(false)` discard, `None` cancel.
    fn answer_close(&self, answer: Option<bool>) {
        let pending = self
            .state
            .borrow_mut()
            .workspace
            .as_mut()
            .and_then(|ws| ws.pending_close.take());
        self.with_ui(|ui| ui.global::<WorkspaceApi>().set_confirm_close(-1));
        let Some(index) = pending else { return };
        match answer {
            Some(true) => {
                if self.save_tab(index) {
                    self.remove_tab(index);
                }
            }
            Some(false) => self.remove_tab(index),
            None => {}
        }
    }

    fn remove_tab(&self, index: usize) {
        {
            let mut st = self.state.borrow_mut();
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            if index >= ws.tabs.len() {
                return;
            }
            let tab = ws.tabs.remove(index);
            ws.packages.forget(&tab.path);
            if tab.lsp_open {
                if let (Some(client), Ok(uri)) = (ws.lsp.client(), path_to_uri(&tab.path)) {
                    let _ = client.did_close(&uri);
                }
            }
            ws.active = match ws.active {
                _ if ws.tabs.is_empty() => None,
                Some(a) if a > index => Some(a - 1),
                Some(a) if a == index => Some(index.min(ws.tabs.len() - 1)),
                other => other,
            };
        }
        self.push_tabs();
        self.load_editor();
    }

    // ------------------------------------------------------------- editor

    pub(crate) fn save_active(&self) -> bool {
        let active = self
            .state
            .borrow()
            .workspace
            .as_ref()
            .and_then(|ws| ws.active);
        active.is_some_and(|i| self.save_tab(i))
    }

    /// Saves tab `index` if it is modified. Returns `false` on failure, or
    /// when the file changed on disk (the user is then asked whether to
    /// overwrite or reload it, and nothing is written yet).
    fn save_tab(&self, index: usize) -> bool {
        self.write_tab(index, false)
    }

    /// Writes tab `index`; `overwrite` skips the changed-on-disk check.
    fn write_tab(&self, index: usize, overwrite: bool) -> bool {
        let result = {
            let mut st = self.state.borrow_mut();
            let Some(ws) = st.workspace.as_mut() else {
                return true;
            };
            let client = ws.lsp.client().cloned();
            let Some(tab) = ws.tabs.get_mut(index) else {
                return true;
            };
            if !tab.buffer.is_dirty() {
                return true;
            }
            tab.buffer.break_undo_group();
            let result = if overwrite {
                tab.buffer.save_overwrite()
            } else {
                tab.buffer.save()
            };
            if result.is_ok() && tab.lsp_open {
                if let (Some(client), Ok(uri)) = (client, path_to_uri(&tab.path)) {
                    let _ = client.did_save(&uri, None);
                }
            }
            let title = tab.title();
            if matches!(result, Err(EditorError::ChangedOnDisk { .. })) {
                // Ask about the first conflict; later ones are asked on the
                // next save.
                if ws.pending_conflict.is_none() {
                    ws.pending_conflict = Some(index);
                }
            }
            result.map(|()| title)
        };
        self.push_tabs();
        match result {
            Ok(title) => {
                self.notify(NoticeKind::Info, format!("Saved {title}"));
                true
            }
            Err(EditorError::ChangedOnDisk { .. }) => {
                let pending = self
                    .state
                    .borrow()
                    .workspace
                    .as_ref()
                    .and_then(|ws| ws.pending_conflict);
                if let Some(pending) = pending {
                    self.with_ui(|ui| {
                        ui.global::<WorkspaceApi>()
                            .set_confirm_conflict(to_i32(pending));
                    });
                }
                false
            }
            Err(e) => {
                self.notify(NoticeKind::Error, format!("Save failed: {e}"));
                false
            }
        }
    }

    /// `Some(true)` overwrite the file, `Some(false)` reload it, `None` cancel.
    fn answer_conflict(&self, answer: Option<bool>) {
        let pending = self
            .state
            .borrow_mut()
            .workspace
            .as_mut()
            .and_then(|ws| ws.pending_conflict.take());
        self.with_ui(|ui| ui.global::<WorkspaceApi>().set_confirm_conflict(-1));
        let Some(index) = pending else { return };
        match answer {
            Some(true) => {
                self.write_tab(index, true);
            }
            Some(false) => self.reload_tab(index),
            None => {}
        }
    }

    /// Replaces tab `index` with the file on disk (undoable).
    fn reload_tab(&self, index: usize) {
        let (result, active) = {
            let mut st = self.state.borrow_mut();
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            let client = ws.lsp.client().cloned();
            let active = ws.active == Some(index);
            let Some(tab) = ws.tabs.get_mut(index) else {
                return;
            };
            tab.buffer.break_undo_group();
            let result = tab.buffer.reload().map(|()| tab.title());
            if result.is_ok() {
                if let Some(client) = client {
                    lsp::send_change(&client, tab);
                }
            }
            (result, active)
        };
        self.push_tabs();
        match result {
            Ok(title) => {
                if active {
                    self.load_editor();
                }
                self.notify(NoticeKind::Info, format!("Reloaded {title} from disk"));
            }
            Err(e) => self.notify(NoticeKind::Error, format!("Reload failed: {e}")),
        }
    }

    // ---------------------------------------------------------------- run

    pub(crate) fn append_output(&self, text: impl Into<String>, kind: i32) {
        append_capped(
            &self.output,
            vec![OutputLine {
                text: SharedString::from(text.into()),
                kind,
            }],
            OUTPUT_CAP,
        );
    }

    pub(crate) fn run_selected(&self) {
        let running = self
            .state
            .borrow()
            .workspace
            .as_ref()
            .is_some_and(|ws| ws.run.is_some());
        if running {
            self.notify(
                NoticeKind::Info,
                "A program is already running; stop it first",
            );
            return;
        }
        // Like PyCharm: save modified files before running.
        if !self.save_all() {
            return;
        }
        let planned = {
            let st = self.state.borrow();
            let Some(ws) = st.workspace.as_ref() else {
                return;
            };
            let choice = run::choice_at(&ws.project, ws.run_choice);
            let current = ws.active_tab().map(|t| t.path.clone());
            run::plan_run(&ws.project, choice, current.as_deref())
        };
        self.with_ui(|ui| {
            let tools = ui.global::<ToolsApi>();
            tools.set_tool_open(true);
            tools.set_tool_tab(0);
        });
        let plan = match planned {
            Ok(plan) => plan,
            Err(e) => {
                let kind = match e {
                    RunPlanError::NoPythonFile => NoticeKind::Info,
                    _ => NoticeKind::Error,
                };
                self.append_output(capitalize(&e.to_string()), OUT_STDERR);
                self.notify(kind, capitalize(&e.to_string()));
                return;
            }
        };
        let id = {
            let mut st = self.state.borrow_mut();
            st.next_run_id += 1;
            st.next_run_id
        };
        self.output.clear();
        self.append_output(
            format!(
                "> {} {}",
                plan.request.python.display(),
                describe_target(&plan.request)
            ),
            OUT_IDE,
        );
        match run::start(&plan, id, &self.sink) {
            Ok(session) => {
                if let Some(ws) = self.state.borrow_mut().workspace.as_mut() {
                    ws.run = Some(session);
                }
                self.with_ui(|ui| {
                    let tools = ui.global::<ToolsApi>();
                    tools.set_running(true);
                    tools.set_run_title(plan.title.as_str().into());
                });
            }
            Err(e) => {
                self.append_output(format!("Could not start Python: {e}"), OUT_STDERR);
                self.notify(NoticeKind::Error, format!("Could not start Python: {e}"));
            }
        }
    }

    /// Appends a batch of program output lines (events of other runs are
    /// ignored).
    pub(crate) fn on_run_output(&self, events: Vec<(u64, RunEvent)>) {
        if events.is_empty() {
            return;
        }
        let current = self
            .state
            .borrow()
            .workspace
            .as_ref()
            .and_then(|ws| ws.run.as_ref())
            .map(|r| r.id);
        let lines: Vec<OutputLine> = events
            .into_iter()
            .filter(|(id, _)| Some(*id) == current)
            .filter_map(|(_, event)| match event {
                RunEvent::Stdout(line) => Some((line, OUT_STDOUT)),
                RunEvent::Stderr(line) => Some((line, OUT_STDERR)),
                RunEvent::Exited(_) => None,
            })
            .map(|(text, kind)| OutputLine {
                text: SharedString::from(text),
                kind,
            })
            .collect();
        append_capped(&self.output, lines, OUTPUT_CAP);
    }

    pub(crate) fn on_run_event(&self, run_id: u64, event: RunEvent) {
        let current = self
            .state
            .borrow()
            .workspace
            .as_ref()
            .and_then(|ws| ws.run.as_ref())
            .map(|r| r.id);
        if current != Some(run_id) {
            return;
        }
        match event {
            RunEvent::Stdout(line) => self.append_output(line, OUT_STDOUT),
            RunEvent::Stderr(line) => self.append_output(line, OUT_STDERR),
            RunEvent::Exited(code) => {
                self.append_output("", OUT_IDE);
                self.append_output(run::exit_message(code), OUT_IDE);
                if let Some(ws) = self.state.borrow_mut().workspace.as_mut() {
                    ws.run = None;
                }
                self.with_ui(|ui| ui.global::<ToolsApi>().set_running(false));
            }
        }
    }

    fn stop_run(&self) {
        let result = {
            let st = self.state.borrow();
            match st.workspace.as_ref().and_then(|ws| ws.run.as_ref()) {
                Some(session) => session.handle.kill(),
                None => return,
            }
        };
        match result {
            Ok(()) => self.append_output("Stopping...", OUT_IDE),
            Err(e) => self.notify(
                NoticeKind::Error,
                format!("Could not stop the program: {e}"),
            ),
        }
    }

    fn send_input(&self, text: &str) {
        // Queued for the runner's writer thread, so this never blocks the UI
        // even when the program is not reading its input.
        let result = {
            let st = self.state.borrow();
            match st.workspace.as_ref().and_then(|ws| ws.run.as_ref()) {
                Some(session) => session.handle.write_stdin(&format!("{text}\n")),
                None => return,
            }
        };
        match result {
            Ok(()) => self.append_output(text, OUT_IDE),
            Err(e) => self.notify(NoticeKind::Warning, format!("Input not sent: {e}")),
        }
    }

    // ---------------------------------------------------------- problems

    fn push_problems(&self) {
        let rows = {
            let mut st = self.state.borrow_mut();
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            ws.problems = ws.diagnostics.rows(&ws.project.root);
            ws.problems
                .iter()
                .map(|p| Problem {
                    severity: p.severity,
                    message: p.message.as_str().into(),
                    location: p.location.as_str().into(),
                    source: p.source.as_str().into(),
                })
                .collect::<Vec<_>>()
        };
        let errors = rows.iter().filter(|p| p.severity == 1).count();
        self.with_ui(|ui| {
            let tools = ui.global::<ToolsApi>();
            tools.set_error_count(to_i32(errors));
            tools.set_problems(ModelRc::new(VecModel::from(rows)));
        });
    }

    fn open_problem(&self, index: usize) {
        let target = self
            .state
            .borrow()
            .workspace
            .as_ref()
            .and_then(|ws| ws.problems.get(index))
            .map(|p| (p.path.clone(), p.line, p.col));
        let Some((path, line, col)) = target else {
            return;
        };
        self.open_file(&path);
        let shown = self
            .state
            .borrow()
            .workspace
            .as_ref()
            .and_then(Workspace::active_tab)
            .is_some_and(|t| t.path == path);
        if shown {
            self.editor_goto_utf16(line, col);
        }
    }

    // --------------------------------------------------------------- lsp

    pub(crate) fn on_lsp_started(
        &self,
        generation: u64,
        result: Result<(Arc<apex_lsp::LspClient>, String), LspStartError>,
    ) {
        let stale = self.state.borrow().generation != generation;
        if stale {
            if let Ok((client, _)) = result {
                lsp::shutdown_in_background(client);
            }
            return;
        }
        let (status, missing, hint) = {
            let mut st = self.state.borrow_mut();
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            ws.lsp = match result {
                Ok((client, name)) => {
                    for tab in &mut ws.tabs {
                        lsp::send_open(&client, tab);
                    }
                    LspState::Ready { client, name }
                }
                Err(LspStartError::NotFound) => LspState::Missing,
                Err(e) => LspState::Failed(e.to_string()),
            };
            let (missing, hint) = match &ws.lsp {
                LspState::Ready { name, .. } => (
                    false,
                    format!("{name} checks Python files as you type and on save."),
                ),
                LspState::Missing => (
                    true,
                    format!(
                        "Install basedpyright to see errors and warnings here, then reopen \
                         the project. Run in a terminal: {}",
                        lsp::INSTALL_HINT
                    ),
                ),
                LspState::Failed(e) => (false, format!("Language server problem: {e}")),
                LspState::Starting => (false, String::new()),
            };
            (ws.lsp.status_text(), missing, hint)
        };
        self.with_ui(|ui| {
            ui.global::<WorkspaceApi>().set_lsp_text(status.into());
            let tools = ui.global::<ToolsApi>();
            tools.set_lsp_missing(missing);
            tools.set_lsp_hint(hint.into());
        });
    }

    pub(crate) fn on_lsp_event(&self, generation: u64, event: LspEvent) {
        if self.state.borrow().generation != generation {
            return;
        }
        match event {
            LspEvent::Diagnostics(params) => {
                let updated = self
                    .state
                    .borrow_mut()
                    .workspace
                    .as_mut()
                    .is_some_and(|ws| ws.diagnostics.update(params));
                if updated {
                    self.push_problems();
                    if let Some(ws) = self.state.borrow_mut().workspace.as_mut() {
                        let files: Vec<PathBuf> = ws.diagnostics.files().cloned().collect();
                        ws.packages.lsp_missing = files
                            .into_iter()
                            .map(|f| {
                                let m = ws.diagnostics.missing_imports(&f);
                                (f, m)
                            })
                            .collect();
                    }
                    self.refresh_highlight();
                }
            }
            LspEvent::ShowMessage(params) => {
                use apex_lsp::lsp_types::MessageType;
                let kind = match params.typ {
                    MessageType::ERROR => NoticeKind::Error,
                    MessageType::WARNING => NoticeKind::Warning,
                    _ => NoticeKind::Info,
                };
                self.notify(kind, format!("Language server: {}", params.message));
            }
            LspEvent::Disconnected => {
                let status = {
                    let mut st = self.state.borrow_mut();
                    let Some(ws) = st.workspace.as_mut() else {
                        return;
                    };
                    ws.lsp = LspState::Failed("the server exited".into());
                    for tab in &mut ws.tabs {
                        tab.lsp_open = false;
                    }
                    ws.lsp.status_text()
                };
                self.with_ui(|ui| {
                    ui.global::<WorkspaceApi>().set_lsp_text(status.into());
                    ui.global::<ToolsApi>().set_lsp_hint(
                        "The language server exited; reopen the project to restart it.".into(),
                    );
                });
            }
            LspEvent::Log(params) => tracing::debug!(target: "lsp", "{}", params.message),
            LspEvent::Stderr(line) => tracing::debug!(target: "lsp", "{line}"),
            _ => {}
        }
    }
}

fn describe_target(request: &apex_python::RunRequest) -> String {
    let target = match &request.target {
        apex_python::RunTarget::Script(p) => p.display().to_string(),
        apex_python::RunTarget::Module(m) => format!("-m {m}"),
        apex_python::RunTarget::Code(_) => "-c ...".to_owned(),
    };
    if request.args.is_empty() {
        target
    } else {
        format!("{target} {}", request.args.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venv_root_needs_pyvenv_cfg() {
        let dir = tempfile::tempdir().unwrap();
        let venv = dir.path().join(".venv");
        let python = venv.join(apex_core::project::venv_interpreter_relative(Path::new("")));
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, "").unwrap();
        assert_eq!(venv_root_of(&python), None);
        std::fs::write(venv.join("pyvenv.cfg"), "home = x").unwrap();
        assert_eq!(venv_root_of(&python), Some(venv));
    }

    #[test]
    fn interpreter_position_matches_paths_loosely() {
        let it = |p: &str| apex_python::Interpreter {
            path: PathBuf::from(p),
            version: (3, 12, 0),
            kind: apex_python::InterpreterKind::System,
            display_name: "Python 3.12".into(),
        };
        let list = [it("C:/a/python.exe"), it("C:/b/.venv/Scripts/python.exe")];
        assert_eq!(
            interpreter_position(&list, Path::new("C:\\b\\.venv\\Scripts\\python.exe")),
            Some(1)
        );
        assert_eq!(
            interpreter_position(&list, Path::new("C:/c/python.exe")),
            None
        );
    }

    #[test]
    fn targets_are_described() {
        let mut r =
            apex_python::RunRequest::new("python", apex_python::RunTarget::Module("pytest".into()));
        assert_eq!(describe_target(&r), "-m pytest");
        r.args = vec!["-q".into()];
        assert_eq!(describe_target(&r), "-m pytest -q");
    }
}
