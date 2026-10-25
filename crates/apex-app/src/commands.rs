//! The command layer: one registry (`apex_core::keymap`) drives the menus,
//! global shortcuts, the command palette, Go to File / Go to Line / Search
//! Everywhere, Find in Files, and the Settings / Keyboard Shortcuts / Open
//! Folder dialogs.
//!
//! Slow work (file index, content search) runs on worker threads and posts
//! back with `slint::invoke_from_event_loop`; the UI thread only filters
//! small in-memory lists.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use apex_core::keymap::{Chord, Keymap, Lookup, CATEGORIES, COMMANDS};
use apex_core::search::{self, FileIndex, FindOptions, FindResult};
use apex_core::{paths, Density};
use slint::platform::Key;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::app::{to_i32, App};
use crate::events::NoticeKind;
use crate::state::Workspace;
use crate::{
    AppApi, AppTheme, AppWindow, FindRow, MenuDef, MenuItem, PaletteItem, Segment, ShellApi,
    ShortcutRow, ToolsApi, WelcomeApi, WorkspaceApi,
};

/// Most results shown in the palette.
const PALETTE_LIMIT: usize = 60;
/// Two Shift presses within this time open Search Everywhere.
const DOUBLE_SHIFT: Duration = Duration::from_millis(400);
/// How long the first chord of a two-chord shortcut stays pending.
const CHORD_TIMEOUT: Duration = Duration::from_secs(2);
/// Bottom tool window index of Find in Files.
pub const TOOL_FIND: i32 = 4;

/// Commands shown with a separator before them in their menu.
const SEPARATOR_BEFORE: &[&str] = &[
    "file.new_file",
    "file.save",
    "file.close_tab",
    "file.settings",
    "file.exit",
    "edit.cut",
    "edit.select_all",
    "edit.find",
    "edit.find_in_files",
    "edit.toggle_comment",
    "view.project",
    "view.dock_left",
    "view.zoom_in",
    "view.toolbar",
    "nav.goto_file",
    "nav.next_problem",
    "window.split_right",
    "run.clear_output",
    "tools.refresh_project",
    "help.about",
];

/// What a palette row does when accepted.
#[derive(Debug, Clone)]
enum Target {
    Command(String),
    File(PathBuf),
    None,
}

/// The editor's own command id for a registry command it implements.
fn editor_command_id(id: &str) -> Option<&'static str> {
    Some(match id {
        "edit.undo" => "editor.undo",
        "edit.redo" => "editor.redo",
        "edit.cut" => "editor.cut",
        "edit.copy" => "editor.copy",
        "edit.paste" => "editor.paste",
        "edit.select_all" => "editor.selectAll",
        "edit.find" => "editor.find",
        "edit.find_next" => "editor.findNext",
        "edit.find_prev" => "editor.findPrevious",
        "edit.toggle_comment" => "editor.toggleComment",
        "edit.duplicate_line" => "editor.duplicateLine",
        "edit.fold" => "editor.fold",
        "edit.unfold" => "editor.unfold",
        "code.complete" => "editor.triggerSuggest",
        "code.install_packages" => "editor.installMissingPackages",
        "view.zoom_in" => "editor.zoomIn",
        "view.zoom_out" => "editor.zoomOut",
        "view.zoom_reset" => "editor.zoomReset",
        "nav.goto_line" => "editor.goToLine",
        "window.split_right" => "editor.splitRight",
        "window.split_down" => "editor.splitDown",
        "window.move_to_new_window" => "editor.moveToNewWindow",
        _ => return None,
    })
}

/// Where a Find in Files row leads.
type FindTarget = Option<(PathBuf, usize, usize)>;

struct Shell {
    app: Weak<App>,
    keymap: Keymap,
    keymap_path: Option<PathBuf>,
    pending: Option<(Chord, Instant)>,
    last_shift: Option<Instant>,
    index: Option<Arc<FileIndex>>,
    index_building: bool,
    index_serial: u64,
    palette_targets: Vec<Target>,
    palette_model: Rc<VecModel<PaletteItem>>,
    menus_model: Rc<VecModel<MenuDef>>,
    find_model: Rc<VecModel<FindRow>>,
    find_targets: Vec<FindTarget>,
    find_cancel: Arc<AtomicBool>,
    find_serial: u64,
    problem_cursor: Option<usize>,
}

thread_local! {
    static SHELL: RefCell<Option<Shell>> = const { RefCell::new(None) };
}

fn with_shell<R>(f: impl FnOnce(&mut Shell) -> R) -> Option<R> {
    SHELL.with(|s| s.borrow_mut().as_mut().map(f))
}

fn shell_app() -> Option<Rc<App>> {
    SHELL.with(|s| s.borrow().as_ref().and_then(|s| s.app.upgrade()))
}

/// The canonical key name for a Slint key event text, if bindable.
fn key_name(text: &str) -> Option<String> {
    let named: [(Key, &str); 22] = [
        (Key::Backspace, "Backspace"),
        (Key::Tab, "Tab"),
        (Key::Backtab, "Tab"),
        (Key::Return, "Enter"),
        (Key::Escape, "Escape"),
        (Key::Delete, "Delete"),
        (Key::Space, "Space"),
        (Key::UpArrow, "Up"),
        (Key::DownArrow, "Down"),
        (Key::LeftArrow, "Left"),
        (Key::RightArrow, "Right"),
        (Key::Insert, "Insert"),
        (Key::Home, "Home"),
        (Key::End, "End"),
        (Key::PageUp, "PageUp"),
        (Key::PageDown, "PageDown"),
        (Key::F1, "F1"),
        (Key::F2, "F2"),
        (Key::F3, "F3"),
        (Key::F4, "F4"),
        (Key::F5, "F5"),
        (Key::F6, "F6"),
    ];
    for (k, name) in named {
        if SharedString::from(k).as_str() == text {
            return Some(name.to_owned());
        }
    }
    let more: [(Key, &str); 6] = [
        (Key::F7, "F7"),
        (Key::F8, "F8"),
        (Key::F9, "F9"),
        (Key::F10, "F10"),
        (Key::F11, "F11"),
        (Key::F12, "F12"),
    ];
    for (k, name) in more {
        if SharedString::from(k).as_str() == text {
            return Some(name.to_owned());
        }
    }
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some('\r' | '\n'), None) => Some("Enter".into()),
        (Some(c), None) if !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(&c) => {
            Some(match c {
                // Shifted digits/symbols on US layouts map back to the key.
                '+' => "=".into(),
                '_' => "-".into(),
                ')' => "0".into(),
                '{' => "[".into(),
                '}' => "]".into(),
                '?' => "/".into(),
                ':' => ";".into(),
                _ => c.to_uppercase().collect(),
            })
        }
        _ => None,
    }
}

