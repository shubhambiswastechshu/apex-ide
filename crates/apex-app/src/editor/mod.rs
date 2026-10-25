//! The code editor controller: owns the per-tab view state (caret, scroll,
//! highlight cache, folds, snippet session), handles keys and mouse from
//! `EditorApi`, runs completion, and pushes the visible rows to Slint.
//!
//! The buffer ([`apex_editor::Buffer`]) stays the source of truth: every
//! edit goes through `apex_editor::commands`, then the view is re-rendered
//! from the buffer. Only rows on screen are sent to the UI, in a ring of
//! slots so scrolling by one row updates one slot.

// Line lists are passed as slices of ranges; a one-range slice is intended.
#![allow(clippy::single_range_in_vec_init)]

pub mod complete;
pub mod groups;
pub mod keys;
pub mod layout;
pub mod packages;
pub mod view;

use std::collections::BTreeSet;
use std::rc::{Rc, Weak};
use std::time::Duration;

use apex_editor::commands;
use apex_editor::cursor::{
    self, index_at_visual, line_col, matching_bracket, move_cursor, visual_col, word_before,
    word_range_at, Cursor, Motion, Selection,
};
use apex_editor::outline::FoldRange;
use apex_editor::snippet::{self, SnippetDef, SnippetSession, Variables};
use apex_editor::{Highlighter, Language, LineEdit, TokenKind};
use apex_lsp::path_to_uri;
use slint::{
    ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel,
};

use crate::app::{to_i32, App};
use crate::events::NoticeKind;
use crate::state::{Tab, Workspace};
use crate::{EdCompletion, EdLine, EdNavItem, EdPane, EdRun, EdSpan, EditorApi, WorkspaceApi};

use self::complete::{Popup, VISIBLE_ROWS};
use self::keys::Action;
use self::view::{FoldMap, LineCache, Outline, Span, Viewport};

/// Rows rendered above and below the visible ones.
const OVERSCAN: usize = 12;
/// Pause in typing after which deferred work runs (language server sync,
/// outline, package scan).
const IDLE_MS: u64 = 300;
/// Bracket matching gives up after this many chars.
const BRACKET_SCAN: usize = 20_000;

/// Editor state of one tab.
#[derive(Default)]
pub struct TabView {
    /// Caret and selection.
    pub cursor: Cursor,
    /// Scroll position.
    pub scroll: (f32, f32),
    /// Highlighted lines.
    pub cache: LineCache,
    /// Folds, indent guides, scopes.
    pub outline: Outline,
    /// Collapsed folds (header lines).
    pub folded: BTreeSet<usize>,
    /// Revision `folded` is in step with.
    pub folded_rev: u64,
    /// Active snippet tab stops.
    pub snippet: Option<SnippetSession>,
    /// Revision `snippet` is in step with.
    pub snippet_rev: u64,
    /// Created on first render.
    pub highlighter: Option<Highlighter>,
    /// The text changed since the language server last got it.
    pub lsp_dirty: bool,
}

/// UI-side state of one editor pane.
pub struct PaneUi {
    lines: Rc<VecModel<EdLine>>,
    slots: Vec<Option<EdLineKey>>,
    spans: Rc<VecModel<EdSpan>>,
    /// Rows of the completion popup, updated in place (never replaced, so
    /// the UI keeps its row elements while the list is filtered).
    popup_rows: Rc<VecModel<EdCompletion>>,
    viewport: Viewport,
    /// Scroll target asked for; viewport reports are ignored until reached.
    pending_scroll: Option<(f32, f32)>,
    popup: Option<Popup>,
    caret_serial: i32,
    scroll_serial: i32,
    scroll_to: (f32, f32),
    focus_serial: i32,
    dragging: bool,
    minimap_key: Option<(u64, usize, usize, usize)>,
    /// Set while rendering after a pause in typing.
    pub(crate) idle_render: bool,
    /// Set while rendering right after an edit.
    pub(crate) typing: bool,
    /// This pane receives the keys (its caret is the tab's caret).
    pub(crate) has_keys: bool,
    /// Caret kept for this pane while another pane has the keys.
    pub(crate) cursor: Option<Cursor>,
    /// Package offered by the quick fix on the caret line.
    quickfix: Option<String>,
    minimap: Image,
    minimap_first: usize,
    shown: Option<std::path::PathBuf>,
    /// The editor group this pane shows.
    pub(crate) group: layout::GroupId,
    /// Where the group sits in the editor area.
    pub(crate) rect: layout::Rect,
    /// Find / go-to-line bar: 0 hidden, 1 find, 2 go to line.
    prompt_mode: i32,
    prompt_text: String,
    prompt_info: String,
    prompt_serial: i32,
    /// Matches of the find text (char ranges) and the revision they are for.
    matches: Vec<std::ops::Range<usize>>,
    matches_rev: Option<u64>,
}

#[derive(Clone, PartialEq)]
struct EdLineKey {
    row: usize,
    line: usize,
    fold: i32,
    guides: i32,
    runs: Vec<view::RunData>,
}

impl Default for PaneUi {
    fn default() -> Self {
        Self {
            lines: Rc::new(VecModel::default()),
            slots: Vec::new(),
            spans: Rc::new(VecModel::default()),
            popup_rows: Rc::new(VecModel::default()),
            viewport: Viewport::default(),
            pending_scroll: None,
            popup: None,
            caret_serial: 0,
            scroll_serial: 0,
            scroll_to: (0.0, 0.0),
            focus_serial: 0,
            dragging: false,
            minimap_key: None,
            idle_render: false,
            typing: false,
            has_keys: false,
            cursor: None,
            quickfix: None,
            minimap: Image::default(),
            minimap_first: 0,
            shown: None,
            group: 0,
            rect: layout::Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
            prompt_mode: 0,
            prompt_text: String::new(),
            prompt_info: String::new(),
            prompt_serial: 0,
            matches: Vec::new(),
            matches_rev: None,
        }
    }
}

/// Editor state shared by all panes.
pub struct EditorUi {
    panes_model: Rc<VecModel<EdPane>>,
    panes: Vec<PaneUi>,
    /// Built-in plus plugin snippets.
    pub snippets: Vec<SnippetDef>,
    clipboard: Option<arboard::Clipboard>,
    /// Text last copied as a whole line (pasted above the caret line).
    line_copy: Option<String>,
    next_request: u64,
    /// Keywords, builtins, snippets and file identifiers ready for the
    /// completion popup.
    index: apex_editor::completion::PythonIndex,
    /// `(file, revision)` whose identifiers are being scanned off-thread.
    scanning: Option<(u64, u64)>,
    app: Weak<App>,
    /// Editor groups and splits.
    pub(crate) layout: layout::EditorLayout,
    /// Editor area position and size in the main window.
    pub(crate) area: (f32, f32, f32, f32),
    /// Splitters from the last layout pass.
    pub(crate) splitters: Vec<layout::Splitter>,
    /// Detached editor windows by group.
    pub(crate) windows: Vec<(layout::GroupId, crate::EditorWindow)>,
    /// Orders language server traffic off the UI thread.
    pub(crate) lsp_worker: Option<crate::lsp::LspWorker>,
    idle: u64,
}

impl Default for EditorUi {
    fn default() -> Self {
        Self {
            panes_model: Rc::new(VecModel::default()),
            panes: Vec::new(),
            snippets: snippet::builtin_python_snippets(),
            index: apex_editor::completion::PythonIndex::new(&snippet::builtin_python_snippets()),
            scanning: None,
            clipboard: None,
            line_copy: None,
            next_request: 0,
            app: Weak::new(),
            layout: layout::EditorLayout::default(),
            area: (0.0, 0.0, 800.0, 600.0),
            splitters: Vec::new(),
            windows: Vec::new(),
            lsp_worker: None,
            idle: 0,
        }
    }
}

/// Results from worker threads for the editor.
pub enum EditorEvent {
    /// A completion request finished.
    Completion(complete::CompletionReply),
    /// A line of package installer output.
    InstallOutput(String),
    /// The package install finished.
    InstallDone(Result<(), String>),
    /// A background scan of a file's identifiers for completion finished.
    Identifiers {
        /// `file_key` of the file.
        file: u64,
        /// Buffer revision that was scanned.
        revision: u64,
        /// What was found.
        scan: apex_editor::completion::IdentScan,
    },
}

/// Everything a render needs from the state, computed under one borrow.
struct Frame {
    pane: EdPane,
    slot_updates: Vec<(usize, EdLine)>,
    slot_count: usize,
    /// The slot bookkeeping was reset (another file, a resize): every
    /// model row must be blanked, or rows the new file does not fill keep
    /// showing the old file's lines.
    reset_lines: bool,
    spans: Vec<EdSpan>,
    lines_model: Rc<VecModel<EdLine>>,
    spans_model: Rc<VecModel<EdSpan>>,
    popup_rows: Vec<EdCompletion>,
    popup_model: Rc<VecModel<EdCompletion>>,
    panes_model: Rc<VecModel<EdPane>>,
    index: usize,
    caret_text: String,
}

/// Makes `model` hold `rows`, touching only the rows that changed.
fn sync_rows<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.row_count() != rows.len() {
        model.set_vec(rows);
        return;
    }
    for (i, row) in rows.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&row) {
            model.set_row_data(i, row);
        }
    }
}

fn token_index(kind: TokenKind) -> i32 {
    TokenKind::ALL
        .iter()
        .position(|k| *k == kind)
        .map_or(12, to_i32)
}

fn completion_kind_index(kind: apex_editor::completion::CompletionKind) -> i32 {
    use apex_editor::completion::CompletionKind as K;
    match kind {
        K::Keyword => 0,
        K::Snippet => 1,
        K::Function => 2,
        K::Class => 3,
        K::Module => 4,
        K::Variable => 5,
        K::Constant => 6,
        K::Property => 7,
        K::Text => 8,
    }
}

/// Fold ranges currently collapsed in `tab`.
fn folded_ranges(tab: &Tab) -> Vec<FoldRange> {
    tab.view
        .folded
        .iter()
        .filter_map(|l| tab.view.outline.fold_at(*l))
        .collect()
}

fn fold_map(tab: &Tab) -> FoldMap {
    FoldMap::new(tab.buffer.len_lines(), &folded_ranges(tab))
}

/// Brings folds, snippet stops and the outline of `tab` in step with its
/// buffer (highlighting happens per render, for the lines shown).
fn sync_tab(tab: &mut Tab) {
    let b = &tab.buffer;
    let v = &mut tab.view;
    let rev = b.revision();
    if v.folded_rev != rev {
        match b.line_edits_since(v.folded_rev) {
            Some(edits) => {
                for e in &edits {
                    v.folded = view::shift_lines(&v.folded, e);
                }
            }
            None => v.folded.clear(),
        }
        v.folded_rev = rev;
    }
    if v.snippet.is_some() && v.snippet_rev != rev {
        let edits: Vec<LineEdit> = b.line_edits_since(v.snippet_rev).unwrap_or_default();
        let ok = !edits.is_empty()
            && edits.iter().all(|e| {
                v.snippet
                    .as_mut()
                    .is_some_and(|s| s.on_edit(e.start, e.removed_chars, e.inserted_chars))
            });
        if !ok {
            v.snippet = None;
        }
        v.snippet_rev = rev;
    }
    // The outline (folds, scopes) is recomputed when typing pauses (see
    // `App::on_idle`); only a never-built outline is built here.
    // Collapsed folds need current fold ranges, so with folds the outline
    // is kept up to date on every edit.
    if v.outline.revision.is_none() || (!v.folded.is_empty() && v.outline.revision != Some(rev)) {
        v.outline.update(b);
    }
    if v.outline.revision == Some(rev) {
        v.folded.retain(|l| v.outline.fold_at(*l).is_some());
    }
    if v.highlighter.is_none() {
        match Highlighter::new(b.language()) {
            Ok(h) => v.highlighter = Some(h),
            Err(e) => tracing::warn!(%e, "highlighter unavailable"),
        }
    }
}

/// Highlights `lines` of `tab` if needed.
fn ensure_highlight(tab: &mut Tab, lines: &[std::ops::Range<usize>]) {
    if let Some(h) = tab.view.highlighter.as_mut() {
        tab.view.cache.ensure(&tab.buffer, h, lines);
    }
}

