//! The application controller: builds the window, owns the UI-thread
//! [`State`], routes worker events, and handles the app-wide concerns
//! (notices, themes, quitting, the Databases and Plugins panels).
//!
//! Screen-specific behaviour lives in `welcome.rs` and `workspace.rs` as
//! further `impl App` blocks.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use apex_core::{paths, Settings};
use slint::{CloseRequestResponse, ComponentHandle, Image, Model, ModelRc, SharedString, VecModel};

use crate::db_panel::{self, DbCommand, DbReply, DbWorker};
use crate::events::{self, AppEvent, EventQueue, EventSink, NoticeKind};
use crate::state::{NewProjectState, State};
use crate::{
    plugins, theming, AppApi, AppWindow, CommandRow, DbApi, DbTable, DriverCard, OutputLine,
    PluginRow, PluginsApi, ResultRow,
};

/// How long info and success notices stay in the status bar.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(8);
/// How long warnings and errors stay.
const PROBLEM_NOTICE_TIMEOUT: Duration = Duration::from_secs(20);

/// The controller. Lives on the UI thread in an `Rc`; callbacks hold clones.
pub struct App {
    /// The window (weak, so callbacks do not keep it alive).
    pub ui: slint::Weak<AppWindow>,
    /// Mutable state. Borrow briefly; never across a call that can re-enter.
    pub state: RefCell<State>,
    /// For workers to post results.
    pub sink: EventSink,
    queue: EventQueue,
    /// Run tool window lines (appended in place for speed).
    pub output: Rc<VecModel<OutputLine>>,
    /// New Project progress log.
    pub create_log: Rc<VecModel<SharedString>>,
    /// Bumped per notice, so an auto-hide timer only hides its own notice.
    notice_serial: Rc<Cell<u64>>,
}

/// Starts the IDE and blocks until the window closes. `initial` is a
/// project folder to open straight away.
pub fn run(initial: Option<PathBuf>) -> anyhow::Result<()> {
    let ui = AppWindow::new()?;
    let (sink, queue) = events::channel();

    let loaded = Settings::load();
    let mut warnings: Vec<String> = Vec::new();
    if let Some(w) = &loaded.warning {
        warnings.push(format!("Settings were reset: {w}"));
    }

    let (mut themes, theme_errors) = theming::load_registry(paths::themes_dir().ok().as_deref());
    warnings.extend(
        theme_errors
            .into_iter()
            .map(|e| format!("Theme skipped: {e}")),
    );

    let plugins_dir = paths::plugins_dir().ok();
    let (plugin_manager, plugin_problems) = plugins::start(sink.clone(), plugins_dir.as_deref());
    warnings.extend(plugin_problems);
    if let Some(manager) = &plugin_manager {
        warnings.extend(plugins::sync_plugin_themes(manager, &mut themes));
    }

    let db = match DbWorker::spawn(sink.clone()) {
        Ok(worker) => Some(worker),
        Err(e) => {
            warnings.push(format!("Database console unavailable: {e}"));
            None
        }
    };

    let theme_ids = themes.list().map(|t| t.id.clone()).collect();
    let state = State {
        settings: loaded.settings,
        settings_path: paths::settings_file().ok(),
        themes,
        theme_ids,
        new_project: NewProjectState::default(),
        workspace: None,
        generation: 0,
        next_run_id: 1,
        plugins: plugin_manager,
        plugin_ids: Vec::new(),
        command_ids: Vec::new(),
        db,
        db_tables: Vec::new(),
        recent_missing: std::collections::HashMap::new(),
        switcher: Vec::new(),
        editor: Default::default(),
    };

    let app = Rc::new(App {
        ui: ui.as_weak(),
        state: RefCell::new(state),
        sink,
        queue,
        output: Rc::new(VecModel::default()),
        create_log: Rc::new(VecModel::default()),
        notice_serial: Rc::new(Cell::new(0)),
    });

    {
        let weak = Rc::downgrade(&app);
        events::set_dispatcher(move || {
            if let Some(app) = weak.upgrade() {
                app.handle_events();
            }
        });
    }

    app.init_ui(&ui, plugins_dir);
    app.wire_app(&ui);
    app.wire_welcome(&ui);
    app.wire_new_project(&ui);
    app.wire_workspace(&ui);
    app.wire_editor(&ui);
    app.wire_tree_view(&ui);
    app.wire_shell(&ui);
    app.wire_tools(&ui);
    crate::screenshot::on_ready(&app, &ui);

    if let Some(w) = warnings.first() {
        let more = warnings.len().saturating_sub(1);
        let text = if more > 0 {
            format!("{w} (+{more} more in the log)")
        } else {
            w.clone()
        };
        app.notify(NoticeKind::Warning, text);
    }
    for w in &warnings {
        tracing::warn!("{w}");
    }

    if let Some(path) = initial {
        app.open_project(path);
    }

    ui.run()?;

    app.shutdown();
    events::clear_dispatcher();
    Ok(())
}