fn is_shift_key(text: &str) -> bool {
    text == SharedString::from(Key::Shift).as_str()
        || text == SharedString::from(Key::ShiftR).as_str()
}

/// Whether the shell takes `text` before the focused element: only the
/// second (non-modifier) key of a pending chord.
fn captures_for_chord(text: &str, chord_pending: bool) -> bool {
    chord_pending && !is_modifier_key(text)
}

fn is_modifier_key(text: &str) -> bool {
    [
        Key::Shift,
        Key::ShiftR,
        Key::Control,
        Key::ControlR,
        Key::Alt,
        Key::AltGr,
        Key::Meta,
        Key::MetaR,
        Key::CapsLock,
    ]
    .into_iter()
    .any(|k| SharedString::from(k).as_str() == text)
}

/// Splits `text` into highlighted / plain runs for the char positions.
fn segments(text: &str, positions: &[usize]) -> ModelRc<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for (i, c) in text.chars().enumerate() {
        let hit = positions.binary_search(&i).is_ok();
        match out.last_mut() {
            Some(s) if s.hit == hit => {
                let mut t = s.text.to_string();
                t.push(c);
                s.text = t.into();
            }
            _ => out.push(Segment {
                text: c.to_string().into(),
                hit,
            }),
        }
    }
    ModelRc::new(VecModel::from(out))
}

fn plain(text: &str) -> ModelRc<Segment> {
    segments(text, &[])
}

impl App {
    pub(crate) fn wire_shell(self: &Rc<Self>, ui: &AppWindow) {
        let keymap_path = paths::keymap_file().ok();
        let (keymap, warnings) = Keymap::load(keymap_path.as_deref());
        let shell = Shell {
            app: Rc::downgrade(self),
            keymap,
            keymap_path: keymap_path.clone(),
            pending: None,
            last_shift: None,
            index: None,
            index_building: false,
            index_serial: 0,
            palette_targets: Vec::new(),
            palette_model: Rc::new(VecModel::default()),
            menus_model: Rc::new(VecModel::default()),
            find_model: Rc::new(VecModel::default()),
            find_targets: Vec::new(),
            find_cancel: Arc::new(AtomicBool::new(false)),
            find_serial: 0,
            problem_cursor: None,
        };
        let api = ui.global::<ShellApi>();
        api.set_palette_items(ModelRc::from(shell.palette_model.clone()));
        api.set_menus(ModelRc::from(shell.menus_model.clone()));
        api.set_find_rows(ModelRc::from(shell.find_model.clone()));
        api.set_keymap_path(
            keymap_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
                .into(),
        );
        SHELL.with(|s| *s.borrow_mut() = Some(shell));

        {
            let st = self.state.borrow();
            api.set_toolbar_visible(st.settings.show_toolbar);
            api.set_status_visible(st.settings.show_status_bar);
            ui.global::<AppTheme>()
                .set_compact(st.settings.density == Density::Compact);
        }

        let app = self.clone();
        api.on_invoke(move |id| app.run_command(&id));
        let app = self.clone();
        api.on_menu_opening(move |_| app.refresh_menus());
        let app = self.clone();
        api.on_key(move |text, ctrl, alt, shift, meta| {
            app.shell_key(&text, ctrl, alt, shift, meta)
        });
        let app = self.clone();
        api.on_key_capture(move |text, ctrl, alt, shift, meta| {
            app.shell_key_capture(&text, ctrl, alt, shift, meta)
        });
        let app = self.clone();
        api.on_key_released(move |text| app.switcher_key_released(&text));
        let app = self.clone();
        api.on_switcher_pick(move |i| app.switcher_commit(i));
        let app = self.clone();
        api.on_palette_edited(move |text| app.palette_update(&text));
        let app = self.clone();
        api.on_palette_accept(move |i| app.palette_accept(i));
        api.on_palette_closed(|| {
            with_shell(|s| s.palette_targets.clear());
        });
        let app = self.clone();
        api.on_set_compact(move |compact| app.set_compact(compact));
        let app = self.clone();
        api.on_find_run(move |text, case| app.find_run(&text, case));
        let app = self.clone();
        api.on_find_open(move |i| app.find_open(i));

        self.refresh_menus();
        if let Some(w) = warnings.first() {
            self.notify(NoticeKind::Warning, w.clone());
        }
        self.report_conflicts();
    }

    fn report_conflicts(&self) {
        let conflicts = with_shell(|s| s.keymap.conflicts()).unwrap_or_default();
        if let Some(c) = conflicts.first() {
            self.notify(
                NoticeKind::Warning,
                format!(
                    "Shortcut {} is bound to {} commands ({}); the first one wins. See Help > Keyboard Shortcuts.",
                    c.keys,
                    c.commands.len(),
                    c.commands.join(", ")
                ),
            );
        }
    }

    fn has_workspace(&self) -> bool {
        self.state.borrow().workspace.is_some()
    }

    fn has_unsaved(&self) -> bool {
        self.state
            .borrow()
            .workspace
            .as_ref()
            .is_some_and(Workspace::has_unsaved)
    }