impl App {
    /// Connects the `EditorApi` callbacks.
    pub(crate) fn wire_editor(self: &Rc<Self>, ui: &crate::AppWindow) {
        {
            let mut st = self.state.borrow_mut();
            st.editor.app = Rc::downgrade(self);
            if st.editor.panes.is_empty() {
                st.editor.panes.push(PaneUi::default());
                st.editor.panes_model.push(EdPane::default());
            }
            let model = st.editor.panes_model.clone();
            drop(st);
            ui.global::<EditorApi>().set_panes(ModelRc::from(model));
        }
        self.load_plugin_snippets();
        self.wire_api(&ui.global::<EditorApi>());
    }

    /// Connects the callbacks of one window's `EditorApi` (the main
    /// window and detached editor windows share the handlers).
    pub(crate) fn wire_api(self: &Rc<Self>, api: &EditorApi<'_>) {
        let steps: Vec<i32> = apex_core::ZOOM_STEPS
            .iter()
            .map(|&z| i32::try_from(z).unwrap_or(100))
            .collect();
        api.set_zoom_steps(ModelRc::new(VecModel::from(steps)));
        let app = self.clone();
        api.on_area_resized(move |x, y, w, h| app.area_resized(x, y, w, h));
        let app = self.clone();
        api.on_tab_clicked(move |pane, tab| app.group_tab_clicked(pane, tab));
        let app = self.clone();
        api.on_tab_close(move |pane, tab| app.group_tab_close(pane, tab));
        let app = self.clone();
        api.on_tab_drag(move |pane, tab, x, y, released| app.tab_drag(pane, tab, x, y, released));
        let app = self.clone();
        api.on_splitter_drag(move |i, delta| app.splitter_drag(i, delta));
        let app = self.clone();
        api.on_group_action(move |pane, action| app.group_action(pane, &action));
        let app = self.clone();
        api.on_viewport(move |pane, sx, sy, w, h, cw, lh| {
            app.editor_viewport(
                pane,
                Viewport {
                    sx,
                    sy,
                    width: w,
                    height: h,
                    char_width: cw,
                    line_height: lh,
                },
            );
        });
        let app = self.clone();
        api.on_key(move |pane, text, ctrl, shift, alt| {
            app.editor_key(pane, &text, ctrl, shift, alt)
        });
        let app = self.clone();
        api.on_mouse_down(move |pane, row, col, shift, ctrl, clicks| {
            app.editor_mouse_down(pane, row, col, shift, ctrl, clicks)
        });
        let app = self.clone();
        api.on_mouse_drag(move |pane, row, col| app.editor_mouse_drag(pane, row, col));
        let app = self.clone();
        api.on_mouse_up(move |pane| {
            if let Some(p) = app.state.borrow_mut().editor.panes.get_mut(pane as usize) {
                p.dragging = false;
            }
        });
        let app = self.clone();
        api.on_fold_click(move |pane, line| app.editor_toggle_fold(pane, line.max(0) as usize));
        let app = self.clone();
        api.on_popup_click(move |pane, i| {
            if let Some(p) = app
                .state
                .borrow_mut()
                .editor
                .panes
                .get_mut(pane as usize)
                .and_then(|p| p.popup.as_mut())
            {
                p.selected = p.first + i.max(0) as usize;
            }
            app.accept_completion(pane);
        });
        let app = self.clone();
        api.on_popup_scroll(move |pane, delta| {
            if let Some(p) = app
                .state
                .borrow_mut()
                .editor
                .panes
                .get_mut(pane as usize)
                .and_then(|p| p.popup.as_mut())
            {
                p.scroll(delta as isize);
            }
            app.render_pane(pane);
        });
        let app = self.clone();
        api.on_nav_jump(move |pane, line| {
            app.focus_pane(pane);
            app.editor_goto_line(pane, line.max(0) as usize, None);
        });
        let app = self.clone();
        api.on_focus_changed(move |pane, focused| {
            if focused {
                app.focus_pane(pane);
            }
            if !focused {
                let changed = {
                    let mut st = app.state.borrow_mut();
                    let st = &mut *st;
                    if let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) {
                        tab.buffer.break_undo_group();
                    }
                    st.editor
                        .panes
                        .get_mut(pane as usize)
                        .and_then(|p| p.popup.take())
                        .is_some()
                };
                if changed {
                    app.render_pane(pane);
                }
            }
        });
        let app = self.clone();
        api.on_zoom(move |_pane, delta| {
            let zoom = app.state.borrow().settings.editor_zoom;
            app.set_zoom(match delta.signum() {
                0 => apex_core::DEFAULT_ZOOM,
                d => apex_core::zoom_step(zoom, d > 0),
            });
        });
        let app = self.clone();
        api.on_set_zoom(move |_pane, percent| {
            app.set_zoom(u32::try_from(percent).unwrap_or(apex_core::DEFAULT_ZOOM));
        });
        let app = self.clone();
        api.on_banner_install(move |pane| app.install_missing_packages(pane));
        let app = self.clone();
        api.on_banner_ignore(move |pane| app.ignore_missing_packages(pane));
        let app = self.clone();
        api.on_quickfix(move |pane| {
            let pkg = app
                .state
                .borrow()
                .editor
                .panes
                .get(pane as usize)
                .and_then(|p| p.quickfix.clone());
            if let Some(pkg) = pkg {
                app.install_packages_named(pane, vec![pkg]);
            }
        });
        let app = self.clone();
        api.on_command(move |pane, id| {
            app.focus_pane(pane);
            app.editor_command(pane, &id);
        });
        let app = self.clone();
        api.on_prompt_changed(move |pane, text| app.prompt_changed(pane, &text));
        let app = self.clone();
        api.on_prompt_accept(move |pane, text, back| app.prompt_accept(pane, &text, back));
        let app = self.clone();
        api.on_prompt_close(move |pane| app.close_prompt(pane));
    }

    /// Loads snippet files contributed by active plugins.
    pub fn load_plugin_snippets(&self) {
        let files: Vec<std::path::PathBuf> = {
            let st = self.state.borrow();
            st.plugins
                .as_ref()
                .map(|m| {
                    m.contributions()
                        .snippets
                        .into_iter()
                        .map(|c| c.path)
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut snippets = snippet::builtin_python_snippets();
        let mut problems = Vec::new();
        for f in files {
            match std::fs::read_to_string(&f)
                .map_err(|e| e.to_string())
                .and_then(|t| snippet::parse_vscode_snippets(&t).map_err(|e| e.to_string()))
            {
                Ok(defs) => snippets.extend(defs),
                Err(e) => problems.push(format!("Snippets {}: {e}", f.display())),
            }
        }
        let mut st = self.state.borrow_mut();
        st.editor.index = apex_editor::completion::PythonIndex::new(&snippets);
        st.editor.snippets = snippets;
        drop(st);
        for p in problems {
            tracing::warn!("{p}");
        }
    }

    /// Index of the pane of the focused editor group (what menus and
    /// "open at line" act on).
    pub fn focused_pane(&self) -> i32 {
        let st = self.state.borrow();
        st.editor
            .layout
            .index_of(st.editor.layout.focused)
            .map_or(0, to_i32)
    }

    fn pane_count(&self) -> usize {
        self.state.borrow().editor.panes.len()
    }

    /// Re-renders every pane (after a theme change, diagnostics, ...).
    pub fn refresh_highlight(&self) {
        self.theme_windows();
        for i in 0..self.pane_count() {
            if let Some(p) = self.state.borrow_mut().editor.panes.get_mut(i) {
                p.minimap_key = None;
                p.slots.clear();
            }
            self.render_pane(to_i32(i));
        }
    }

    /// Shows the active tab in the editor: restores its caret and scroll
    /// and focuses the editor.
    pub fn editor_show_active(&self) {
        self.sync_groups();
        let focused = {
            let st = self.state.borrow();
            st.editor.layout.index_of(st.editor.layout.focused)
        };
        if let Some(p) = focused.and_then(|i| {
            self.state.borrow_mut().editor.panes.get_mut(i).map(|p| {
                p.focus_serial += 1;
                i
            })
        }) {
            self.render_pane(to_i32(p));
        }
    }

    /// Sets the editor zoom (percent, clamped to 50..=300) for every pane
    /// and remembers it in the settings file.
    pub fn set_zoom(&self, percent: u32) {
        let percent = percent.clamp(apex_core::MIN_ZOOM, apex_core::MAX_ZOOM);
        {
            let mut st = self.state.borrow_mut();
            if st.settings.editor_zoom == percent {
                return;
            }
            st.settings.editor_zoom = percent;
        }
        self.save_settings();
        self.render_all_panes();
    }

    /// Moves the caret to a 0-based line and UTF-16 column (as language
    /// servers report positions) and scrolls it into view.
    pub fn editor_goto_utf16(&self, line: usize, col: usize) {
        let idx = {
            let st = self.state.borrow();
            st.workspace
                .as_ref()
                .and_then(Workspace::active_tab)
                .map(|t| {
                    let b = &t.buffer;
                    let line = line.min(b.len_lines().saturating_sub(1));
                    b.utf16_position_to_char(apex_editor::Position::new(line, col))
                        .unwrap_or_else(|_| b.len_chars())
                })
        };
        if let Some(idx) = idx {
            let pane = self.focused_pane();
            self.set_cursor(pane, Cursor::at(idx), true);
            self.focus_editor(pane);
        }
    }

    /// Moves the caret to a 0-based line and char column (clamped) and
    /// focuses pane `pane`.
    pub(crate) fn editor_goto_line(&self, pane: i32, line: usize, col: Option<usize>) {
        let idx = {
            let st = self.state.borrow();
            st.workspace
                .as_ref()
                .and_then(Workspace::active_tab)
                .map(|t| goto_index(&t.buffer, line, col))
        };
        if let Some(idx) = idx {
            self.set_cursor(pane, Cursor::at(idx), true);
            self.focus_editor(pane);
        }
    }

    fn focus_editor(&self, pane: i32) {
        if let Some(p) = self.state.borrow_mut().editor.panes.get_mut(pane as usize) {
            p.focus_serial += 1;
        }
        self.render_pane(pane);
    }

    /// Sets the caret of the active tab, reveals it (centred when `center`)
    /// and renders.
    fn set_cursor(&self, pane: i32, cursor: Cursor, center: bool) {
        {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) else {
                return;
            };
            tab.buffer.break_undo_group();
            tab.view.cursor = cursor;
            unfold_around(tab, cursor.sel.head);
            if let Some(p) = st.editor.panes.get_mut(pane as usize) {
                p.popup = None;
                reveal(p, tab, center);
            }
        }
        self.render_pane(pane);
    }

    // ------------------------------------------------------------ viewport

    fn editor_viewport(&self, pane: i32, vp: Viewport) {
        {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let Some(p) = st.editor.panes.get_mut(pane as usize) else {
                return;
            };
            let mut vp = vp;
            if let Some((tx, ty)) = p.pending_scroll {
                if (vp.sx - tx).abs() < 1.0 && (vp.sy - ty).abs() < 1.0 {
                    p.pending_scroll = None;
                } else {
                    vp.sx = p.viewport.sx;
                    vp.sy = p.viewport.sy;
                }
            }
            if vp == p.viewport {
                return;
            }
            let metrics_changed = vp.char_width != p.viewport.char_width
                || vp.line_height != p.viewport.line_height
                || vp.height != p.viewport.height;
            p.viewport = vp;
            if metrics_changed {
                p.minimap_key = None;
            }
            let path = st
                .editor
                .layout
                .groups
                .get(pane as usize)
                .and_then(|g| g.active_path())
                .map(|p| p.to_path_buf());
            if let (Some(ws), Some(path)) = (st.workspace.as_mut(), path) {
                if let Some(i) = ws.tab_index(&path) {
                    ws.tabs[i].view.scroll = (vp.sx, vp.sy);
                }
            }
        }
        self.render_pane(pane);
    }

    // ------------------------------------------------------------ render

    /// Pushes the visible state of pane `pane` to the UI.
    pub fn render_pane(&self, pane: i32) {
        let started = std::time::Instant::now();
        let tab_index = self.pane_tab(pane);
        let frame = {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let theme = st.themes.get_or_default(&st.settings.theme);
            let zoom = i32::try_from(st.settings.editor_zoom).unwrap_or(100);
            let Some(p) = st.editor.panes.get_mut(pane as usize) else {
                return;
            };
            // A different file in this pane: restore its scroll position.
            let ws = st.workspace.as_mut();
            let tab = ws
                .as_ref()
                .and_then(|w| tab_index.and_then(|i| w.tabs.get(i)));
            let path = tab.map(|t| t.path.clone());
            if p.shown != path {
                let scroll = tab.map_or((0.0, 0.0), |t| t.view.scroll);
                p.shown = path;
                p.popup = None;
                p.slots.clear();
                p.minimap_key = None;
                p.scroll_serial += 1;
                p.scroll_to = scroll;
                p.pending_scroll = Some(scroll);
                p.viewport.sx = scroll.0;
                p.viewport.sy = scroll.1;
            }
            build_frame(
                pane as usize,
                p,
                &st.editor.panes_model,
                ws,
                tab_index,
                theme,
                zoom,
            )
        };
        let Some(mut frame) = frame else { return };
        self.group_fields(&mut frame.pane, pane as usize);
        // Model updates happen without holding the state borrow.
        if frame.reset_lines || frame.lines_model.row_count() != frame.slot_count {
            let rows: Vec<EdLine> = (0..frame.slot_count).map(|_| EdLine::default()).collect();
            frame.lines_model.set_vec(rows);
        }
        for (slot, line) in frame.slot_updates {
            frame.lines_model.set_row_data(slot, line);
        }
        frame.spans_model.set_vec(frame.spans);
        sync_rows(&frame.popup_model, frame.popup_rows);
        if frame.index < frame.panes_model.row_count() {
            frame.panes_model.set_row_data(frame.index, frame.pane);
        }
        let is_active = {
            let st = self.state.borrow();
            tab_index.is_some() && st.workspace.as_ref().map(|w| w.active) == Some(tab_index)
        };
        if is_active {
            let caret = frame.caret_text;
            self.with_ui(|ui| ui.global::<WorkspaceApi>().set_caret_text(caret.into()));
        }
        tracing::debug!(target: "editor", "render pane {pane} in {:?}", started.elapsed());
    }

    // ------------------------------------------------------------ input

    fn editor_mouse_down(
        &self,
        pane: i32,
        row: i32,
        col: i32,
        shift: bool,
        ctrl: bool,
        clicks: i32,
    ) {
        let _ = ctrl;
        {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) else {
                return;
            };
            sync_tab(tab);
            let map = fold_map(tab);
            let line = map.line_of_row(row.max(0) as usize);
            let idx = index_at_visual(&tab.buffer, line, col.max(0) as usize);
            tab.buffer.break_undo_group();
            let sel = if clicks >= 2 {
                let r = word_range_at(&tab.buffer, idx);
                Selection::new(r.start, r.end)
            } else if shift {
                Selection::new(tab.view.cursor.sel.anchor, idx)
            } else {
                Selection::caret(idx)
            };
            tab.view.cursor = Cursor::with(sel);
            if let Some(s) = &tab.view.snippet {
                if !s.contains(idx) {
                    tab.view.snippet = None;
                }
            }
            if let Some(p) = st.editor.panes.get_mut(pane as usize) {
                p.popup = None;
                p.dragging = clicks < 2;
                p.caret_serial += 1;
            }
        }
        self.render_pane(pane);
    }

    fn editor_mouse_drag(&self, pane: i32, row: i32, col: i32) {
        {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let Some(p) = st.editor.panes.get_mut(pane as usize) else {
                return;
            };
            if !p.dragging {
                return;
            }
            let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) else {
                return;
            };
            let map = fold_map(tab);
            let rows = map.rows();
            let row = if row < 0 { 0 } else { (row as usize).min(rows) };
            let idx = if row >= rows {
                tab.buffer.len_chars()
            } else {
                index_at_visual(&tab.buffer, map.line_of_row(row), col.max(0) as usize)
            };
            if tab.view.cursor.sel.head == idx {
                return;
            }
            tab.view.cursor.sel.head = idx;
            tab.view.cursor.preferred_col = None;
            reveal(p, tab, false);
        }
        self.render_pane(pane);
    }

    fn editor_toggle_fold(&self, pane: i32, line: usize) {
        {
            let mut st = self.state.borrow_mut();
            let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) else {
                return;
            };
            sync_tab(tab);
            if !tab.view.folded.remove(&line) && tab.view.outline.fold_at(line).is_some() {
                tab.view.folded.insert(line);
                move_caret_out_of_folds(tab);
            }
        }
        self.render_pane(pane);
    }

    /// Handles a key; returns whether it was used.
    fn editor_key(&self, pane: i32, text: &str, ctrl: bool, shift: bool, alt: bool) -> bool {
        let Some(action) = keys::map_key(text, ctrl, shift, alt) else {
            return false;
        };
        let has_tab = self
            .state
            .borrow()
            .workspace
            .as_ref()
            .and_then(Workspace::active_tab)
            .is_some();
        if !has_tab {
            return false;
        }

        // The completion popup takes navigation keys first.
        let popup_open = self
            .state
            .borrow()
            .editor
            .panes
            .get(pane as usize)
            .and_then(|p| p.popup.as_ref())
            .is_some_and(|p| !p.is_empty());
        if popup_open {
            let delta = match &action {
                Action::Move(Motion::Up, false) => Some(-1),
                Action::Move(Motion::Down, false) => Some(1),
                Action::Move(Motion::PageUp(_), false) => Some(-(VISIBLE_ROWS as isize)),
                Action::Move(Motion::PageDown(_), false) => Some(VISIBLE_ROWS as isize),
                _ => None,
            };
            if let Some(d) = delta {
                if let Some(p) = self
                    .state
                    .borrow_mut()
                    .editor
                    .panes
                    .get_mut(pane as usize)
                    .and_then(|p| p.popup.as_mut())
                {
                    p.move_by(d);
                }
                self.render_pane(pane);
                return true;
            }
            match action {
                Action::Newline | Action::Tab => {
                    self.accept_completion(pane);
                    return true;
                }
                Action::Escape => {
                    self.close_popup(pane);
                    return true;
                }
                _ => {}
            }
        }

        self.run_action(pane, action)
    }

    /// Performs `action` on pane `pane`; returns whether it was used.
    fn run_action(&self, pane: i32, action: Action) -> bool {
        match action {
            Action::Find => {
                self.open_prompt(pane, 1);
                true
            }
            Action::GoToLine => {
                self.open_prompt(pane, 2);
                true
            }
            Action::FindNext(forward) => {
                let has_text = self
                    .state
                    .borrow()
                    .editor
                    .panes
                    .get(pane as usize)
                    .is_some_and(|p| !p.prompt_text.is_empty());
                if has_text {
                    self.find_step(pane, Some(forward));
                } else {
                    self.open_prompt(pane, 1);
                }
                true
            }
            Action::Escape
                if self
                    .state
                    .borrow()
                    .editor
                    .panes
                    .get(pane as usize)
                    .is_some_and(|p| p.prompt_mode != 0) =>
            {
                self.close_prompt(pane);
                true
            }
            Action::Save => {
                self.sync_lsp();
                self.save_active();
                true
            }
            Action::Run => {
                self.run_selected();
                true
            }
            Action::Complete => {
                self.trigger_completion(pane, true);
                true
            }
            Action::Escape => self.escape(pane),
            Action::Copy => {
                self.copy(false);
                true
            }
            Action::Move(m, extend) => {
                self.move_caret(pane, m, extend);
                true
            }
            Action::SelectAll => {
                let len = self.with_tab(|t| t.buffer.len_chars()).unwrap_or(0);
                self.update_cursor(pane, Cursor::with(Selection::new(0, len)), false);
                true
            }
            Action::Fold(fold) => {
                self.fold_at_caret(pane, fold);
                true
            }
            Action::Tab | Action::BackTab if self.snippet_jump(pane, action == Action::Tab) => true,
            other => {
                self.edit(pane, other);
                true
            }
        }
    }

    /// Feeds scripted keys to pane 0 (for off-screen screenshots and
    /// tests): plain text is typed; `{name}` sends a special key: `enter`,
    /// `tab`, `esc`, `bs`, `up`, `down`, `left`, `right`, `home`, `end`,
    /// `ctrl+space`, `ctrl+/`, `ctrl+a`, `ctrl+z`, `shift+right`,
    /// `fold`, `complete-now`.
    pub fn editor_script(&self, script: &str) {
        use slint::platform::Key;
        let k = |key: Key| String::from(char::from(key));
        let mut rest = script;
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix('{') {
                let Some(end) = after.find('}') else { break };
                let name = &after[..end];
                rest = &after[end + 1..];
                let (text, ctrl, shift) = match name {
                    "enter" => ("\n".to_owned(), false, false),
                    "tab" => ("\t".to_owned(), false, false),
                    "esc" => (k(Key::Escape), false, false),
                    "bs" => (k(Key::Backspace), false, false),
                    "up" => (k(Key::UpArrow), false, false),
                    "down" => (k(Key::DownArrow), false, false),
                    "left" => (k(Key::LeftArrow), false, false),
                    "right" => (k(Key::RightArrow), false, false),
                    "shift+right" => (k(Key::RightArrow), false, true),
                    "home" => (k(Key::Home), false, false),
                    "end" => (k(Key::End), false, false),
                    "ctrl+end" => (k(Key::End), true, false),
                    "ctrl+space" => (" ".to_owned(), true, false),
                    "ctrl+/" => ("/".to_owned(), true, false),
                    "ctrl+a" => ("a".to_owned(), true, false),
                    "ctrl+z" => ("z".to_owned(), true, false),
                    "fold" => ("-".to_owned(), true, false),
                    "run" => (String::from(char::from(Key::F10)), false, true),
                    "drag-bottom" => {
                        // Drags the first tab of pane 0 to the bottom edge.
                        let (x, y, w, h) = self.state.borrow().editor.area;
                        self.tab_drag(0, 0, x + w / 4.0, y + h - 10.0, false);
                        self.tab_drag(0, 0, x + w / 4.0, y + h - 10.0, true);
                        continue;
                    }
                    cmd if cmd.starts_with("cmd:") => {
                        self.editor_command(-1, &cmd[4..]);
                        continue;
                    }
                    "install" => {
                        self.install_missing_packages(0);
                        continue;
                    }
                    "complete-now" => {
                        self.trigger_completion(0, false);
                        continue;
                    }
                    _ => continue,
                };
                {
                    let pane = self.focused_pane();
                    self.editor_key(pane, &text, ctrl, shift, false);
                }
            } else {
                let c = rest.chars().next().unwrap_or(' ');
                rest = &rest[c.len_utf8()..];
                self.editor_key(self.focused_pane(), &c.to_string(), false, false, false);
            }
        }
    }

    /// Runs a named editor command on pane `pane` (`-1`: the focused
    /// group). Ids: `editor.undo`, `editor.redo`, `editor.cut`,
    /// `editor.copy`, `editor.paste`, `editor.selectAll`, `editor.find`,
    /// `editor.findNext`, `editor.findPrevious`, `editor.goToLine`,
    /// `editor.toggleComment`, `editor.duplicateLine`,
    /// `editor.triggerSuggest`, `editor.fold`, `editor.unfold`,
    /// `editor.zoomIn`, `editor.zoomOut`, `editor.zoomReset`,
    /// `editor.splitRight`, `editor.splitDown`, `editor.moveToNewWindow`,
    /// `editor.installMissingPackages`. Returns whether the id is known.
    pub fn editor_command(&self, pane: i32, id: &str) -> bool {
        let pane = if pane < 0 { self.focused_pane() } else { pane };
        let action = match id {
            "editor.undo" => Action::Undo,
            "editor.redo" => Action::Redo,
            "editor.cut" => Action::Cut,
            "editor.copy" => Action::Copy,
            "editor.paste" => Action::Paste,
            "editor.selectAll" => Action::SelectAll,
            "editor.find" => Action::Find,
            "editor.findNext" => Action::FindNext(true),
            "editor.findPrevious" => Action::FindNext(false),
            "editor.goToLine" => Action::GoToLine,
            "editor.toggleComment" => Action::ToggleComment,
            "editor.duplicateLine" => Action::Duplicate,
            "editor.triggerSuggest" => Action::Complete,
            "editor.fold" => Action::Fold(true),
            "editor.unfold" => Action::Fold(false),
            "editor.zoomIn" | "editor.zoomOut" | "editor.zoomReset" => {
                let zoom = self.state.borrow().settings.editor_zoom;
                self.set_zoom(match id {
                    "editor.zoomIn" => apex_core::zoom_step(zoom, true),
                    "editor.zoomOut" => apex_core::zoom_step(zoom, false),
                    _ => apex_core::DEFAULT_ZOOM,
                });
                return true;
            }
            "editor.installMissingPackages" => {
                self.install_missing_packages(pane);
                return true;
            }
            "editor.splitRight" | "editor.splitDown" | "editor.moveToNewWindow" => {
                return self.layout_command(pane, id);
            }
            _ => return false,
        };
        self.run_action(pane, action);
        if let Some(p) = self.state.borrow_mut().editor.panes.get_mut(pane as usize) {
            if p.prompt_mode == 0 {
                p.focus_serial += 1;
            }
        }
        self.render_pane(pane);
        true
    }

    // ------------------------------------------------------------ find

    fn open_prompt(&self, pane: i32, mode: i32) {
        {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let selected = st
                .workspace
                .as_ref()
                .and_then(Workspace::active_tab)
                .map(|t| commands::copy_text(&t.buffer, t.view.cursor.sel))
                .filter(|(_, line)| !line)
                .map(|(t, _)| t)
                .filter(|t| !t.contains('\n'));
            let Some(p) = st.editor.panes.get_mut(pane as usize) else {
                return;
            };
            p.popup = None;
            p.prompt_mode = mode;
            p.prompt_text = if mode == 1 {
                selected.unwrap_or_else(|| p.prompt_text.clone())
            } else {
                String::new()
            };
            p.prompt_info.clear();
            p.prompt_serial += 1;
            p.matches_rev = None;
        }
        if mode == 1 {
            let text = self.state.borrow().editor.panes[pane as usize]
                .prompt_text
                .clone();
            self.prompt_changed(pane, &text);
        } else {
            self.render_pane(pane);
        }
    }

    fn close_prompt(&self, pane: i32) {
        if let Some(p) = self.state.borrow_mut().editor.panes.get_mut(pane as usize) {
            p.prompt_mode = 0;
            p.matches.clear();
            p.matches_rev = None;
            p.prompt_serial += 1;
        }
        self.render_pane(pane);
    }

    /// The find text changed: select the first match at or after the caret.
    fn prompt_changed(&self, pane: i32, text: &str) {
        let mode = {
            let mut st = self.state.borrow_mut();
            let Some(p) = st.editor.panes.get_mut(pane as usize) else {
                return;
            };
            p.prompt_text = text.to_owned();
            p.matches_rev = None;
            p.prompt_mode
        };
        if mode == 1 {
            self.find_step(pane, None);
        } else {
            self.render_pane(pane);
        }
    }

    fn prompt_accept(&self, pane: i32, text: &str, back: bool) {
        let mode = self
            .state
            .borrow()
            .editor
            .panes
            .get(pane as usize)
            .map_or(0, |p| p.prompt_mode);
        match mode {
            1 => self.find_step(pane, Some(!back)),
            2 => {
                let mut parts = text.trim().splitn(2, [':', ',']);
                let line = parts.next().and_then(|l| l.trim().parse::<usize>().ok());
                let col = parts.next().and_then(|c| c.trim().parse::<usize>().ok());
                if let Some(line) = line {
                    self.close_prompt(pane);
                    self.editor_goto_line(
                        pane,
                        line.saturating_sub(1),
                        col.map(|c| c.saturating_sub(1)),
                    );
                }
            }
            _ => {}
        }
    }

    /// Selects a match of the find text: `None` the first at or after the
    /// selection start, `Some(true)` the next, `Some(false)` the previous.
    fn find_step(&self, pane: i32, dir: Option<bool>) {
        let target = {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let Some(p) = st.editor.panes.get_mut(pane as usize) else {
                return;
            };
            let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) else {
                return;
            };
            refresh_matches(p, tab);
            let sel = tab.view.cursor.sel.range();
            let n = p.matches.len();
            let index = if n == 0 {
                None
            } else {
                Some(match dir {
                    None => p
                        .matches
                        .iter()
                        .position(|m| m.start >= sel.start)
                        .unwrap_or(0),
                    Some(true) => p
                        .matches
                        .iter()
                        .position(|m| m.start > sel.start)
                        .unwrap_or(0),
                    Some(false) => p
                        .matches
                        .iter()
                        .rposition(|m| m.start < sel.start)
                        .unwrap_or(n - 1),
                })
            };
            p.prompt_info = match index {
                Some(i) => format!("{} of {n}", i + 1),
                None if p.prompt_text.is_empty() => String::new(),
                None => "No results".to_owned(),
            };
            index.map(|i| p.matches[i].clone())
        };
        match target {
            Some(r) => self.set_cursor(pane, Cursor::with(Selection::new(r.start, r.end)), true),
            None => self.render_pane(pane),
        }
    }

    fn with_tab<T>(&self, f: impl FnOnce(&Tab) -> T) -> Option<T> {
        let st = self.state.borrow();
        st.workspace.as_ref().and_then(Workspace::active_tab).map(f)
    }

    fn escape(&self, pane: i32) -> bool {
        let done = {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let popup = st
                .editor
                .panes
                .get_mut(pane as usize)
                .and_then(|p| p.popup.take())
                .is_some();
            let tab = st.workspace.as_mut().and_then(Workspace::active_tab_mut);
            match tab {
                Some(tab) if tab.view.snippet.is_some() => {
                    tab.view.snippet = None;
                    true
                }
                Some(tab) if !tab.view.cursor.sel.is_empty() => {
                    let h = tab.view.cursor.sel.head;
                    tab.view.cursor = Cursor::at(h);
                    true
                }
                _ => popup,
            }
        };
        if done {
            self.render_pane(pane);
        }
        done
    }

    fn close_popup(&self, pane: i32) {
        if let Some(p) = self.state.borrow_mut().editor.panes.get_mut(pane as usize) {
            p.popup = None;
        }
        self.render_pane(pane);
    }

    fn update_cursor(&self, pane: i32, cursor: Cursor, keep_popup: bool) {
        {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) else {
                return;
            };
            tab.view.cursor = cursor;
            if let Some(p) = st.editor.panes.get_mut(pane as usize) {
                if !keep_popup {
                    p.popup = None;
                }
                p.caret_serial += 1;
                reveal(p, tab, false);
            }
        }
        self.render_pane(pane);
    }

    fn move_caret(&self, pane: i32, motion: Motion, extend: bool) {
        let cursor = {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let page = st
                .editor
                .panes
                .get(pane as usize)
                .map_or(20, |p| p.viewport.page_rows().saturating_sub(1).max(1));
            let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) else {
                return;
            };
            sync_tab(tab);
            tab.buffer.break_undo_group();
            let motion = match motion {
                Motion::PageUp(_) => Motion::PageUp(page),
                Motion::PageDown(_) => Motion::PageDown(page),
                m => m,
            };
            let mut c = move_cursor(&tab.buffer, tab.view.cursor, motion, extend);
            // Vertical moves step over folded blocks.
            if matches!(motion, Motion::Up | Motion::Down) {
                let map = fold_map(tab);
                let (line, _) = line_col(&tab.buffer, c.sel.head);
                if map.is_hidden(line) {
                    let row = map.row_of_line(line);
                    let target_row = if motion == Motion::Down { row + 1 } else { row };
                    let target = map.line_of_row(target_row.min(map.rows().saturating_sub(1)));
                    let want = c.preferred_col.unwrap_or(0);
                    c.sel.head = index_at_visual(&tab.buffer, target, want);
                    if !extend {
                        c.sel.anchor = c.sel.head;
                    }
                }
            }
            if let Some(s) = &tab.view.snippet {
                if !s.contains(c.sel.head) {
                    tab.view.snippet = None;
                }
            }
            c
        };
        self.update_cursor(pane, cursor, false);
    }

    fn fold_at_caret(&self, pane: i32, fold: bool) {
        {
            let mut st = self.state.borrow_mut();
            let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) else {
                return;
            };
            sync_tab(tab);
            let (line, _) = line_col(&tab.buffer, tab.view.cursor.sel.head);
            let containing: Vec<FoldRange> = tab
                .view
                .outline
                .folds
                .iter()
                .filter(|f| f.start <= line && line <= f.end)
                .copied()
                .collect();
            if fold {
                if let Some(f) = containing
                    .iter()
                    .rev()
                    .find(|f| !tab.view.folded.contains(&f.start))
                {
                    tab.view.folded.insert(f.start);
                }
                move_caret_out_of_folds(tab);
            } else {
                for f in containing {
                    tab.view.folded.remove(&f.start);
                }
            }
        }
        self.render_pane(pane);
    }

    /// Tab / Shift+Tab inside a snippet: jumps between its stops. Returns
    /// whether it did.
    fn snippet_jump(&self, pane: i32, forward: bool) -> bool {
        let sel = {
            let mut st = self.state.borrow_mut();
            let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) else {
                return false;
            };
            sync_tab(tab);
            let Some(session) = tab.view.snippet.as_mut() else {
                return false;
            };
            let next = if forward {
                session.next_stop()
            } else {
                session.prev_stop()
            };
            let at_last = session.at_last();
            match next {
                Some(r) => {
                    if at_last {
                        tab.view.snippet = None;
                    }
                    Selection::new(r.start, r.end)
                }
                None => {
                    tab.view.snippet = None;
                    return false;
                }
            }
        };
        self.update_cursor(pane, Cursor::with(sel), false);
        true
    }

    // ------------------------------------------------------------ editing

    fn copy(&self, cut: bool) {
        let text = {
            let mut st = self.state.borrow_mut();
            let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) else {
                return;
            };
            commands::copy_text(&tab.buffer, tab.view.cursor.sel)
        };
        let _ = cut;
        self.set_clipboard(&text.0, text.1);
    }

    fn set_clipboard(&self, text: &str, whole_line: bool) {
        let mut st = self.state.borrow_mut();
        let ed = &mut st.editor;
        ed.line_copy = whole_line.then(|| text.to_owned());
        if ed.clipboard.is_none() {
            ed.clipboard = arboard::Clipboard::new().ok();
        }
        if let Some(c) = ed.clipboard.as_mut() {
            if let Err(e) = c.set_text(text.to_owned()) {
                tracing::warn!(%e, "clipboard write");
            }
        }
    }

    fn clipboard_text(&self) -> Option<(String, bool)> {
        let mut st = self.state.borrow_mut();
        let ed = &mut st.editor;
        if ed.clipboard.is_none() {
            ed.clipboard = arboard::Clipboard::new().ok();
        }
        let text = ed.clipboard.as_mut()?.get_text().ok()?;
        let line = ed.line_copy.as_deref() == Some(text.as_str());
        Some((text, line))
    }

    /// Runs an editing action on the active tab, then updates everything
    /// that depends on the text.
    fn edit(&self, pane: i32, action: Action) {
        let clipboard = if action == Action::Paste {
            match self.clipboard_text() {
                Some(c) => Some(c),
                None => return,
            }
        } else {
            None
        };
        let typed = match &action {
            Action::Type(t) => Some(t.clone()),
            _ => None,
        };
        let is_backspace = action == Action::Backspace;
        let result = {
            let mut st = self.state.borrow_mut();
            let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) else {
                return;
            };
            sync_tab(tab);
            let was_dirty = tab.buffer.is_dirty();
            let rev = tab.buffer.revision();
            let sel = tab.view.cursor.sel;
            let b = &mut tab.buffer;
            let auto = commands::auto_closes(b.language());
            let res: apex_editor::Result<Option<(Selection, Option<String>)>> = match action {
                Action::Type(t) => commands::type_text(b, sel, &t, auto).map(|s| Some((s, None))),
                Action::Newline => commands::newline(b, sel).map(|s| Some((s, None))),
                Action::Backspace => commands::backspace(b, sel).map(|s| Some((s, None))),
                Action::Delete => commands::delete_forward(b, sel).map(|s| Some((s, None))),
                Action::DeleteWord(fwd) => {
                    commands::delete_word(b, sel, fwd).map(|s| Some((s, None)))
                }
                Action::Tab => commands::indent(b, sel).map(|s| Some((s, None))),
                Action::BackTab => commands::dedent(b, sel).map(|s| Some((s, None))),
                Action::ToggleComment => {
                    commands::toggle_comment(b, sel).map(|o| o.map(|s| (s, None)))
                }
                Action::Undo => Ok(commands::undo(b).map(|s| (s, None))),
                Action::Redo => Ok(commands::redo(b).map(|s| (s, None))),
                Action::Duplicate => commands::duplicate(b, sel).map(|s| Some((s, None))),
                Action::Cut => commands::cut(b, sel).map(|(t, s)| Some((s, Some(t)))),
                Action::Paste => {
                    let (text, whole_line) = clipboard.unwrap_or_default();
                    if whole_line && sel.is_empty() {
                        let (line, _) = line_col(b, sel.head);
                        let start = b.rope().line_to_char(line);
                        // The caret keeps its column on the shifted line.
                        commands::insert_text(b, Selection::caret(start), &text)
                            .map(|s| Some((Selection::caret(sel.head + (s.head - start)), None)))
                    } else {
                        commands::insert_text(b, sel, &text).map(|s| Some((s, None)))
                    }
                }
                _ => Ok(None),
            };
            match res {
                Ok(Some((new_sel, cut_text))) => {
                    tab.view.cursor = Cursor::with(new_sel);
                    let changed = tab.buffer.revision() != rev;
                    Ok(Some((
                        changed,
                        was_dirty != tab.buffer.is_dirty(),
                        cut_text,
                    )))
                }
                Ok(None) => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        };
        let (changed, dirty_changed, cut_text) = match result {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                self.notify(NoticeKind::Error, format!("Edit failed: {e}"));
                return;
            }
        };
        if let Some(t) = cut_text {
            let line = t.ends_with('\n');
            self.set_clipboard(&t, line);
        }
        if changed {
            self.text_changed();
        }
        if dirty_changed {
            self.push_tabs();
        }
        // Completion follows typing.
        let popup_open = self
            .state
            .borrow()
            .editor
            .panes
            .get(pane as usize)
            .is_some_and(|p| p.popup.is_some());
        match typed.as_deref() {
            // The popup opens (or narrows) in this same key event and is
            // painted by the render below: no debounce before it shows.
            Some(".") => {
                self.close_popup_quiet(pane);
                self.open_completion(pane, false);
            }
            Some(t) if t.chars().all(|c| c.is_alphanumeric() || c == '_') => {
                if popup_open {
                    self.refilter_popup_quiet(pane);
                } else {
                    self.open_completion(pane, false);
                }
            }
            _ if popup_open && is_backspace => {}
            _ => self.close_popup_quiet(pane),
        }
        {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            if let (Some(p), Some(tab)) = (
                st.editor.panes.get_mut(pane as usize),
                st.workspace.as_mut().and_then(Workspace::active_tab_mut),
            ) {
                sync_tab(tab);
                unfold_around(tab, tab.view.cursor.sel.head);
                p.caret_serial += 1;
                reveal(p, tab, false);
            }
        }
        if let Some(p) = self.state.borrow_mut().editor.panes.get_mut(pane as usize) {
            p.typing = true;
        }
        if popup_open && is_backspace {
            // Keeps filtering (closes once the word is gone).
            self.refilter_popup(pane);
        } else {
            self.render_pane(pane);
        }
        if let Some(p) = self.state.borrow_mut().editor.panes.get_mut(pane as usize) {
            p.typing = false;
        }
    }

    fn close_popup_quiet(&self, pane: i32) {
        if let Some(p) = self.state.borrow_mut().editor.panes.get_mut(pane as usize) {
            p.popup = None;
        }
    }

    /// The active tab's text changed: tell the language server and re-check
    /// imports.
    fn text_changed(&self) {
        {
            let mut st = self.state.borrow_mut();
            let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) else {
                return;
            };
            tab.view.lsp_dirty = true;
        }
        self.schedule_idle();
    }

    /// Runs [`Self::on_idle`] once typing pauses for [`IDLE_MS`].
    fn schedule_idle(&self) {
        let (seq, app) = {
            let mut st = self.state.borrow_mut();
            st.editor.idle += 1;
            (st.editor.idle, st.editor.app.clone())
        };
        slint::Timer::single_shot(Duration::from_millis(IDLE_MS), move || {
            let Some(app) = app.upgrade() else { return };
            if app.state.borrow().editor.idle == seq {
                app.on_idle();
            }
        });
    }

    /// Work deferred while typing: language server sync, outline (folds,
    /// guides, navigation bar), missing-package scan.
    fn on_idle(&self) {
        self.sync_lsp();
        {
            let mut st = self.state.borrow_mut();
            if let Some(ws) = st.workspace.as_mut() {
                for tab in &mut ws.tabs {
                    if let Some(h) = tab.view.highlighter.as_mut().filter(|h| h.is_stale()) {
                        h.finish_parse(tab.buffer.rope());
                        tab.view.cache.invalidate();
                    }
                    let rev = tab.buffer.revision();
                    if tab.view.outline.revision != Some(rev) {
                        tab.view.outline.update(&tab.buffer);
                        let o = &tab.view.outline;
                        tab.view.folded.retain(|l| o.fold_at(*l).is_some());
                    }
                }
                ws.packages.scan_allowed = true;
            }
            st.editor
                .panes
                .iter_mut()
                .for_each(|p| p.idle_render = true);
        }
        self.render_all_panes();
        self.state
            .borrow_mut()
            .editor
            .panes
            .iter_mut()
            .for_each(|p| p.idle_render = false);
        if let Some(ws) = self.state.borrow_mut().workspace.as_mut() {
            ws.packages.scan_allowed = false;
        }
    }

    /// Sends pending text changes of every tab to the language server (on
    /// its worker thread).
    pub(crate) fn sync_lsp(&self) {
        let mut st = self.state.borrow_mut();
        let st = &mut *st;
        let Some(ws) = st.workspace.as_mut() else {
            return;
        };
        let Some(client) = ws.lsp.client().cloned() else {
            return;
        };
        if !st
            .editor
            .lsp_worker
            .as_ref()
            .is_some_and(|w| w.serves(&client))
        {
            st.editor.lsp_worker = crate::lsp::LspWorker::spawn(client);
        }
        let Some(worker) = st.editor.lsp_worker.as_ref() else {
            return;
        };
        for tab in &mut ws.tabs {
            crate::lsp::queue_change(worker, tab);
        }
    }

    // ------------------------------------------------------------ completion

    /// Opens the completion popup at the caret (Ctrl+Space when `explicit`)
    /// and renders it.
    fn trigger_completion(&self, pane: i32, explicit: bool) {
        self.open_completion(pane, explicit);
        self.render_pane(pane);
    }

    /// Opens the completion popup at the caret straight away from the
    /// built-in candidates (prebuilt index, identifiers cached per text
    /// revision), then asks the language server after a short pause and
    /// merges its items in when they come. Does not render; returns whether
    /// a popup was opened.
    fn open_completion(&self, pane: i32, explicit: bool) -> bool {
        let mut rescan = None;
        let request = {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            st.editor.next_request += 1;
            let request_id = st.editor.next_request;
            let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) else {
                return false;
            };
            sync_tab(tab);
            let caret = tab.view.cursor.sel.head;
            let (start, prefix) = word_before(&tab.buffer, caret);
            let after_dot = start > 0 && tab.buffer.rope().char(start - 1) == '.';
            if !explicit && prefix.is_empty() && !after_dot {
                return false;
            }
            if prefix.starts_with(|c: char| c.is_ascii_digit()) {
                return false;
            }
            if !explicit && in_string_or_comment(tab, caret) {
                return false;
            }
            let b = &tab.buffer;
            let python = b.language() == Language::Python;
            let builtin = if python {
                let file = file_key(&tab.path);
                let revision = b.revision();
                let items = st.editor.index.complete(
                    file,
                    revision,
                    || std::borrow::Cow::Owned(b.text()),
                    &nearby_text(b, caret),
                    after_dot,
                );
                // Refresh the file's identifiers off the UI thread.
                if !st.editor.index.is_fresh(file, revision)
                    && st.editor.scanning != Some((file, revision))
                {
                    st.editor.scanning = Some((file, revision));
                    rescan = Some((file, revision, b.text()));
                }
                items
            } else {
                std::rc::Rc::new(apex_editor::completion::word_fallback(&b.text(), &prefix))
            };
            let mut popup = Popup::new(builtin, start, request_id, explicit);
            popup.refilter(&prefix);
            let wants_lsp = python && tab.lsp_open;
            let Some(p) = st.editor.panes.get_mut(pane as usize) else {
                return false;
            };
            p.popup = Some(popup);
            wants_lsp.then_some(request_id)
        };
        if let Some((file, revision, text)) = rescan {
            let sink = self.sink.clone();
            let spawned = std::thread::Builder::new()
                .name("completion-scan".into())
                .spawn(move || {
                    let scan = apex_editor::completion::IdentScan::of(&text);
                    sink.send(crate::events::AppEvent::Editor(EditorEvent::Identifiers {
                        file,
                        revision,
                        scan,
                    }));
                });
            if spawned.is_err() {
                self.state.borrow_mut().editor.scanning = None;
            }
        }
        if let Some(id) = request {
            if explicit {
                self.request_lsp_completion(pane, id);
            } else {
                let app = self.state.borrow().editor.app.clone();
                slint::Timer::single_shot(
                    Duration::from_millis(complete::LSP_DEBOUNCE_MS),
                    move || {
                        if let Some(app) = app.upgrade() {
                            app.request_lsp_completion(pane, id);
                        }
                    },
                );
            }
        }
        true
    }

    /// Asks the language server to complete at the caret for popup
    /// `request`, if that popup is still open.
    fn request_lsp_completion(&self, pane: i32, request: u64) {
        let target = {
            let st = self.state.borrow();
            let open = st
                .editor
                .panes
                .get(pane as usize)
                .and_then(|p| p.popup.as_ref())
                .is_some_and(|pp| pp.request == request);
            let ws = st.workspace.as_ref().filter(|_| open);
            ws.filter(|w| w.lsp.client().is_some())
                .and_then(Workspace::active_tab)
                .filter(|t| t.lsp_open)
                .and_then(|tab| {
                    let uri = path_to_uri(&tab.path).ok()?;
                    let pos = tab
                        .buffer
                        .char_to_utf16_position(tab.view.cursor.sel.head)
                        .ok()?;
                    Some((
                        uri,
                        apex_lsp::lsp_types::Position::new(pos.line as u32, pos.col as u32),
                    ))
                })
        };
        let Some((uri, pos)) = target else { return };
        // The server must see the latest text before it completes.
        self.sync_lsp();
        let st = self.state.borrow();
        if let Some(worker) = st.editor.lsp_worker.as_ref() {
            complete::request(worker, uri, pos, request, self.sink.clone());
        }
    }

    /// Handles an [`EditorEvent`] on the UI thread.
    pub(crate) fn on_editor_event(&self, event: EditorEvent) {
        match event {
            EditorEvent::Completion(reply) => self.on_completion(reply),
            EditorEvent::InstallOutput(line) => self.append_output(line, 2),
            EditorEvent::Identifiers {
                file,
                revision,
                scan,
            } => {
                let mut st = self.state.borrow_mut();
                st.editor.index.set_identifiers(file, revision, scan);
                if st.editor.scanning == Some((file, revision)) {
                    st.editor.scanning = None;
                }
            }
            EditorEvent::InstallDone(result) => {
                {
                    let mut st = self.state.borrow_mut();
                    if let Some(ws) = st.workspace.as_mut() {
                        ws.packages.installing = false;
                        let python = ws.packages.python.clone();
                        ws.packages.rescan(python);
                    }
                }
                match result {
                    Ok(()) => {
                        self.append_output("Packages installed.", 2);
                        self.notify(NoticeKind::Success, "Packages installed");
                    }
                    Err(e) => {
                        self.append_output(format!("Install failed: {e}"), 1);
                        self.notify(NoticeKind::Error, format!("Install failed: {e}"));
                    }
                }
                self.refresh_highlight();
            }
        }
    }

    /// Installs `names` into the project environment (quick fix).
    fn install_packages_named(&self, pane: i32, names: Vec<String>) {
        let python = {
            let mut st = self.state.borrow_mut();
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            if ws.packages.installing {
                return;
            }
            let python = ws.packages.python.clone();
            ws.packages.installing = python.is_some();
            python
        };
        let Some(python) = python else {
            self.notify(
                NoticeKind::Warning,
                "This project has no virtual environment to install packages into",
            );
            return;
        };
        self.with_ui(|ui| {
            let tools = ui.global::<crate::ToolsApi>();
            tools.set_tool_open(true);
            tools.set_tool_tab(0);
        });
        self.append_output(format!("> install {}", names.join(" ")), 2);
        packages::install(&python, names, self.sink.clone());
        self.render_pane(pane);
    }

    /// The banner's "Install all".
    fn install_missing_packages(&self, pane: i32) {
        let plan = {
            let mut st = self.state.borrow_mut();
            let Some(ws) = st.workspace.as_mut() else {
                return;
            };
            if ws.packages.installing {
                return;
            }
            let (_, pkgs) = packages::active_missing(ws);
            let names: Vec<String> = pkgs.into_iter().map(|p| p.pypi).collect();
            match (ws.packages.python.clone(), names.is_empty()) {
                (_, true) => None,
                (None, false) => Some(Err(())),
                (Some(py), false) => {
                    ws.packages.installing = true;
                    Some(Ok((py, names)))
                }
            }
        };
        match plan {
            None => {}
            Some(Err(())) => self.notify(
                NoticeKind::Warning,
                "This project has no virtual environment to install packages into",
            ),
            Some(Ok((python, names))) => {
                self.with_ui(|ui| {
                    let tools = ui.global::<crate::ToolsApi>();
                    tools.set_tool_open(true);
                    tools.set_tool_tab(0);
                });
                self.append_output(format!("> install {}", names.join(" ")), 2);
                packages::install(&python, names, self.sink.clone());
                self.render_pane(pane);
            }
        }
    }

    /// The banner's "Ignore".
    fn ignore_missing_packages(&self, pane: i32) {
        {
            let mut st = self.state.borrow_mut();
            if let Some(ws) = st.workspace.as_mut() {
                let (_, pkgs) = packages::active_missing(ws);
                ws.packages.ignored.extend(
                    pkgs.iter()
                        .map(|p| apex_python::packages::normalize(&p.pypi)),
                );
            }
        }
        self.render_pane(pane);
    }

    /// A server completion reply arrived.
    pub(crate) fn on_completion(&self, reply: complete::CompletionReply) {
        let pane = {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let Some(tab) = st.workspace.as_ref().and_then(Workspace::active_tab) else {
                return;
            };
            let mut found = None;
            for (i, p) in st.editor.panes.iter_mut().enumerate() {
                if let Some(popup) = p.popup.as_mut().filter(|pp| pp.request == reply.request) {
                    let caret = tab.view.cursor.sel.head;
                    let prefix = tab
                        .buffer
                        .slice(popup.word_start..caret.max(popup.word_start))
                        .unwrap_or_default();
                    popup.merge_lsp(reply.items, &prefix);
                    found = Some(i);
                    break;
                }
            }
            found
        };
        if let Some(pane) = pane {
            self.render_pane(to_i32(pane));
        }
    }

    fn refilter_popup(&self, pane: i32) {
        self.refilter_popup_quiet(pane);
        self.render_pane(pane);
    }

    /// Re-filters the open popup for the word at the caret (closing it when
    /// the caret left the word), without rendering.
    fn refilter_popup_quiet(&self, pane: i32) {
        {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let Some(tab) = st.workspace.as_ref().and_then(Workspace::active_tab) else {
                return;
            };
            let Some(p) = st.editor.panes.get_mut(pane as usize) else {
                return;
            };
            let caret = tab.view.cursor.sel.head;
            let keep = p.popup.as_mut().and_then(|popup| {
                if caret < popup.word_start {
                    return None;
                }
                let prefix = tab.buffer.slice(popup.word_start..caret).ok()?;
                if !prefix.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    return None;
                }
                if prefix.is_empty() && !popup.explicit {
                    let after_dot =
                        popup.word_start > 0 && tab.buffer.rope().char(popup.word_start - 1) == '.';
                    if !after_dot {
                        return None;
                    }
                }
                popup.refilter(&prefix);
                Some(())
            });
            if keep.is_none() {
                p.popup = None;
            }
        }
    }

    /// Inserts the selected completion.
    fn accept_completion(&self, pane: i32) {
        let done = {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let Some(popup) = st
                .editor
                .panes
                .get_mut(pane as usize)
                .and_then(|p| p.popup.take())
            else {
                return;
            };
            let Some(item) = popup.current().cloned() else {
                return;
            };
            let Some(tab) = st.workspace.as_mut().and_then(Workspace::active_tab_mut) else {
                return;
            };
            sync_tab(tab);
            let caret = tab.view.cursor.sel.head;
            let start = popup.word_start.min(caret);
            let was_dirty = tab.buffer.is_dirty();
            let b = &mut tab.buffer;
            b.break_undo_group();
            let result = if item.is_snippet {
                let (line, _) = line_col(b, start);
                let line_text = b.line(line).unwrap_or_default();
                let indent: String = line_text
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                let mut vars = Variables::new();
                if let Some(name) = tab.path.file_name() {
                    vars.insert("TM_FILENAME".into(), name.to_string_lossy().into_owned());
                }
                if let Some(stem) = tab.path.file_stem() {
                    vars.insert(
                        "TM_FILENAME_BASE".into(),
                        stem.to_string_lossy().into_owned(),
                    );
                }
                let snip = snippet::expand(&item.insert, &indent, &vars);
                b.replace_range(start..caret, &snip.text).map(|()| {
                    let (session, first) = SnippetSession::start(&snip, start);
                    (Selection::new(first.start, first.end), session)
                })
            } else {
                b.replace_range(start..caret, &item.insert)
                    .map(|()| (Selection::caret(start + item.insert.chars().count()), None))
            };
            b.break_undo_group();
            match result {
                Ok((sel, session)) => {
                    tab.view.cursor = Cursor::with(sel);
                    tab.view.snippet = session;
                    tab.view.snippet_rev = tab.buffer.revision();
                    if let Some(p) = st.editor.panes.get_mut(pane as usize) {
                        p.caret_serial += 1;
                        reveal(p, tab, false);
                    }
                    Ok(was_dirty != tab.buffer.is_dirty())
                }
                Err(e) => Err(e.to_string()),
            }
        };
        match done {
            Ok(dirty_changed) => {
                self.text_changed();
                if dirty_changed {
                    self.push_tabs();
                }
                self.render_pane(pane);
            }
            Err(e) => self.notify(NoticeKind::Error, format!("Completion failed: {e}")),
        }
    }
}