impl App {
    /// Runs `f` with the window if it still exists.
    pub fn with_ui(&self, f: impl FnOnce(&AppWindow)) {
        if let Some(ui) = self.ui.upgrade() {
            f(&ui);
        }
    }

    /// Shows a status message; it hides itself after a while.
    pub fn notify(&self, kind: NoticeKind, text: impl Into<String>) {
        let text = text.into();
        match kind {
            NoticeKind::Error => tracing::error!("{text}"),
            NoticeKind::Warning => tracing::warn!("{text}"),
            _ => tracing::info!("{text}"),
        }
        let serial = self.notice_serial.get().wrapping_add(1);
        self.notice_serial.set(serial);
        self.with_ui(|ui| {
            let api = ui.global::<AppApi>();
            api.set_notice(SharedString::from(text.as_str()));
            api.set_notice_kind(kind as i32);
        });
        let timeout = match kind {
            NoticeKind::Warning | NoticeKind::Error => PROBLEM_NOTICE_TIMEOUT,
            _ => NOTICE_TIMEOUT,
        };
        let ui = self.ui.clone();
        let current = self.notice_serial.clone();
        slint::Timer::single_shot(timeout, move || {
            if current.get() == serial {
                if let Some(ui) = ui.upgrade() {
                    ui.global::<AppApi>().set_notice(SharedString::new());
                }
            }
        });
    }

    /// Saves settings, reporting failures as a notice.
    pub fn save_settings(&self) {
        let result = {
            let st = self.state.borrow();
            match &st.settings_path {
                Some(path) => st.settings.save_to(path).map_err(|e| e.to_string()),
                None => Err("the settings folder is unknown on this system".to_owned()),
            }
        };
        if let Err(e) = result {
            self.notify(NoticeKind::Warning, format!("Could not save settings: {e}"));
        }
    }

    fn init_ui(self: &Rc<Self>, ui: &AppWindow, plugins_dir: Option<PathBuf>) {
        self.push_theme_list(ui);

        ui.global::<crate::ToolsApi>()
            .set_output(ModelRc::from(self.output.clone()));
        ui.global::<crate::NewProjectApi>()
            .set_log(ModelRc::from(self.create_log.clone()));

        // Databases.
        let cards: Vec<DriverCard> = db_panel::sorted_catalog()
            .into_iter()
            .map(|d| {
                let logo = d
                    .logo_svg()
                    .and_then(|svg| Image::load_from_svg_data(svg).ok());
                let (r, g, b) = d.brand_rgb().unwrap_or((128, 128, 128));
                DriverCard {
                    id: d.id.into(),
                    name: d.name.into(),
                    category: d.category.label().into(),
                    has_logo: logo.is_some(),
                    logo: logo.unwrap_or_default(),
                    brand: slint::Color::from_rgb_u8(r, g, b),
                    available: d.is_available(),
                }
            })
            .collect();
        let db = ui.global::<DbApi>();
        db.set_drivers(ModelRc::new(VecModel::from(cards)));

        // Plugins.
        ui.global::<PluginsApi>().set_plugins_dir(
            plugins_dir
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unknown)".into())
                .into(),
        );
        self.refresh_plugins();