    /// Whether `id` can run right now.
    fn command_enabled(&self, id: &str) -> bool {
        let Some(ui) = self.ui.upgrade() else {
            return false;
        };
        let ws = ui.global::<WorkspaceApi>();
        let in_ws = ui.global::<AppApi>().get_screen() == 2 && self.has_workspace();
        let has_tab = in_ws && ws.get_active_tab() >= 0;
        let running = ui.global::<ToolsApi>().get_running();
        match id {
            "file.new_project"
            | "file.open_folder"
            | "file.settings"
            | "file.edit_keymap"
            | "file.reload_keymap"
            | "file.exit"
            | "view.command_palette"
            | "view.toolbar"
            | "view.status_bar"
            | "view.compact"
            | "help.shortcuts"
            | "help.about" => true,
            "file.save"
            | "file.close_tab"
            | "file.close_all_tabs"
            | "nav.goto_line"
            | "code.complete"
            | "code.install_packages"
            | "view.zoom_in"
            | "view.zoom_out"
            | "view.zoom_reset"
            | "window.split_right"
            | "window.split_down"
            | "window.move_to_new_window" => has_tab,
            _ if id.starts_with("edit.") && id != "edit.find_in_files" => has_tab,
            "window.next_tab" | "window.prev_tab" => in_ws && ws.get_tabs().row_count() > 1,
            "window.switcher" => in_ws && ws.get_tabs().row_count() > 0,
            "nav.next_problem" | "nav.prev_problem" => {
                in_ws && ui.global::<ToolsApi>().get_problems().row_count() > 0
            }
            "run.run" => in_ws && !running,
            "run.stop" => running,
            _ if id.starts_with("theme:") => true,
            _ => in_ws,
        }
    }

    fn command_checked(&self, id: &str) -> bool {
        let Some(ui) = self.ui.upgrade() else {
            return false;
        };
        let ws = ui.global::<WorkspaceApi>();
        let tools = ui.global::<ToolsApi>();
        let shell = ui.global::<ShellApi>();
        let tool = |i: i32| tools.get_tool_open() && tools.get_tool_tab() == i;
        match id {
            "view.project" => ws.get_tree_visible(),
            "view.dock_left" => !ws.get_tree_right(),
            "view.dock_right" => ws.get_tree_right(),
            "view.output" => tool(0),
            "view.problems" => tool(1),
            "view.databases" => tool(2),
            "view.plugins" => tool(3),
            "edit.find_in_files" => tool(TOOL_FIND),
            "view.toolbar" => shell.get_toolbar_visible(),
            "view.status_bar" => shell.get_status_visible(),
            "view.compact" => ui.global::<AppTheme>().get_compact(),
            _ => {
                if let Some(theme) = id.strip_prefix("theme:") {
                    let st = self.state.borrow();
                    return st.settings.theme == theme;
                }
                if let Some(i) = id.strip_prefix("runcfg:") {
                    return i.parse::<i32>().ok() == Some(ws.get_run_config_index());
                }
                false
            }
        }
    }

    fn shortcut_label(&self, id: &str) -> String {
        if id == "nav.search_everywhere" {
            return "Double Shift".into();
        }
        with_shell(|s| s.keymap.label(id)).unwrap_or_default()
    }

    /// Rebuilds the menu bar model from the registry and current state.
    pub(crate) fn refresh_menus(&self) {
        let theme_items: Vec<(String, String)> = {
            let st = self.state.borrow();
            st.theme_ids
                .iter()
                .map(|id| {
                    let name = st.themes.get(id).map_or(id.as_str(), |t| t.name.as_str());
                    (format!("theme:{id}"), format!("Theme: {name}"))
                })
                .collect()
        };
        let run_items: Vec<(String, String)> = self
            .ui
            .upgrade()
            .map(|ui| {
                ui.global::<WorkspaceApi>()
                    .get_run_configs()
                    .iter()
                    .enumerate()
                    .map(|(i, name)| (format!("runcfg:{i}"), format!("Configuration: {name}")))
                    .collect()
            })
            .unwrap_or_default();

        let item = |id: &str, text: &str| MenuItem {
            id: id.into(),
            text: text.into(),
            shortcut: self.shortcut_label(id).into(),
            enabled: self.command_enabled(id),
            checked: self.command_checked(id),
            separator: false,
        };
        let sep = || MenuItem {
            id: SharedString::new(),
            text: SharedString::new(),
            shortcut: SharedString::new(),
            enabled: false,
            checked: false,
            separator: true,
        };
        let menus: Vec<MenuDef> = CATEGORIES
            .iter()
            .map(|cat| {
                let mut items = Vec::new();
                for c in COMMANDS.iter().filter(|c| c.category == *cat) {
                    if SEPARATOR_BEFORE.contains(&c.id) && !items.is_empty() {
                        items.push(sep());
                    }
                    items.push(item(c.id, c.title));
                }
                let extra = match *cat {
                    "View" => &theme_items,
                    "Run" => &run_items,
                    _ => &Vec::new(),
                };
                if !extra.is_empty() {
                    items.push(sep());
                    for (id, text) in extra {
                        items.push(item(id, text));
                    }
                }
                MenuDef {
                    title: (*cat).into(),
                    items: ModelRc::new(VecModel::from(items)),
                }
            })
            .collect();
        with_shell(|s| {
            // Update rows in place so open menus keep their state.
            if s.menus_model.row_count() == menus.len() {
                for (i, m) in menus.into_iter().enumerate() {
                    s.menus_model.set_row_data(i, m);
                }
            } else {
                s.menus_model.set_vec(menus);
            }
        });
    }

    /// Sees every key before the focused element. Any key but Shift ends a
    /// double-Shift (typing "AB" in the editor must not open Search
    /// Everywhere), and while a chord is pending its second key goes to the
    /// keymap even if the editor would use it (Ctrl+K Ctrl+S).
    fn shell_key_capture(
        &self,
        text: &str,
        ctrl: bool,
        alt: bool,
        shift: bool,
        meta: bool,
    ) -> bool {
        // Ctrl+Tab opens the switcher; while it is up it takes every key.
        let switcher_open = self
            .ui
            .upgrade()
            .is_some_and(|ui| ui.global::<ShellApi>().get_switcher_open());
        if switcher_open {
            return self.switcher_key(text, ctrl, shift);
        }
        if ctrl && !alt && !meta && crate::switcher::is_tab(text) {
            return self.switcher_step(!(shift || text != "\t"));
        }
        if is_shift_key(text) {
            return false;
        }
        let now = Instant::now();
        let pending = with_shell(|s| {
            s.last_shift = None;
            s.pending
                .as_ref()
                .is_some_and(|(_, t)| now.duration_since(*t) < CHORD_TIMEOUT)
        })
        .unwrap_or(false);
        captures_for_chord(text, pending) && self.shell_key(text, ctrl, alt, shift, meta)
    }