/// Char index for "go to line `line`, column `col`" (0-based; without a
/// column, the first non-blank char). Both are clamped: "Go to 3:500" stops
/// at the line end instead of landing on another line or past the text.
fn goto_index(b: &apex_editor::Buffer, line: usize, col: Option<usize>) -> usize {
    let line = line.min(b.len_lines().saturating_sub(1));
    let text = b.line(line).unwrap_or_default();
    let first = text.chars().take_while(|c| c.is_whitespace()).count();
    let len = text.chars().count();
    b.rope().line_to_char(line) + col.unwrap_or(first).min(len)
}

/// Whether the caret sits inside a string or comment token.
/// Identifies a file for the completion index's identifier cache.
fn file_key(path: &std::path::Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    h.finish()
}

/// Lines around the caret kept in step with typing while the full scan of
/// the file's identifiers is refreshed in the background.
const NEARBY_LINES: usize = 60;

/// The text of the lines around `caret` (see [`NEARBY_LINES`]).
fn nearby_text(b: &apex_editor::Buffer, caret: usize) -> String {
    let rope = b.rope();
    let line = rope.char_to_line(caret.min(rope.len_chars()));
    let first = line.saturating_sub(NEARBY_LINES);
    let last = (line + NEARBY_LINES + 1).min(rope.len_lines());
    rope.slice(rope.line_to_char(first)..rope.line_to_char(last))
        .to_string()
}