        self.refresh_recent();
        self.prepare_new_project_defaults();
    }

    fn wire_app(self: &Rc<Self>, ui: &AppWindow) {
        let api = ui.global::<AppApi>();

        let app = self.clone();
        api.on_select_theme(move |i| app.select_theme(i));

        let app = self.clone();
        api.on_dismiss_notice(move || {
            app.with_ui(|ui| ui.global::<AppApi>().set_notice(SharedString::new()));
        });

        let app = self.clone();
        api.on_quit_discard(move || {
            app.with_ui(|ui| {
                ui.global::<AppApi>().set_confirm_quit(false);
                let _ = ui.hide();
            });
        });

        let app = self.clone();
        api.on_quit_save_all(move || {
            if app.save_all() {
                app.with_ui(|ui| {
                    ui.global::<AppApi>().set_confirm_quit(false);
                    let _ = ui.hide();
                });
            } else {
                app.with_ui(|ui| ui.global::<AppApi>().set_confirm_quit(false));
            }
        });

        let app = self.clone();
        ui.window().on_close_requested(move || {
            let unsaved = app
                .state
                .try_borrow()
                .map(|s| s.workspace.as_ref().is_some_and(|w| w.has_unsaved()))
                .unwrap_or(false);
            if unsaved {
                app.with_ui(|ui| ui.global::<AppApi>().set_confirm_quit(true));
                CloseRequestResponse::KeepWindowShown
            } else {
                CloseRequestResponse::HideWindow
            }
        });
    }

    fn wire_tools(self: &Rc<Self>, ui: &AppWindow) {
        // Databases.
        let db = ui.global::<DbApi>();

        let app = self.clone();
        db.on_driver_clicked(move |i| app.driver_clicked(i));

        let app = self.clone();
        db.on_connect(move || {
            let form = app.ui.upgrade().map(|ui| {
                let db = ui.global::<DbApi>();
                (db.get_path(), db.get_create_new())
            });
            if let Some((path, create)) = form {
                app.db_send(DbCommand::Connect {
                    path: PathBuf::from(path.trim()),
                    create,
                });
            }
        });

        let app = self.clone();
        db.on_disconnect(move || app.db_send(DbCommand::Disconnect));

        let app = self.clone();
        db.on_cancel(move || {
            let cancelled = app.state.borrow().db.as_ref().is_some_and(DbWorker::cancel);
            if cancelled {
                app.with_ui(|ui| {
                    ui.global::<DbApi>()
                        .set_result_status("Cancelling...".into())
                });
            }
        });

        let app = self.clone();
        db.on_execute(move || {
            let sql = app.ui.upgrade().map(|ui| ui.global::<DbApi>().get_sql());
            if let Some(sql) = sql {
                app.db_send(DbCommand::Execute(sql.to_string()));
            }
        });

        let app = self.clone();
        db.on_table_clicked(move |i| {
            let table = app
                .state
                .borrow()
                .db_tables
                .get(usize::try_from(i).unwrap_or(usize::MAX))
                .cloned();
            if let Some(table) = table {
                let sql = db_panel::select_all_sql(&table);
                app.with_ui(|ui| ui.global::<DbApi>().set_sql(sql.as_str().into()));
                app.db_send(DbCommand::Execute(sql));
            }
        });

        // Plugins.
        let plugins = ui.global::<PluginsApi>();

        let app = self.clone();
        plugins.on_set_enabled(move |i, on| app.set_plugin_enabled(i, on));

        let app = self.clone();
        plugins.on_run_command(move |i| app.run_plugin_command(i));

        let app = self.clone();
        plugins.on_grant_permissions(move |i| app.grant_plugin_permissions(i));
    }

    /// Fills the theme picker from the registry and applies the selected theme.
    fn push_theme_list(&self, ui: &AppWindow) {
        let mut st = self.state.borrow_mut();
        let st = &mut *st;
        st.theme_ids = st.themes.list().map(|t| t.id.clone()).collect();
        let names: Vec<SharedString> = st
            .themes
            .list()
            .map(|t| SharedString::from(t.name.as_str()))
            .collect();
        let theme = st.themes.get_or_default(&st.settings.theme);
        let index = st.theme_ids.iter().position(|id| *id == theme.id);
        theming::apply(ui, theme, &st.settings);
        let api = ui.global::<AppApi>();
        api.set_theme_names(ModelRc::new(VecModel::from(names)));
        api.set_theme_index(index.map_or(0, to_i32));
    }

    /// Re-syncs plugin-contributed themes after plugins were enabled,
    /// disabled or restarted.
    fn sync_plugin_contributions(&self) {
        let problems = {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            match st.plugins.as_ref() {
                Some(manager) => plugins::sync_plugin_themes(manager, &mut st.themes),
                None => Vec::new(),
            }
        };
        if let Some(ui) = self.ui.upgrade() {
            self.push_theme_list(&ui);
        }
        self.refresh_highlight();
        if let Some(first) = problems.first() {
            self.notify(NoticeKind::Warning, first.clone());
        }
        for p in &problems {
            tracing::warn!("{p}");
        }
    }

    fn select_theme(&self, index: i32) {
        let applied = {
            let mut st = self.state.borrow_mut();
            let Some(id) = st
                .theme_ids
                .get(usize::try_from(index).unwrap_or(usize::MAX))
                .cloned()
            else {
                return;
            };
            st.settings.theme = id;
            true
        };
        if applied {
            self.apply_current_theme();
            self.save_settings();
        }
    }

    /// Re-applies the selected theme (also refreshes the highlighted view,
    /// whose colors are baked into its model).
    pub fn apply_current_theme(&self) {
        {
            let st = self.state.borrow();
            let theme = st.themes.get_or_default(&st.settings.theme);
            let index = st.theme_ids.iter().position(|id| *id == theme.id);
            self.with_ui(|ui| {
                theming::apply(ui, theme, &st.settings);
                ui.global::<AppApi>()
                    .set_theme_index(index.map_or(0, to_i32));
            });
        }
        self.refresh_highlight();
    }

    /// Drains queued worker events (called on the UI thread). Consecutive
    /// program output lines are appended to the Run window as one batch.
    fn handle_events(&self) {
        let mut output = Vec::new();
        for event in self.queue.drain(&self.sink) {
            let event = match event {
                AppEvent::Run {
                    run_id,
                    event:
                        line @ (apex_python::RunEvent::Stdout(_) | apex_python::RunEvent::Stderr(_)),
                } => {
                    output.push((run_id, line));
                    continue;
                }
                other => other,
            };
            self.on_run_output(std::mem::take(&mut output));
            match event {
                AppEvent::Interpreters { found, uv } => self.on_interpreters(found, uv),
                AppEvent::CreateProgress(p) => self.on_create_progress(p),
                AppEvent::CreateFinished(result) => self.on_create_finished(result),
                AppEvent::TreeBuilt { generation, result } => {
                    self.on_tree_built(generation, result)
                }
                AppEvent::Run { run_id, event } => self.on_run_event(run_id, event),
                AppEvent::LspStarted { generation, result } => {
                    self.on_lsp_started(generation, result);
                }
                AppEvent::Lsp { generation, event } => self.on_lsp_event(generation, event),
                AppEvent::Db(reply) => self.on_db_reply(reply),
                AppEvent::RecentChecked(missing) => self.on_recent_checked(missing),
                AppEvent::Notice(kind, text) => self.notify(kind, text),
                AppEvent::Editor(e) => self.on_editor_event(e),
                AppEvent::ProjectInterpreters { generation, found } => {
                    self.on_project_interpreters(generation, found);
                }
            }
        }
        self.on_run_output(output);
    }

    // ------------------------------------------------------------ databases

    fn db_send(&self, command: DbCommand) {
        let sent = self
            .state
            .borrow()
            .db
            .as_ref()
            .is_some_and(|db| db.send(command));
        if sent {
            self.with_ui(|ui| ui.global::<DbApi>().set_busy(true));
        } else {
            self.notify(NoticeKind::Error, "The database worker is not running");
        }
    }

    fn driver_clicked(&self, index: i32) {
        let Some(driver) = db_panel::sorted_catalog()
            .get(usize::try_from(index).unwrap_or(usize::MAX))
            .copied()
        else {
            return;
        };
        if driver.is_available() {
            self.notify(
                NoticeKind::Info,
                format!(
                    "{} is ready: enter a database file path in the console and press Connect",
                    driver.name
                ),
            );
        } else {
            let message = match apex_db::DriverRegistry::with_builtin().get(driver.id) {
                Ok(_) => format!("{} is registered", driver.name),
                Err(e) => e.to_string(),
            };
            self.notify(NoticeKind::Info, message);
        }
    }

    fn on_db_reply(&self, reply: DbReply) {
        let Some(ui) = self.ui.upgrade() else { return };
        let db = ui.global::<DbApi>();
        db.set_busy(false);
        let set_tables = |tables: &[apex_db::TableInfo]| {
            let names: Vec<String> = tables.iter().map(|t| t.name.clone()).collect();
            let rows: Vec<DbTable> = tables
                .iter()
                .map(|t| DbTable {
                    name: t.name.as_str().into(),
                    kind: db_panel::table_kind_label(t.kind).into(),
                })
                .collect();
            db.set_tables(ModelRc::new(VecModel::from(rows)));
            self.state.borrow_mut().db_tables = names;
        };
        match reply {
            DbReply::Connected { label, tables } => {
                db.set_connected(true);
                db.set_connection_label(label.as_str().into());
                set_tables(&tables);
                db.set_result_error(false);
                db.set_result_status(format!("Connected, {} tables", tables.len()).into());
            }
            DbReply::Executed { result, tables } => {
                set_tables(&tables);
                let (columns, rows) = db_panel::result_grid(&result);
                let columns: Vec<SharedString> = columns.into_iter().map(Into::into).collect();
                let rows: Vec<ResultRow> = rows
                    .into_iter()
                    .map(|cells| {
                        let cells: Vec<SharedString> = cells.into_iter().map(Into::into).collect();
                        ResultRow {
                            cells: ModelRc::new(VecModel::from(cells)),
                        }
                    })
                    .collect();
                db.set_columns(ModelRc::new(VecModel::from(columns)));
                db.set_rows(ModelRc::new(VecModel::from(rows)));
                db.set_result_error(false);
                db.set_result_status(db_panel::result_status(&result).into());
            }
            DbReply::Disconnected => {
                db.set_connected(false);
                db.set_connection_label(SharedString::new());
                set_tables(&[]);
                db.set_columns(ModelRc::default());
                db.set_rows(ModelRc::default());
                db.set_result_error(false);
                db.set_result_status("Disconnected".into());
            }
            DbReply::Failed(message) => {
                db.set_result_error(true);
                db.set_result_status(message.as_str().into());
            }
        }
    }

    // -------------------------------------------------------------- plugins

    /// Rebuilds the Plugins panel from the manager.
    pub fn refresh_plugins(&self) {
        let (rows, commands) = {
            let mut st = self.state.borrow_mut();
            let Some(manager) = st.plugins.as_ref() else {
                return;
            };
            let views = plugins::plugin_views(manager);
            let commands = plugins::commands(manager);
            st.plugin_ids = views.iter().map(|v| v.id.clone()).collect();
            st.command_ids = commands.iter().map(|c| c.id.clone()).collect();
            let rows: Vec<PluginRow> = views
                .into_iter()
                .map(|v| PluginRow {
                    id: v.id.as_str().into(),
                    name: v.name.into(),
                    version: v.version.into(),
                    description: v.description.into(),
                    enabled: v.enabled,
                    status: v.status.into(),
                    failed: v.failed,
                    permissions: v.permissions.into(),
                    needs_grant: v.needs_grant,
                })
                .collect();
            let commands: Vec<CommandRow> = commands
                .into_iter()
                .map(|c| CommandRow {
                    id: c.id.into(),
                    title: c.title.into(),
                    plugin: c.plugin.as_str().into(),
                    keybinding: c.keybinding.unwrap_or_default().into(),
                })
                .collect();
            (rows, commands)
        };
        self.with_ui(|ui| {
            let api = ui.global::<PluginsApi>();
            api.set_plugins(ModelRc::new(VecModel::from(rows)));
            api.set_commands(ModelRc::new(VecModel::from(commands)));
        });
    }

    fn set_plugin_enabled(&self, index: i32, enabled: bool) {
        let result = {
            let mut st = self.state.borrow_mut();
            let Some(id) = st
                .plugin_ids
                .get(usize::try_from(index).unwrap_or(usize::MAX))
                .cloned()
            else {
                return;
            };
            match st.plugins.as_mut() {
                Some(manager) => manager.set_enabled(&id, enabled).map_err(|e| e.to_string()),
                None => Err("plugins are not available".into()),
            }
        };
        if let Err(e) = result {
            self.notify(NoticeKind::Error, format!("Plugin: {e}"));
        }
        self.refresh_plugins();
        self.sync_plugin_contributions();
    }

    /// Grants every permission plugin `index` requests (the user clicked
    /// "Allow" after reading them in the panel).
    fn grant_plugin_permissions(&self, index: i32) {
        let result = {
            let mut st = self.state.borrow_mut();
            let Some(id) = st
                .plugin_ids
                .get(usize::try_from(index).unwrap_or(usize::MAX))
                .cloned()
            else {
                return;
            };
            match st.plugins.as_mut() {
                Some(manager) => {
                    let requested = manager.missing_permissions(&id);
                    let all = manager
                        .granted_permissions(&id)
                        .union(&requested)
                        .copied()
                        .collect();
                    manager
                        .grant_permissions(&id, &all)
                        .map(|()| id)
                        .map_err(|e| e.to_string())
                }
                None => Err("plugins are not available".into()),
            }
        };
        match result {
            Ok(id) => self.notify(NoticeKind::Info, format!("Permissions granted to {id}")),
            Err(e) => self.notify(NoticeKind::Error, format!("Plugin: {e}")),
        }
        self.refresh_plugins();
        self.sync_plugin_contributions();
    }

    fn run_plugin_command(&self, index: i32) {
        // Handlers may post notices through the sink; those are queued, so
        // holding the borrow while the handler runs cannot re-enter.
        let result = {
            let st = self.state.borrow();
            let Some(id) = st
                .command_ids
                .get(usize::try_from(index).unwrap_or(usize::MAX))
                .cloned()
            else {
                return;
            };
            match st.plugins.as_ref() {
                Some(manager) => manager.execute_command(&id, &[]).map_err(|e| e.to_string()),
                None => Err("plugins are not available".into()),
            }
        };
        if let Err(e) = result {
            self.notify(NoticeKind::Error, format!("Command failed: {e}"));
        }
    }

    // ------------------------------------------------------------- shutdown

    /// Saves state and stops background work when the window closes.
    fn shutdown(&self) {
        self.close_workspace_now();
        if let Some(db) = self.state.borrow().db.as_ref() {
            db.send(DbCommand::Disconnect);
        }
        if let Ok(mut st) = self.state.try_borrow_mut() {
            if let Some(manager) = st.plugins.as_mut() {
                for (id, e) in manager.deactivate_all() {
                    tracing::warn!(plugin = %id, %e, "deactivating plugin");
                }
            }
        }
    }
}