    /// Global key handling for keys nothing focused consumed.
    fn shell_key(&self, text: &str, ctrl: bool, alt: bool, shift: bool, meta: bool) -> bool {
        if is_shift_key(text) {
            if !ctrl && !alt && !meta {
                let now = Instant::now();
                let double = with_shell(|s| {
                    let hit = s
                        .last_shift
                        .is_some_and(|t| now.duration_since(t) < DOUBLE_SHIFT);
                    s.last_shift = if hit { None } else { Some(now) };
                    hit
                })
                .unwrap_or(false);
                if double {
                    self.open_palette(3);
                    return true;
                }
            }
            return false;
        }
        with_shell(|s| s.last_shift = None);
        if is_modifier_key(text) {
            return false;
        }
        let Some(name) = key_name(text) else {
            return false;
        };
        // Backtab arrives as its own key; it means Shift+Tab.
        let shift = shift || text == SharedString::from(Key::Backtab).as_str();
        let Some(chord) = Chord::from_event(&name, ctrl, alt, shift, meta) else {
            return false;
        };
        // Plain typing keys are never shortcuts.
        let named = name.chars().count() > 1;
        if chord.is_plain() && !named {
            return false;
        }
        let now = Instant::now();
        let pending = with_shell(|s| {
            s.pending
                .take()
                .filter(|(_, t)| now.duration_since(*t) < CHORD_TIMEOUT)
                .map(|(c, _)| c)
        })
        .flatten();
        let lookup =
            with_shell(|s| s.keymap.lookup(pending.as_ref(), &chord)).unwrap_or(Lookup::None);
        self.with_ui(|ui| {
            ui.global::<ShellApi>()
                .set_pending_chord(SharedString::new())
        });
        match lookup {
            Lookup::Command(id) => {
                if self.command_enabled(&id) {
                    self.run_command(&id);
                    true
                } else {
                    // Let disabled editor commands fall through harmlessly.
                    pending.is_some()
                }
            }
            Lookup::Prefix => {
                with_shell(|s| s.pending = Some((chord.clone(), now)));
                self.with_ui(|ui| {
                    ui.global::<ShellApi>().set_pending_chord(
                        format!("({chord}) was pressed. Waiting for second key of chord...").into(),
                    );
                });
                true
            }
            Lookup::None => pending.is_some(),
        }
    }

    fn toggle_tool(&self, index: i32) {
        self.with_ui(|ui| {
            let t = ui.global::<ToolsApi>();
            if t.get_tool_open() && t.get_tool_tab() == index {
                t.set_tool_open(false);
            } else {
                t.set_tool_tab(index);
                t.set_tool_open(true);
            }
        });
    }

    fn show_tool(&self, index: i32) {
        self.with_ui(|ui| {
            let t = ui.global::<ToolsApi>();
            t.set_tool_tab(index);
            t.set_tool_open(true);
        });
    }

    fn set_compact(&self, compact: bool) {
        self.state.borrow_mut().settings.density = if compact {
            Density::Compact
        } else {
            Density::Comfortable
        };
        self.with_ui(|ui| ui.global::<AppTheme>().set_compact(compact));
        self.save_settings();
    }