fn in_string_or_comment(tab: &mut Tab, caret: usize) -> bool {
    let (line, col) = line_col(&tab.buffer, caret);
    ensure_highlight(tab, &[line..line + 1]);
    let text = tab.buffer.line(line).unwrap_or_default();
    let vcol = visual_col(&text, col);
    let Some(runs) = tab.view.cache.get(line) else {
        return false;
    };
    runs.iter().any(|r| {
        let end = r.col + r.text.chars().count();
        matches!(r.kind, TokenKind::String | TokenKind::Comment) && r.col < vcol && vcol <= end
    })
}

/// Recomputes the find matches of pane `p` if the text or query changed.
fn refresh_matches(p: &mut PaneUi, tab: &Tab) {
    let rev = tab.buffer.revision();
    if p.matches_rev == Some(rev) {
        return;
    }
    p.matches = if p.prompt_mode == 1 && !p.prompt_text.is_empty() {
        tab.buffer
            .find_all(&p.prompt_text, apex_editor::SearchOptions::default())
    } else {
        Vec::new()
    };
    p.matches_rev = Some(rev);
}

/// Indent guide levels of `line`: its own indentation, or for a blank line
/// the smaller indentation of the nearest non-blank lines around it
/// (looking at most 64 lines each way, so it stays cheap).
fn guide_levels(b: &apex_editor::Buffer, line: usize) -> usize {
    use apex_editor::outline::indent_width;
    let width = |l: usize| indent_width(&b.line(l).unwrap_or_default());
    if let Some(w) = width(line) {
        return w / cursor::TAB_WIDTH;
    }
    let n = b.len_lines();
    let up = (line.saturating_sub(64)..line)
        .rev()
        .find_map(width)
        .unwrap_or(0);
    let down = (line + 1..(line + 64).min(n)).find_map(width).unwrap_or(0);
    up.min(down) / cursor::TAB_WIDTH
}