/// Converts an index for Slint (`int`), saturating.
pub fn to_i32(i: usize) -> i32 {
    i32::try_from(i).unwrap_or(i32::MAX)
}

/// Appends lines to a model, dropping the oldest beyond `cap`.
///
/// When the cap is hit the model is trimmed to 90% of it in a single
/// rebuild, so a program printing without pause costs one rebuild per
/// `cap / 10` lines instead of shifting the whole model for every line.
pub fn append_capped<T: Clone + 'static>(model: &VecModel<T>, items: Vec<T>, cap: usize) {
    if model.row_count() + items.len() <= cap {
        model.extend(items);
        return;
    }
    let keep = cap - cap / 10;
    let mut rows: Vec<T> = model.iter().chain(items).collect();
    let excess = rows.len().saturating_sub(keep);
    rows.drain(..excess);
    model.set_vec(rows);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_capped_keeps_the_newest_rows_within_the_cap() {
        let model = VecModel::default();
        append_capped(&model, (0..8).collect(), 10);
        assert_eq!(model.row_count(), 8);
        append_capped(&model, vec![8, 9], 10);
        assert_eq!(model.row_count(), 10);
        // Over the cap: trimmed to 9 rows, newest last.
        append_capped(&model, vec![10], 10);
        let rows: Vec<i32> = model.iter().collect();
        assert_eq!(rows, (2..=10).collect::<Vec<_>>());
        // A batch larger than the cap keeps only its tail.
        append_capped(&model, (100..150).collect(), 10);
        let rows: Vec<i32> = model.iter().collect();
        assert_eq!(rows, (141..150).collect::<Vec<_>>());
    }
}