    /// Runs command `id` (a registry id, `theme:<id>` or `runcfg:<n>`).
    pub(crate) fn run_command(&self, id: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        let ws_api = ui.global::<WorkspaceApi>();
        let shell = ui.global::<ShellApi>();
        if let Some(theme) = id.strip_prefix("theme:") {
            let index = self
                .state
                .borrow()
                .theme_ids
                .iter()
                .position(|t| t == theme);
            if let Some(i) = index {
                ui.global::<AppApi>().invoke_select_theme(to_i32(i));
            }
            return;
        }
        if let Some(i) = id
            .strip_prefix("runcfg:")
            .and_then(|i| i.parse::<i32>().ok())
        {
            ws_api.invoke_select_run_config(i);
            return;
        }
        match id {
            "file.new_project" => {
                if self.has_unsaved() {
                    self.notify(NoticeKind::Warning, "Save or close modified files first");
                    return;
                }
                if self.has_workspace() {
                    ws_api.invoke_close_project();
                }
                ui.global::<WelcomeApi>().invoke_new_project();
            }
            "file.open_folder" => {
                if self.has_unsaved() {
                    self.notify(NoticeKind::Warning, "Save or close modified files first");
                    return;
                }
                shell.set_open_folder_path(ui.global::<WelcomeApi>().get_open_path());
                shell.set_open_folder_open(true);
            }
            "file.new_file" | "file.new_folder" => {
                ws_api.set_tree_visible(true);
                shell.set_new_entry_kind(if id == "file.new_file" { 1 } else { 2 });
            }
            "file.save" => ws_api.invoke_save(),
            "file.save_all" => {
                self.save_all();
            }
            "file.close_tab" => ws_api.invoke_close_tab(ws_api.get_active_tab()),
            "file.close_all_tabs" => {
                for i in (0..ws_api.get_tabs().row_count()).rev() {
                    ws_api.invoke_close_tab(to_i32(i));
                    if ws_api.get_confirm_close() >= 0 {
                        break;
                    }
                }
            }
            "file.close_project" => ws_api.invoke_close_project(),
            "file.settings" => shell.set_settings_open(true),
            "file.edit_keymap" => self.edit_keymap(),
            "file.reload_keymap" => self.reload_keymap(),
            "file.exit" => {
                if self.has_unsaved() {
                    ui.global::<AppApi>().set_confirm_quit(true);
                } else {
                    let _ = ui.hide();
                }
            }
            "edit.undo" | "edit.redo" | "edit.cut" | "edit.copy" | "edit.paste"
            | "edit.select_all" | "edit.find" | "edit.find_next" | "edit.find_prev"
            | "edit.toggle_comment" | "edit.duplicate_line" | "edit.fold" | "edit.unfold"
            | "code.complete" | "code.install_packages" | "view.zoom_in" | "view.zoom_out"
            | "view.zoom_reset" | "nav.goto_line" | "window.split_right" | "window.split_down"
            | "window.move_to_new_window" => {
                if let Some(editor_id) = editor_command_id(id) {
                    self.editor_command(-1, editor_id);
                }
            }
            "edit.find_in_files" => {
                self.show_tool(TOOL_FIND);
                shell.set_find_focus(shell.get_find_focus() + 1);
            }
            "view.command_palette" => self.open_palette(0),
            "view.project" => {
                ws_api.set_tree_visible(!ws_api.get_tree_visible());
                ws_api.invoke_tree_layout_changed();
            }
            "view.dock_left" | "view.dock_right" => {
                ws_api.set_tree_right(id == "view.dock_right");
                ws_api.set_tree_visible(true);
                ws_api.invoke_tree_layout_changed();
            }
            "view.output" => self.toggle_tool(0),
            "view.problems" => self.toggle_tool(1),
            "view.databases" => self.toggle_tool(2),
            "view.plugins" => self.toggle_tool(3),
            "view.hide_tool_windows" => ui.global::<ToolsApi>().set_tool_open(false),
            "view.toolbar" | "view.status_bar" => {
                let toolbar = id == "view.toolbar";
                let on = if toolbar {
                    !shell.get_toolbar_visible()
                } else {
                    !shell.get_status_visible()
                };
                {
                    let mut st = self.state.borrow_mut();
                    if toolbar {
                        st.settings.show_toolbar = on;
                    } else {
                        st.settings.show_status_bar = on;
                    }
                }
                if toolbar {
                    shell.set_toolbar_visible(on);
                } else {
                    shell.set_status_visible(on);
                }
                self.save_settings();
            }
            "view.compact" => self.set_compact(!ui.global::<AppTheme>().get_compact()),
            "nav.search_everywhere" => self.open_palette(3),
            "nav.goto_file" => self.open_palette(1),
            "nav.search_explorer" => {
                ws_api.set_tree_visible(true);
                shell.set_explorer_focus(shell.get_explorer_focus() + 1);
            }
            "nav.next_problem" | "nav.prev_problem" => {
                let n = ui.global::<ToolsApi>().get_problems().row_count();
                if n == 0 {
                    return;
                }
                let next = with_shell(|s| {
                    let i = match (s.problem_cursor, id == "nav.next_problem") {
                        (None, true) => 0,
                        (None, false) => n - 1,
                        (Some(c), true) => (c + 1) % n,
                        (Some(c), false) => (c + n - 1) % n,
                    };
                    s.problem_cursor = Some(i);
                    i
                })
                .unwrap_or(0);
                ui.global::<ToolsApi>().invoke_open_problem(to_i32(next));
            }
            "run.run" => {
                self.show_tool(0);
                ui.global::<ToolsApi>().invoke_run();
            }
            "run.stop" => ui.global::<ToolsApi>().invoke_stop(),
            "run.clear_output" => ui.global::<ToolsApi>().invoke_clear_output(),
            "tools.databases" => self.show_tool(2),
            "tools.plugins" => self.show_tool(3),
            "tools.refresh_project" => {
                ws_api.invoke_refresh_tree();
                invalidate_file_index();
            }
            "window.next_tab" | "window.prev_tab" => {
                let n = to_i32(ws_api.get_tabs().row_count());
                if n > 1 {
                    let step = if id == "window.next_tab" { 1 } else { n - 1 };
                    ws_api.invoke_select_tab((ws_api.get_active_tab().max(0) + step) % n);
                }
            }
            "window.switcher" => {
                self.switcher_step(true);
            }
            "help.shortcuts" => self.open_shortcuts(),
            "help.about" => self.notify(
                NoticeKind::Info,
                format!(
                    "Apex IDE {}: a native Python IDE in Rust and Slint. Press Ctrl+Shift+P for all commands.",
                    env!("CARGO_PKG_VERSION")
                ),
            ),
            other => tracing::warn!("unknown command {other}"),
        }
    }

    // ------------------------------------------------------------ keymap