/// Opens folds that hide `idx`.
fn unfold_around(tab: &mut Tab, idx: usize) {
    let (line, _) = line_col(&tab.buffer, idx);
    let hiding: Vec<usize> = tab
        .view
        .folded
        .iter()
        .copied()
        .filter(|s| {
            tab.view
                .outline
                .fold_at(*s)
                .is_some_and(|f| f.start < line && line <= f.end)
        })
        .collect();
    for s in hiding {
        tab.view.folded.remove(&s);
    }
}

/// After folding: a caret inside a folded block moves to its header.
fn move_caret_out_of_folds(tab: &mut Tab) {
    let map = fold_map(tab);
    let (line, _) = line_col(&tab.buffer, tab.view.cursor.sel.head);
    if map.is_hidden(line) {
        let header = map.line_of_row(map.row_of_line(line));
        let b = &tab.buffer;
        let end = b.rope().line_to_char(header) + b.line_len(header).unwrap_or(0);
        tab.view.cursor = Cursor::at(end);
    }
}

/// Scrolls pane `p` so the caret of `tab` is visible.
fn reveal(p: &mut PaneUi, tab: &Tab, center: bool) {
    let map = fold_map(tab);
    let head = tab.view.cursor.sel.head.min(tab.buffer.len_chars());
    let (line, _) = line_col(&tab.buffer, head);
    let row = map.row_of_line(line);
    let col = cursor::visual_col_of(&tab.buffer, head);
    let target = if center {
        let (_, sy) = p.viewport.center_on(row);
        let (sx, _) = p.viewport.reveal(row, col).unwrap_or((p.viewport.sx, 0.0));
        if (sy - p.viewport.sy).abs() < 1.0 && (sx - p.viewport.sx).abs() < 1.0 {
            None
        } else {
            Some((sx, sy))
        }
    } else {
        p.viewport.reveal(row, col)
    };
    if let Some((sx, sy)) = target {
        p.viewport.sx = sx;
        p.viewport.sy = sy;
        p.scroll_serial += 1;
        p.scroll_to = (sx, sy);
        p.pending_scroll = Some((sx, sy));
    }
}