    fn edit_keymap(&self) {
        let Some(path) = with_shell(|s| s.keymap_path.clone()).flatten() else {
            self.notify(
                NoticeKind::Warning,
                "The settings folder is unknown on this system",
            );
            return;
        };
        if !path.exists() {
            let written = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&path, Keymap::template()));
            if let Err(e) = written {
                self.notify(
                    NoticeKind::Error,
                    format!("Cannot create {}: {e}", path.display()),
                );
                return;
            }
        }
        if self.has_workspace() {
            self.open_file(&path);
            self.notify(
                NoticeKind::Info,
                "Edit and save keymap.toml, then run File > Reload Keymap",
            );
        } else {
            self.notify(
                NoticeKind::Info,
                format!("Keymap file: {} (edit it, then reload)", path.display()),
            );
        }
    }

    fn reload_keymap(&self) {
        let path = with_shell(|s| s.keymap_path.clone()).flatten();
        let (keymap, warnings) = Keymap::load(path.as_deref());
        with_shell(|s| s.keymap = keymap);
        self.refresh_menus();
        if let Some(w) = warnings.first() {
            self.notify(NoticeKind::Warning, w.clone());
        } else {
            self.notify(NoticeKind::Success, "Keymap reloaded");
        }
        self.report_conflicts();
    }

    fn open_shortcuts(&self) {
        let rows: Vec<ShortcutRow> = with_shell(|s| {
            let conflicts = s.keymap.conflicts();
            let mut rows = Vec::new();
            for cat in CATEGORIES {
                rows.push(ShortcutRow {
                    category: cat.into(),
                    title: cat.into(),
                    keys: SharedString::new(),
                    conflict: false,
                    header: true,
                });
                for c in COMMANDS.iter().filter(|c| c.category == cat) {
                    let keys: Vec<String> = if c.id == "nav.search_everywhere" {
                        vec!["Double Shift".into()]
                    } else {
                        s.keymap
                            .keys(c.id)
                            .iter()
                            .map(ToString::to_string)
                            .collect()
                    };
                    let conflict = conflicts
                        .iter()
                        .any(|x| x.commands.iter().any(|id| id == c.id));
                    rows.push(ShortcutRow {
                        category: cat.into(),
                        title: c.title.trim_end_matches("...").into(),
                        keys: keys.join("  ·  ").into(),
                        conflict,
                        header: false,
                    });
                }
            }
            rows
        })
        .unwrap_or_default();
        self.with_ui(|ui| {
            let shell = ui.global::<ShellApi>();
            shell.set_shortcuts(ModelRc::new(VecModel::from(rows)));
            shell.set_shortcuts_open(true);
        });
    }

    // ----------------------------------------------------------- palette

    /// Opens the palette: 0 commands, 1 files, 2 go to line, 3 everywhere.
    pub(crate) fn open_palette(&self, mode: i32) {
        if mode == 2 && !self.command_enabled("file.save") {
            return;
        }
        if (mode == 1 || mode == 3) && self.has_workspace() {
            self.ensure_file_index();
        }
        self.with_ui(|ui| {
            let shell = ui.global::<ShellApi>();
            shell.set_palette_mode(mode);
            shell.set_palette_text(SharedString::new());
            shell.set_palette_index(0);
            shell.set_palette_open(true);
            shell.set_palette_serial(shell.get_palette_serial() + 1);
        });
        self.palette_update("");
    }

    fn command_rows(&self, query: &str, limit: usize) -> Vec<(i64, PaletteItem, Target)> {
        let mut entries: Vec<(String, String, &str)> = COMMANDS
            .iter()
            .filter(|c| c.id != "view.command_palette")
            .map(|c| {
                (
                    c.id.to_owned(),
                    c.title.trim_end_matches("...").to_owned(),
                    c.category,
                )
            })
            .collect();
        {
            let st = self.state.borrow();
            for id in &st.theme_ids {
                let name = st.themes.get(id).map_or(id.as_str(), |t| t.name.as_str());
                entries.push((format!("theme:{id}"), format!("Theme: {name}"), "View"));
            }
        }
        let mut rows: Vec<(i64, PaletteItem, Target)> = entries
            .into_iter()
            .filter(|(id, _, _)| self.command_enabled(id))
            .filter_map(|(id, title, cat)| {
                let m = search::fuzzy_match(query, &title)?;
                let item = PaletteItem {
                    title: segments(&title, &m.positions),
                    detail: cat.into(),
                    shortcut: self.shortcut_label(&id).into(),
                    icon: "command".into(),
                    header: false,
                };
                Some((m.score, item, Target::Command(id)))
            })
            .collect();
        if !query.trim().is_empty() {
            rows.sort_by_key(|r| std::cmp::Reverse(r.0));
        }
        rows.truncate(limit);
        rows
    }

    fn file_rows(&self, query: &str, limit: usize) -> Option<Vec<(i64, PaletteItem, Target)>> {
        let index = with_shell(|s| s.index.clone()).flatten()?;
        Some(
            index
                .search(query, limit)
                .into_iter()
                .filter_map(|hit| {
                    let e = index.entries.get(hit.entry)?;
                    let dir = e.rel[..e.name_start].trim_end_matches('/');
                    let item = PaletteItem {
                        title: segments(e.name(), &hit.name_positions),
                        detail: dir.into(),
                        shortcut: SharedString::new(),
                        icon: crate::tree_view::file_icon_kind(e.name(), false).into(),
                        header: false,
                    };
                    Some((hit.score, item, Target::File(index.root.join(&e.rel))))
                })
                .collect(),
        )
    }

    fn palette_update(&self, query: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        let shell = ui.global::<ShellApi>();
        let mode = shell.get_palette_mode();
        let header = |t: &str| PaletteItem {
            title: plain(t),
            detail: SharedString::new(),
            shortcut: SharedString::new(),
            icon: SharedString::new(),
            header: true,
        };
        let mut items: Vec<PaletteItem> = Vec::new();
        let mut targets: Vec<Target> = Vec::new();
        let mut status = String::new();
        let indexing = with_shell(|s| s.index_building && s.index.is_none()).unwrap_or(false);
        match mode {
            1 => match self.file_rows(query, PALETTE_LIMIT) {
                Some(rows) => {
                    status = format!("{} files", rows.len());
                    for (_, item, t) in rows {
                        items.push(item);
                        targets.push(t);
                    }
                }
                None if indexing || self.has_workspace() => {
                    status = "Indexing project files...".into()
                }
                None => status = "Open a project to search its files".into(),
            },
            2 => {
                status = "Type a line number (or line:column) and press Enter".into();
                let n = query.trim();
                if !n.is_empty() {
                    let ok = n
                        .split(':')
                        .all(|p| p.trim().parse::<usize>().is_ok_and(|v| v > 0));
                    items.push(PaletteItem {
                        title: plain(&if ok {
                            format!("Go to line {n}")
                        } else {
                            "Not a line number".into()
                        }),
                        detail: SharedString::new(),
                        shortcut: "Enter".into(),
                        icon: "command".into(),
                        header: false,
                    });
                    targets.push(if ok {
                        Target::Command(format!("line:{n}"))
                    } else {
                        Target::None
                    });
                }
            }
            3 => {
                if let Some(rows) = self.file_rows(query, 12) {
                    if !rows.is_empty() {
                        items.push(header("Files"));
                        targets.push(Target::None);
                    }
                    for (_, item, t) in rows {
                        items.push(item);
                        targets.push(t);
                    }
                } else if self.has_workspace() {
                    status = "Indexing project files...".into();
                }
                let cmds = self.command_rows(query, 12);
                if !cmds.is_empty() {
                    items.push(header("Actions"));
                    targets.push(Target::None);
                }
                for (_, item, t) in cmds {
                    items.push(item);
                    targets.push(t);
                }
            }
            _ => {
                for (_, item, t) in self.command_rows(query, usize::MAX) {
                    items.push(item);
                    targets.push(t);
                }
                status = format!("{} commands", items.len());
            }
        }
        if items.is_empty() && status.is_empty() {
            status = "Nothing found".into();
        }
        let first = targets
            .iter()
            .position(|t| !matches!(t, Target::None))
            .unwrap_or(0);
        with_shell(|s| {
            s.palette_targets = targets;
            s.palette_model.set_vec(items);
        });
        shell.set_palette_index(to_i32(first));
        shell.set_palette_status(status.into());
    }

    fn palette_accept(&self, index: i32) {
        let target = with_shell(|s| {
            usize::try_from(index)
                .ok()
                .and_then(|i| s.palette_targets.get(i).cloned())
        })
        .flatten();
        let Some(target) = target else { return };
        if matches!(target, Target::None) {
            return;
        }
        self.with_ui(|ui| ui.global::<ShellApi>().set_palette_open(false));
        with_shell(|s| s.palette_targets.clear());
        match target {
            Target::Command(id) => {
                if !id.starts_with("line:") {
                    self.with_ui(|ui| {
                        let shell = ui.global::<ShellApi>();
                        shell.set_refocus(shell.get_refocus() + 1);
                    });
                }
                if let Some(spec) = id.strip_prefix("line:") {
                    let mut parts = spec
                        .split(':')
                        .map(|p| p.trim().parse::<usize>().unwrap_or(1));
                    let line = parts.next().unwrap_or(1).saturating_sub(1);
                    let col = parts.next().unwrap_or(1).saturating_sub(1);
                    self.editor_goto_utf16(line, col);
                } else {
                    self.run_command(&id);
                }
            }
            Target::File(path) => self.open_file(&path),
            Target::None => {}
        }
    }

    // -------------------------------------------------------- file index

    /// Starts building the project file index on a worker if there is none
    /// for the open project.
    pub(crate) fn ensure_file_index(&self) {
        let root = self
            .state
            .borrow()
            .workspace
            .as_ref()
            .map(|w| w.project.root.clone());
        let Some(root) = root else { return };
        let start = with_shell(|s| {
            let stale = s.index.as_ref().is_some_and(|i| i.root != root);
            if stale {
                s.index = None;
            }
            if s.index.is_some() || s.index_building {
                return None;
            }
            s.index_building = true;
            s.index_serial += 1;
            Some(s.index_serial)
        })
        .flatten();
        let Some(serial) = start else { return };
        let spawned = std::thread::Builder::new()
            .name("file-index".into())
            .spawn(move || {
                let index = FileIndex::build(&root).map_err(|e| e.to_string());
                let _ = slint::invoke_from_event_loop(move || {
                    let fresh = with_shell(|s| {
                        if s.index_serial != serial {
                            return false;
                        }
                        s.index_building = false;
                        match index {
                            Ok(i) => {
                                s.index = Some(Arc::new(i));
                                true
                            }
                            Err(e) => {
                                tracing::warn!("file index failed: {e}");
                                false
                            }
                        }
                    })
                    .unwrap_or(false);
                    if fresh {
                        if let Some(app) = shell_app() {
                            app.on_index_ready();
                        }
                    }
                });
            });
        if spawned.is_err() {
            with_shell(|s| s.index_building = false);
        }
    }

    fn on_index_ready(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let shell = ui.global::<ShellApi>();
        if shell.get_palette_open() && matches!(shell.get_palette_mode(), 1 | 3) {
            let text = shell.get_palette_text();
            self.palette_update(&text);
        }
    }

    /// Screenshot mode: types `q` into whichever search box is open.
    pub(crate) fn screenshot_query(&self, q: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        let shell = ui.global::<ShellApi>();
        if shell.get_palette_open() {
            shell.set_palette_text(q.into());
            self.palette_update(q);
        } else {
            shell.set_find_text(q.into());
            self.find_run(q, false);
        }
    }

    // ---------------------------------------------------- find in files

    fn find_run(&self, text: &str, case_sensitive: bool) {
        let root = self
            .state
            .borrow()
            .workspace
            .as_ref()
            .map(|w| w.project.root.clone());
        let Some(root) = root else { return };
        let cancel = Arc::new(AtomicBool::new(false));
        let (serial, index) = with_shell(|s| {
            s.find_cancel.store(true, Ordering::Relaxed);
            s.find_cancel = cancel.clone();
            s.find_serial += 1;
            (s.find_serial, s.index.clone().filter(|i| i.root == root))
        })
        .unwrap_or((0, None));
        let needle = text.to_owned();
        if needle.is_empty() {
            with_shell(|s| {
                s.find_model.set_vec(Vec::new());
                s.find_targets.clear();
            });
            self.with_ui(|ui| {
                let shell = ui.global::<ShellApi>();
                shell.set_find_busy(false);
                shell.set_find_status(SharedString::new());
            });
            return;
        }
        self.with_ui(|ui| {
            let shell = ui.global::<ShellApi>();
            shell.set_find_busy(true);
            shell.set_find_status(format!("Searching for \"{needle}\"...").into());
        });
        let options = FindOptions {
            case_sensitive,
            ..FindOptions::default()
        };
        let _ = std::thread::Builder::new()
            .name("find-in-files".into())
            .spawn(move || {
                let index = match index {
                    Some(i) => i,
                    None => match FileIndex::build(&root) {
                        Ok(i) => Arc::new(i),
                        Err(e) => {
                            tracing::warn!("find in files: {e}");
                            return;
                        }
                    },
                };
                let result = search::find_in_files(&index, &needle, &options, &cancel);
                if result.cancelled {
                    return;
                }
                let root = index.root.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let current = with_shell(|s| s.find_serial == serial).unwrap_or(false);
                    if current {
                        if let Some(app) = shell_app() {
                            app.on_find_done(&root, &needle, result);
                        }
                    }
                });
            });
    }

    fn on_find_done(&self, root: &Path, needle: &str, result: FindResult) {
        let mut rows = Vec::new();
        let mut targets: Vec<FindTarget> = Vec::new();
        for f in &result.files {
            let (dir, name) = f.rel.rsplit_once('/').unwrap_or(("", f.rel.as_str()));
            rows.push(FindRow {
                kind: 0,
                text: name.into(),
                detail: dir.into(),
                icon: crate::tree_view::file_icon_kind(name, false).into(),
                count: to_i32(f.hits.len()),
            });
            let path = root.join(&f.rel);
            targets.push(f.hits.first().map(|h| (path.clone(), h.line, h.col)));
            for h in &f.hits {
                rows.push(FindRow {
                    kind: 1,
                    text: h.preview.as_str().into(),
                    detail: (h.line + 1).to_string().into(),
                    icon: SharedString::new(),
                    count: 0,
                });
                targets.push(Some((path.clone(), h.line, h.col)));
            }
        }
        let status = if result.total == 0 {
            format!("No matches for \"{needle}\"")
        } else {
            format!(
                "{}{} matches in {} files",
                result.total,
                if result.truncated { "+" } else { "" },
                result.files.len()
            )
        };
        with_shell(|s| {
            s.find_model.set_vec(rows);
            s.find_targets = targets;
        });
        self.with_ui(|ui| {
            let shell = ui.global::<ShellApi>();
            shell.set_find_busy(false);
            shell.set_find_status(status.into());
        });
    }

    fn find_open(&self, row: i32) {
        let target = with_shell(|s| {
            usize::try_from(row)
                .ok()
                .and_then(|i| s.find_targets.get(i).cloned())
                .flatten()
        })
        .flatten();
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
            // Find in Files reports char columns (not UTF-16 units).
            self.editor_goto_line(self.focused_pane(), line, Some(col));
        }
    }
}