fn build_frame(
    index: usize,
    p: &mut PaneUi,
    panes_model: &Rc<VecModel<EdPane>>,
    ws: Option<&mut Workspace>,
    tab_index: Option<usize>,
    theme: &apex_themes::Theme,
    zoom: i32,
) -> Option<Frame> {
    let mut frame = Frame {
        pane: EdPane {
            zoom,
            popup_items: ModelRc::from(p.popup_rows.clone()),
            ..EdPane::default()
        },
        slot_updates: Vec::new(),
        slot_count: 0,
        reset_lines: false,
        spans: Vec::new(),
        lines_model: p.lines.clone(),
        spans_model: p.spans.clone(),
        popup_rows: Vec::new(),
        popup_model: p.popup_rows.clone(),
        panes_model: panes_model.clone(),
        index,
        caret_text: String::new(),
    };
    let Some(ws) = ws else {
        p.slots.clear();
        return Some(frame);
    };
    let diag = crate::lsp::squiggles(
        &ws.diagnostics,
        tab_index
            .and_then(|i| ws.tabs.get(i))
            .map(|t| t.path.as_path()),
    );
    let (missing, missing_pkgs) = packages::missing_for(ws, tab_index);
    let banner = (packages::banner_text(&missing_pkgs), ws.packages.installing);
    let Some(tab) = tab_index.and_then(|i| ws.tabs.get_mut(i)) else {
        p.slots.clear();
        frame.pane.lines = ModelRc::from(p.lines.clone());
        frame.slot_count = 0;
        return Some(frame);
    };
    sync_tab(tab);
    let map = fold_map(tab);
    let rows = map.rows();
    let vp = p.viewport;
    let page = vp.page_rows();
    let first = vp.first_row();
    let map_rows = (vp.height / 2.0).max(1.0) as usize;
    let mfirst = view::minimap_first(rows, map_rows, first, page);
    {
        // While typing only lines without any runs (the edited ones) are
        // highlighted; the rest of the screen keeps its runs until typing
        // pauses (then every stale row on screen is redone). Overscan rows
        // may show older runs until they scroll into view.
        let hi = (first + page + 2).min(rows);
        let cache = &tab.view.cache;
        let typing = p.typing;
        let mut needed: Vec<usize> = (first.min(hi)..hi)
            .map(|r| map.line_of_row(r))
            .filter(|l| {
                if typing {
                    cache.get_any(*l).is_none()
                } else {
                    cache.get(*l).is_none()
                }
            })
            .collect();
        needed.extend(
            (first.saturating_sub(OVERSCAN)..(first + page + 2 + OVERSCAN).min(rows))
                .map(|r| map.line_of_row(r))
                .filter(|l| cache.get_any(*l).is_none()),
        );
        needed.sort_unstable();
        needed.dedup();
        ensure_highlight(tab, &view::line_ranges(needed));
    }
    let b = &tab.buffer;
    let slot_count = page + 2 + 2 * OVERSCAN;
    frame.slot_count = slot_count;
    if p.slots.len() != slot_count {
        p.slots = vec![None; slot_count];
        frame.reset_lines = true;
    }
    let lo = first.saturating_sub(OVERSCAN);
    let hi = (first + page + 2 + OVERSCAN).min(rows);
    let style = |k: TokenKind| crate::theming::syntax_style(theme, k);
    // Rows in the window.
    let mut wanted: Vec<Option<EdLineKey>> = vec![None; slot_count];
    for row in lo..hi {
        let line = map.line_of_row(row);
        let runs = match tab.view.cache.get_any(line) {
            Some(r) => r.to_vec(),
            None => view::plain_runs(&b.line(line).unwrap_or_default()),
        };
        let fold = match tab.view.outline.fold_at(line) {
            Some(_) if tab.view.folded.contains(&line) => 2,
            Some(_) => 1,
            None => 0,
        };
        let guides = guide_levels(b, line);
        wanted[row % slot_count] = Some(EdLineKey {
            row,
            line,
            fold,
            guides: to_i32(guides),
            runs,
        });
    }
    for (slot, want) in wanted.into_iter().enumerate() {
        if p.slots[slot] == want {
            continue;
        }
        let line = match &want {
            Some(k) => EdLine {
                row: to_i32(k.row),
                line: to_i32(k.line),
                runs: ModelRc::new(VecModel::from(
                    k.runs
                        .iter()
                        .map(|r| {
                            let s = style(r.kind);
                            EdRun {
                                text: SharedString::from(r.text.as_str()),
                                col: to_i32(r.col),
                                kind: token_index(r.kind),
                                bold: s.bold,
                                italic: s.italic,
                            }
                        })
                        .collect::<Vec<_>>(),
                )),
                fold: k.fold,
                guides: k.guides,
                visible: true,
            },
            None => EdLine::default(),
        };
        frame.slot_updates.push((slot, line));
        p.slots[slot] = want;
    }

    // Caret and spans.
    let sel = p
        .cursor
        .filter(|_| !p.has_keys)
        .unwrap_or(tab.view.cursor)
        .sel
        .clamped(b.len_chars());
    let head = sel.head;
    let (cline, ccol) = line_col(b, head);
    let caret_row = map.row_of_line(cline);
    let caret_vcol = visual_col(&b.line(cline).unwrap_or_default(), ccol);
    let mut spans: Vec<Span> = view::selection_spans(b, &map, sel, lo..hi);
    if sel.is_empty() {
        if let Some((a, z)) = matching_bracket(b, head, BRACKET_SCAN) {
            for i in [a, z] {
                let (l, c) = line_col(b, i);
                if map.is_hidden(l) {
                    continue;
                }
                let t = b.line(l).unwrap_or_default();
                spans.push(Span {
                    row: map.row_of_line(l),
                    col: visual_col(&t, c),
                    cols: 1,
                    kind: 1,
                });
            }
        }
    }
    if let Some(s) = &tab.view.snippet {
        if let Some(r) = s.current() {
            let (l, c) = line_col(b, r.start);
            let t = b.line(l).unwrap_or_default();
            let (l2, c2) = line_col(b, r.end);
            if l == l2 && !map.is_hidden(l) {
                let a = visual_col(&t, c);
                spans.push(Span {
                    row: map.row_of_line(l),
                    col: a,
                    cols: (visual_col(&t, c2) - a).max(1),
                    kind: 4,
                });
            }
        }
    }
    let mut ed_spans: Vec<EdSpan> = spans
        .into_iter()
        .map(|s| EdSpan {
            row: to_i32(s.row),
            col: to_i32(s.col),
            cols: to_i32(s.cols),
            kind: s.kind,
            path: SharedString::new(),
        })
        .collect();
    // Squiggles: diagnostics and missing packages.
    let mut errors = 0;
    let mut warnings = 0;
    let squiggle = |line: usize, c0: usize, c1: Option<usize>, kind: i32, out: &mut Vec<EdSpan>| {
        if line >= b.len_lines() || map.is_hidden(line) {
            return;
        }
        let row = map.row_of_line(line);
        if row < lo || row >= hi {
            return;
        }
        let t = b.line(line).unwrap_or_default();
        let len = t.chars().count();
        let a = visual_col(&t, c0.min(len));
        let z = visual_col(&t, c1.unwrap_or(len).min(len)).max(a + 1);
        let cols = z - a;
        out.push(EdSpan {
            row: to_i32(row),
            col: to_i32(a),
            cols: to_i32(cols),
            kind,
            path: view::wave_path(cols).into(),
        });
    };
    for d in &diag {
        if d.error {
            errors += 1;
        } else {
            warnings += 1;
        }
        let start = b
            .utf16_position_to_char(apex_editor::Position::new(d.line, d.col))
            .unwrap_or(0);
        let end = b
            .utf16_position_to_char(apex_editor::Position::new(d.end_line, d.end_col))
            .unwrap_or(start);
        let (l0, c0) = line_col(b, start);
        let (l1, c1) = line_col(b, end.max(start));
        for l in l0..=l1.min(l0 + 200) {
            let a = if l == l0 { c0 } else { 0 };
            let z = if l == l1 { Some(c1) } else { None };
            squiggle(l, a, z, if d.error { 2 } else { 3 }, &mut ed_spans);
        }
    }
    // Find matches on screen.
    refresh_matches(p, tab);
    let first_match = p
        .matches
        .partition_point(|m| b.rope().char_to_line(m.end.min(b.len_chars())) < map.line_of_row(lo));
    for m in p.matches[first_match..].iter().take(500) {
        let (l0, c0) = line_col(b, m.start);
        let (l1, c1) = line_col(b, m.end);
        if l0 != l1 || map.is_hidden(l0) {
            continue;
        }
        let row = map.row_of_line(l0);
        if row >= hi {
            break;
        }
        let t = b.line(l0).unwrap_or_default();
        let a = visual_col(&t, c0);
        ed_spans.push(EdSpan {
            row: to_i32(row),
            col: to_i32(a),
            cols: to_i32(visual_col(&t, c1).saturating_sub(a).max(1)),
            kind: 6,
            path: SharedString::new(),
        });
    }
    for (line, c0, c1, _) in &missing {
        squiggle(*line, *c0, Some(*c1), 5, &mut ed_spans);
    }
    // Quick fix on the caret line: "Install 'package'".
    if let Some((line, _, _, pkg)) = missing.iter().find(|m| m.0 == cline) {
        let t = b.line(*line).unwrap_or_default();
        frame.pane.quickfix = format!("Install package '{pkg}'").into();
        frame.pane.quickfix_row = to_i32(map.row_of_line(*line));
        frame.pane.quickfix_col = to_i32(apex_editor::cursor::display_width(&t) + 2);
        p.quickfix = Some(pkg.clone());
    } else {
        p.quickfix = None;
    }
    frame.spans = ed_spans;

    // Popup.
    if let Some(popup) = p.popup.as_ref().filter(|pp| !pp.is_empty()) {
        let (wl, wc) = line_col(b, popup.word_start.min(b.len_chars()));
        let items: Vec<EdCompletion> = popup
            .visible()
            .map(|(it, _)| EdCompletion {
                label: it.label.as_str().into(),
                detail: it.detail.lines().next().unwrap_or_default().into(),
                badge: it.kind.badge().into(),
                kind: completion_kind_index(it.kind),
            })
            .collect();
        frame.pane.popup_open = true;
        frame.popup_rows = items;
        frame.pane.popup_selected = to_i32(popup.selected.saturating_sub(popup.first));
        frame.pane.popup_total = to_i32(popup.filtered.len());
        frame.pane.popup_row = to_i32(map.row_of_line(wl));
        frame.pane.popup_col = to_i32(visual_col(&b.line(wl).unwrap_or_default(), wc));
        frame.pane.popup_doc = popup
            .current()
            .map(complete::doc_text)
            .unwrap_or_default()
            .into();
    }

    // Navigation bar.
    let (class, func) = tab.view.outline.scope_names(cline);
    frame.pane.nav_class = class.into();
    frame.pane.nav_func = func.into();
    let nav: Vec<EdNavItem> = tab
        .view
        .outline
        .defs
        .iter()
        .map(|(s, depth)| EdNavItem {
            label: s.name.as_str().into(),
            line: to_i32(s.line),
            depth: to_i32(*depth),
            is_class: s.kind == "class",
        })
        .collect();
    // The scope (left) drop-down lists classes and top-level definitions.
    let classes: Vec<EdNavItem> = nav
        .iter()
        .filter(|n| n.is_class || n.depth == 0)
        .cloned()
        .collect();
    frame.pane.nav_items = ModelRc::new(VecModel::from(nav));
    frame.pane.nav_classes = ModelRc::new(VecModel::from(classes));

    // Minimap.
    let key = (b.revision(), mfirst, map_rows, rows);
    // While typing, the minimap repaints only when it scrolls; edits show
    // up in it when typing pauses.
    let moved = p
        .minimap_key
        .is_none_or(|k| (k.1, k.2, k.3) != (key.1, key.2, key.3));
    if moved || (p.idle_render && p.minimap_key != Some(key)) {
        let mut lines = Vec::new();
        for row in mfirst..(mfirst + map_rows).min(rows) {
            let line = map.line_of_row(row);
            lines.push(match tab.view.cache.get_any(line) {
                Some(r) => r.to_vec(),
                None => view::plain_runs(&b.line(line).unwrap_or_default()),
            });
        }
        let (w, h, px) = view::paint_minimap(&lines, |k| {
            let c = style(k).color;
            [c.r, c.g, c.b, 0xC0]
        });
        let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&px, w as u32, h as u32);
        p.minimap = Image::from_rgba8(buf);
        p.minimap_first = mfirst;
        p.minimap_key = Some(key);
    }

    let has_tabs = tab.view.outline.uses_tabs;
    let pane = &mut frame.pane;
    pane.has_file = true;
    pane.lines = ModelRc::from(p.lines.clone());
    pane.spans = ModelRc::from(p.spans.clone());
    pane.rows = to_i32(rows);
    pane.max_cols = to_i32(tab.view.outline.max_cols.max(caret_vcol + 1));
    pane.gutter_digits = to_i32(b.len_lines().to_string().len());
    pane.caret_row = to_i32(caret_row);
    pane.caret_col = to_i32(caret_vcol);
    pane.caret_serial = p.caret_serial;
    pane.show_current_line = sel.is_empty();
    pane.scroll_serial = p.scroll_serial;
    pane.scroll_to_x = p.scroll_to.0;
    pane.scroll_to_y = p.scroll_to.1;
    pane.focus_serial = p.focus_serial;
    pane.errors = errors;
    pane.warnings = warnings;
    pane.ln = to_i32(cline + 1);
    pane.ch = to_i32(ccol + 1);
    pane.indent_text = if has_tabs { "TABS" } else { "SPC" }.into();
    pane.eol_text = if b.has_mixed_line_endings() {
        "Mixed".into()
    } else {
        b.line_ending().label().into()
    };
    pane.minimap = p.minimap.clone();
    pane.minimap_first = to_i32(p.minimap_first);
    pane.banner = banner.0.into();
    pane.banner_busy = banner.1;
    pane.prompt_mode = p.prompt_mode;
    pane.prompt_text = p.prompt_text.as_str().into();
    pane.prompt_info = p.prompt_info.as_str().into();
    pane.prompt_serial = p.prompt_serial;
    frame.caret_text = format!("{}:{}", cline + 1, ccol + 1);
    Some(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_editor::Buffer;
    use std::path::PathBuf;

    fn tab(text: &str) -> Tab {
        let mut b = Buffer::from_text(text);
        b.set_path(Some(PathBuf::from("x.py")));
        Tab::new(PathBuf::from("x.py"), b)
    }

    #[test]
    fn sync_keeps_folds_and_snippets_in_step() {
        let mut t = tab("def f():\n    pass\n\nx = 1\n");
        sync_tab(&mut t);
        ensure_highlight(&mut t, &[0..1]);
        assert!(t.view.cache.get(0).is_some());
        t.view.folded.insert(0);
        // Insert two lines above: the fold header moves down.
        t.buffer.insert(0, "a = 1\nb = 2\n").unwrap();
        sync_tab(&mut t);
        assert!(t.view.folded.contains(&2));
        assert_eq!(fold_map(&t).rows(), t.buffer.len_lines() - 1);
        let at3 = t.buffer.line_to_char(3).unwrap();
        unfold_around(&mut t, at3);
        assert!(t.view.folded.is_empty());

        // A snippet session follows typing in its placeholder.
        let snip = snippet::Snippet::parse("for ${1:x} in ${2:xs}:");
        let at = t.buffer.len_chars();
        t.buffer.insert(at, &snip.text).unwrap();
        let (s, first) = SnippetSession::start(&snip, at);
        t.view.snippet = s;
        t.view.snippet_rev = t.buffer.revision();
        t.buffer.replace_range(first, "item").unwrap();
        sync_tab(&mut t);
        let s = t.view.snippet.as_mut().unwrap();
        assert_eq!(s.current(), Some(at + 4..at + 8));
    }

    #[test]
    fn caret_moves_out_of_a_new_fold() {
        let mut t = tab("if x:\n    a = 1\n    b = 2\n");
        sync_tab(&mut t);
        t.view.cursor = Cursor::at(t.buffer.line_to_char(2).unwrap() + 3);
        t.view.folded.insert(0);
        move_caret_out_of_folds(&mut t);
        assert_eq!(t.view.cursor.sel.head, 5);
    }

    /// Regression: editing must keep working after other UI activity (the
    /// old TextInput-based editor could stop accepting edits after a Run).
    /// Edits here depend only on the tab's own buffer and cursor.
    #[test]
    fn edits_apply_after_save_and_reload_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.py");
        std::fs::write(&path, "print(1)\n").unwrap();
        let mut t = Tab::new(path.clone(), Buffer::open(&path).unwrap());
        sync_tab(&mut t);
        t.view.cursor = Cursor::at(t.buffer.len_chars());
        let s = commands::type_text(&mut t.buffer, t.view.cursor.sel, "x", true).unwrap();
        t.view.cursor = Cursor::with(s);
        t.buffer.save().unwrap();
        // "Run" saves all files; editing and deleting must still work.
        let s = commands::type_text(&mut t.buffer, t.view.cursor.sel, "y", true).unwrap();
        let s = commands::backspace(&mut t.buffer, s).unwrap();
        let s = commands::backspace(&mut t.buffer, s).unwrap();
        assert_eq!(t.buffer.text(), "print(1)\n");
        assert_eq!(s.head, t.buffer.len_chars());
        sync_tab(&mut t);
        ensure_highlight(&mut t, &[0..1]);
        assert!(t.view.cache.get(0).is_some());
    }

    #[test]
    fn go_to_line_clamps_line_and_column() {
        let b = apex_editor::Buffer::from_text("ab\n  cd\n");
        assert_eq!(goto_index(&b, 1, None), 5);
        assert_eq!(goto_index(&b, 0, Some(500)), 2);
        assert_eq!(goto_index(&b, 1, Some(500)), 7);
        assert_eq!(goto_index(&b, 99, Some(500)), b.len_chars());
        let empty = apex_editor::Buffer::from_text("");
        assert_eq!(goto_index(&empty, 3, Some(4)), 0);
    }

    #[test]
    fn strings_and_comments_suppress_auto_completion() {
        let mut t = tab("x = 'abc'  # note");
        sync_tab(&mut t);
        assert!(in_string_or_comment(&mut t, 7));
        assert!(in_string_or_comment(&mut t, 16));
        assert!(!in_string_or_comment(&mut t, 1));
    }
}