/// Drops the cached file index (the project files changed); the next Go to
/// File rebuilds it on a worker.
pub fn invalidate_file_index() {
    let rebuild = with_shell(|s| {
        s.index = None;
        s.index_building = false;
        s.index_serial += 1;
        s.app.upgrade()
    })
    .flatten();
    if let Some(app) = rebuild {
        app.ensure_file_index();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_cover_letters_symbols_and_named_keys() {
        assert_eq!(key_name("p").as_deref(), Some("P"));
        assert_eq!(key_name("+").as_deref(), Some("="));
        assert_eq!(
            key_name(SharedString::from(Key::F4).as_str()).as_deref(),
            Some("F4")
        );
        assert_eq!(
            key_name(SharedString::from(Key::F12).as_str()).as_deref(),
            Some("F12")
        );
        assert_eq!(key_name("\t").as_deref(), Some("Tab"));
        assert_eq!(key_name("ab"), None);
        assert!(is_modifier_key(SharedString::from(Key::Control).as_str()));
        assert!(is_shift_key(SharedString::from(Key::Shift).as_str()));
    }

    /// The Slint key event text for a canonical key name.
    fn event_text(key: &str) -> String {
        match key {
            "Tab" => "\t".to_owned(),
            "Space" => " ".to_owned(),
            "PageUp" => SharedString::from(Key::PageUp).to_string(),
            "PageDown" => SharedString::from(Key::PageDown).to_string(),
            k if k.starts_with('F') && k.len() > 1 => {
                let n: u32 = k[1..].parse().unwrap();
                let f = [
                    Key::F1,
                    Key::F2,
                    Key::F3,
                    Key::F4,
                    Key::F5,
                    Key::F6,
                    Key::F7,
                    Key::F8,
                    Key::F9,
                    Key::F10,
                    Key::F11,
                    Key::F12,
                ][n as usize - 1];
                SharedString::from(f).to_string()
            }
            k => k.to_lowercase(),
        }
    }

    #[test]
    fn every_registry_shortcut_resolves_from_a_key_event() {
        // Each default single-chord binding must be producible by key_name.
        for c in COMMANDS {
            for k in c.keys {
                let seq = apex_core::keymap::KeySeq::parse(k).unwrap();
                for chord in &seq.0 {
                    let text = event_text(&chord.key);
                    assert_eq!(key_name(&text).as_deref(), Some(chord.key.as_str()), "{k}");
                }
            }
        }
    }

    #[test]
    fn the_editor_never_swallows_window_shortcuts() {
        // A focused editor sees keys first. Every default shortcut of a
        // command the editor does not implement itself must bubble up
        // (Alt+1, Ctrl+Shift+P, Ctrl+Alt+S, Ctrl+PageDown, ...).
        for c in COMMANDS {
            if editor_command_id(c.id).is_some() || matches!(c.id, "file.save" | "run.run") {
                continue;
            }
            for k in c.keys {
                let seq = apex_core::keymap::KeySeq::parse(k).unwrap();
                let chord = &seq.0[0];
                let text = event_text(&chord.key);
                assert_eq!(
                    crate::editor::keys::map_key(&text, chord.ctrl, chord.shift, chord.alt),
                    None,
                    "{} ({k}) is swallowed by the editor",
                    c.id
                );
            }
        }
    }

    #[test]
    fn only_a_pending_chord_captures_keys() {
        assert!(captures_for_chord("s", true));
        assert!(!captures_for_chord("s", false));
        assert!(!captures_for_chord(
            SharedString::from(Key::Control).as_str(),
            true
        ));
    }

    #[test]
    fn segments_merge_runs() {
        let m = segments("main.py", &[0, 1, 5]);
        let runs: Vec<(String, bool)> = m.iter().map(|s| (s.text.to_string(), s.hit)).collect();
        assert_eq!(
            runs,
            [
                ("ma".to_owned(), true),
                ("in.".to_owned(), false),
                ("p".to_owned(), true),
                ("y".to_owned(), false)
            ]
        );
    }
}