/// Release-mode benchmark of the editing hot path (run with
/// `cargo test --release -p apex-ide bench_editing -- --ignored --nocapture`).
#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    fn frame(p: &mut PaneUi, model: &Rc<VecModel<EdPane>>, ws: &mut Workspace) -> Frame {
        build_frame(
            0,
            p,
            model,
            Some(ws),
            Some(0),
            apex_themes::default_theme(),
            100,
        )
        .unwrap()
    }

    /// Switching a pane to a shorter file must blank the rows the new file
    /// does not fill (they used to keep showing the old file's lines).
    #[test]
    fn switching_to_a_shorter_file_resets_rows() {
        let dir = tempfile::tempdir().unwrap();
        let long = dir.path().join("long.py");
        let short = dir.path().join("short.py");
        std::fs::write(
            &long,
            "a = 1
b = 2
c = 3
d = 4
",
        )
        .unwrap();
        std::fs::write(
            &short, "x = 1
",
        )
        .unwrap();
        let project = apex_core::Project::open(dir.path()).unwrap();
        let mut ws = Workspace::new(project);
        for path in [&long, &short] {
            ws.tabs.push(Tab::new(
                path.to_path_buf(),
                apex_editor::Buffer::open(path).unwrap(),
            ));
        }
        ws.active = Some(0);
        let model = Rc::new(VecModel::from(vec![EdPane::default()]));
        let mut p = PaneUi::default();
        p.viewport.height = 400.0;
        let first = frame(&mut p, &model, &mut ws);
        assert!(!first.reset_lines || first.slot_updates.len() >= 4);
        // What render_pane does with the frame, minus the UI.
        let apply = |f: Frame| {
            if f.reset_lines || f.lines_model.row_count() != f.slot_count {
                f.lines_model.set_vec(
                    (0..f.slot_count)
                        .map(|_| EdLine::default())
                        .collect::<Vec<_>>(),
                );
            }
            for (slot, line) in f.slot_updates {
                f.lines_model.set_row_data(slot, line);
            }
        };
        apply(first);
        let visible = |p: &PaneUi| p.lines.iter().filter(|l| l.visible).count();
        assert_eq!(visible(&p), 5);
        // The pane shows another file: render_pane clears the slots.
        p.slots.clear();
        ws.active = Some(1);
        let second = build_frame(
            0,
            &mut p,
            &model,
            Some(&mut ws),
            Some(1),
            apex_themes::default_theme(),
            100,
        )
        .unwrap();
        assert!(second.reset_lines);
        apply(second);
        assert_eq!(visible(&p), 2, "only short.py's two lines remain");
    }

    #[test]
    #[ignore]
    fn bench_editing() {
        let unit = "class Inventory:\n    \"\"\"Keeps items.\"\"\"\n\n    def add(self, name, count=1):\n        if count <= 0:\n            raise ValueError('count')  # check\n        self.items[name] = self.items.get(name, 0) + count\n        return self.items[name]\n\n\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.py");
        std::fs::write(&path, unit.repeat(2000)).unwrap();
        let project = apex_core::Project::open(dir.path()).unwrap();
        let mut ws = Workspace::new(project);
        ws.tabs.push(Tab::new(
            path.clone(),
            apex_editor::Buffer::open(&path).unwrap(),
        ));
        ws.active = Some(0);
        let model = Rc::new(VecModel::from(vec![EdPane::default()]));
        let mut p = PaneUi::default();
        p.viewport.height = 900.0;
        let lines = ws.tabs[0].buffer.len_lines();

        let t = Instant::now();
        frame(&mut p, &model, &mut ws);
        eprintln!("lines={lines} first render={:?}", t.elapsed());

        // Type 100 chars at the end of a line inside a method body.
        p.typing = true;
        let at = ws.tabs[0].buffer.line_to_char(27).unwrap() - 1;
        ws.tabs[0].view.cursor = Cursor::at(at);
        let mut worst = std::time::Duration::ZERO;
        let t = Instant::now();
        for i in 0..100 {
            let k = Instant::now();
            let tab = &mut ws.tabs[0];
            let s = commands::type_text(
                &mut tab.buffer,
                tab.view.cursor.sel,
                if i % 10 == 9 { " " } else { "x" },
                true,
            )
            .unwrap();
            tab.view.cursor = Cursor::with(s);
            let k1 = Instant::now();
            sync_tab(tab);
            let ks = k1.elapsed();
            let k2 = Instant::now();
            let hi = k2.elapsed();
            let k3 = Instant::now();
            frame(&mut p, &model, &mut ws);
            if i % 25 == 0 {
                eprintln!("  sync={ks:?} highlight={hi:?} frame={:?}", k3.elapsed());
            }
            worst = worst.max(k.elapsed());
        }
        eprintln!("keystroke avg={:?} worst={worst:?}", t.elapsed() / 100);

        p.typing = false;
        let t = Instant::now();
        frame(&mut p, &model, &mut ws);
        eprintln!("render after typing pauses={:?}", t.elapsed());

        // Paste 5k lines.
        let paste = unit.repeat(500);
        let t = Instant::now();
        let tab = &mut ws.tabs[0];
        let s = commands::insert_text(&mut tab.buffer, tab.view.cursor.sel, &paste).unwrap();
        tab.view.cursor = Cursor::with(s);
        sync_tab(tab);
        frame(&mut p, &model, &mut ws);
        eprintln!("paste 5k lines={:?}", t.elapsed());

        // Scroll by one page 50 times.
        let t = Instant::now();
        for i in 0..50 {
            p.viewport.sy = (i * 900) as f32;
            frame(&mut p, &model, &mut ws);
        }
        eprintln!("scroll page avg={:?}", t.elapsed() / 50);
    }
}
