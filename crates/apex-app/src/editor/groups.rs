//! Editor groups in the UI: keeping [`EditorLayout`] in step with the open
//! tabs, the panes model, tab strips, splitters, tab drag and drop, and
//! detached editor windows.

use std::path::PathBuf;
use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use super::layout::{self, EditorLayout, GroupId, Rect, Zone};
use super::PaneUi;
use crate::app::{to_i32, App};
use crate::state::Workspace;
use crate::{AppTheme, EdPane, EdSplitter, EdTab, EditorApi, EditorWindow};
use apex_editor::Cursor;

/// Height of a group's tab strip used for drop targeting (px).
const STRIP_HEIGHT: f32 = 32.0;

/// Width Rust gives a tab with `title` (px); Slint uses it as-is so drop
/// positions map to tab indices.
pub fn tab_width(title: &str) -> f32 {
    (title.chars().count() as f32 * 7.2 + 54.0).clamp(80.0, 240.0)
}

/// Insertion index for a drop at `x` (relative to the strip start).
pub fn insert_index(titles: &[String], x: f32) -> usize {
    let mut left = 0.0;
    for (i, t) in titles.iter().enumerate() {
        let w = tab_width(t);
        if x < left + w / 2.0 {
            return i;
        }
        left += w;
    }
    titles.len()
}

fn title_of(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Copies every theme property of the main window into another window
/// (each Slint window has its own globals).
pub fn copy_theme(src: &AppTheme<'_>, dst: &AppTheme<'_>) {
    macro_rules! copy {
        ($($get:ident => $set:ident),* $(,)?) => { $( dst.$set(src.$get()); )* };
    }
    copy!(
        get_dark => set_dark, get_background => set_background, get_surface => set_surface,
        get_surface_alt => set_surface_alt, get_border => set_border,
        get_foreground => set_foreground, get_foreground_muted => set_foreground_muted,
        get_accent => set_accent, get_accent_foreground => set_accent_foreground,
        get_selection => set_selection, get_error => set_error, get_warning => set_warning,
        get_success => set_success, get_editor_background => set_editor_background,
        get_editor_foreground => set_editor_foreground,
        get_gutter_foreground => set_gutter_foreground, get_line_highlight => set_line_highlight,
        get_editor_font => set_editor_font, get_editor_font_size => set_editor_font_size,
        get_compact => set_compact, get_ui_font => set_ui_font, get_code_font => set_code_font,
        get_code_font_size => set_code_font_size, get_framed => set_framed,
        get_title_bar => set_title_bar, get_title_bar_foreground => set_title_bar_foreground,
        get_frame => set_frame, get_frame_foreground => set_frame_foreground,
        get_tab_active => set_tab_active, get_tab_active_foreground => set_tab_active_foreground,
        get_status_bar => set_status_bar, get_status_bar_foreground => set_status_bar_foreground,
        get_popup_background => set_popup_background, get_popup_border => set_popup_border,
        get_popup_selection => set_popup_selection, get_popup_shadow => set_popup_shadow,
        get_syntax_keyword => set_syntax_keyword, get_syntax_string => set_syntax_string,
        get_syntax_number => set_syntax_number, get_syntax_comment => set_syntax_comment,
        get_syntax_function => set_syntax_function, get_syntax_type => set_syntax_type,
        get_syntax_variable => set_syntax_variable, get_syntax_constant => set_syntax_constant,
        get_syntax_operator => set_syntax_operator,
        get_syntax_punctuation => set_syntax_punctuation,
        get_syntax_decorator => set_syntax_decorator, get_syntax_builtin => set_syntax_builtin,
        get_syntax_plain => set_syntax_plain,
    );
}

impl App {
    /// Index into `ws.tabs` of the file pane `pane` shows.
    pub(crate) fn pane_tab(&self, pane: i32) -> Option<usize> {
        let st = self.state.borrow();
        let group = st.editor.layout.groups.get(pane as usize)?;
        let path = group.active_path()?;
        st.workspace.as_ref()?.tab_index(path)
    }

    /// Makes pane `pane` the one keys go to: its group gets the focus and
    /// its file becomes the workspace's active tab (Save, Run, status bar).
    pub(crate) fn focus_pane(&self, pane: i32) {
        let changed = {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let Some(group) = st.editor.layout.groups.get(pane as usize).map(|g| g.id) else {
                return;
            };
            let path = st.editor.layout.groups[pane as usize]
                .active_path()
                .map(|p| p.to_path_buf());
            let was_focused = st.editor.layout.focused;
            st.editor.layout.focus(group);
            let mut changed = was_focused != st.editor.layout.focused;
            // Each pane keeps its own caret: park the caret of the pane
            // that had the keys and restore this pane's.
            let old = st.editor.panes.iter().position(|p| p.has_keys);
            if old != Some(pane as usize) {
                changed = true;
                if let (Some(o), Some(ws)) = (old, st.workspace.as_ref()) {
                    let path = st.editor.layout.groups.get(o).and_then(|g| g.active_path());
                    let cursor = path
                        .and_then(|p| ws.tab_index(p))
                        .map(|i| ws.tabs[i].view.cursor);
                    st.editor.panes[o].cursor = cursor;
                }
                for (i, p) in st.editor.panes.iter_mut().enumerate() {
                    p.has_keys = i == pane as usize;
                }
                let saved = st
                    .editor
                    .panes
                    .get_mut(pane as usize)
                    .and_then(|p| p.cursor.take());
                if let (Some(c), Some(ws), Some(path)) =
                    (saved, st.workspace.as_mut(), path.as_ref())
                {
                    if let Some(i) = ws.tab_index(path) {
                        let len = ws.tabs[i].buffer.len_chars();
                        ws.tabs[i].view.cursor = Cursor::with(c.sel.clamped(len));
                    }
                }
            }
            if let (Some(ws), Some(path)) = (st.workspace.as_mut(), path) {
                let idx = ws.tab_index(&path);
                if idx.is_some() && ws.active != idx {
                    if let Some(tab) = ws.active_tab_mut() {
                        tab.buffer.break_undo_group();
                    }
                    ws.active = idx;
                    changed = true;
                }
            }
            changed
        };
        if changed {
            self.push_tabs();
            self.render_all_panes();
        }
    }

    /// Re-renders every pane.
    pub(crate) fn render_all_panes(&self) {
        let n = self.state.borrow().editor.panes.len();
        for i in 0..n {
            self.render_pane(to_i32(i));
        }
    }

    /// Brings groups in step with the workspace's tabs and active tab, then
    /// rebuilds panes, geometry and windows, and renders.
    pub(crate) fn sync_groups(&self) {
        {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let layout = &mut st.editor.layout;
            match st.workspace.as_ref() {
                None => *layout = EditorLayout::default(),
                Some(ws) => {
                    // Files closed elsewhere leave every group.
                    let open: Vec<PathBuf> = layout
                        .groups
                        .iter()
                        .flat_map(|g| g.tabs.iter().cloned())
                        .collect();
                    for p in open {
                        if ws.tab_index(&p).is_none() {
                            layout.close_everywhere(&p);
                        }
                    }
                    // Show the active tab where it is, or open it in the
                    // focused group.
                    if let Some(tab) = ws.active_tab() {
                        let focused = layout.focused;
                        let in_focused = layout
                            .group(focused)
                            .and_then(|g| g.tabs.iter().position(|t| *t == tab.path));
                        match (in_focused, layout.group_with(&tab.path)) {
                            (Some(i), _) => layout.activate(focused, i),
                            (None, Some(g)) => {
                                let i = layout
                                    .group(g)
                                    .and_then(|gr| gr.tabs.iter().position(|t| *t == tab.path))
                                    .unwrap_or(0);
                                if layout.is_floating(g) {
                                    if let Some(gr) = layout.groups.iter_mut().find(|x| x.id == g) {
                                        gr.active = Some(i);
                                    }
                                } else {
                                    layout.activate(g, i);
                                }
                            }
                            (None, None) => layout.open(&tab.path),
                        }
                    }
                }
            }
        }
        self.rebuild_panes();
        self.render_all_panes();
    }

    /// Makes the panes (UI state and model rows) match the layout's groups
    /// and recomputes geometry.
    pub(crate) fn rebuild_panes(&self) {
        let (model, count_changed, splitters, windows_to_open, windows_to_close) = {
            let mut st = self.state.borrow_mut();
            let st = &mut *st;
            let ed = &mut st.editor;
            let mut old: Vec<PaneUi> = std::mem::take(&mut ed.panes);
            let mut panes = Vec::with_capacity(ed.layout.groups.len());
            for g in &ed.layout.groups {
                let pane = match old.iter().position(|p| p.group == g.id) {
                    Some(i) => old.swap_remove(i),
                    None => PaneUi {
                        group: g.id,
                        ..PaneUi::default()
                    },
                };
                panes.push(pane);
            }
            ed.panes = panes;
            if !ed.panes.iter().any(|p| p.has_keys) {
                let focused = ed.layout.index_of(ed.layout.focused).unwrap_or(0);
                if let Some(p) = ed.panes.get_mut(focused) {
                    p.has_keys = true;
                }
            }
            let (rects, splitters) = ed.layout.rects(ed.area.2, ed.area.3);
            for p in &mut ed.panes {
                p.rect = rects
                    .iter()
                    .find(|(id, _)| *id == p.group)
                    .map(|(_, r)| *r)
                    .unwrap_or(Rect {
                        x: 0.0,
                        y: 0.0,
                        w: ed.area.2,
                        h: ed.area.3,
                    });
            }
            ed.splitters = splitters;
            let count_changed = ed.panes_model.row_count() != ed.panes.len();
            let wanted: Vec<GroupId> = ed.layout.floating.clone();
            let to_open: Vec<GroupId> = wanted
                .iter()
                .copied()
                .filter(|g| !ed.windows.iter().any(|(id, _)| id == g))
                .collect();
            let mut to_close = Vec::new();
            ed.windows.retain(|(id, w)| {
                let keep = wanted.contains(id);
                if !keep {
                    to_close.push(w.clone_strong());
                }
                keep
            });
            let sp: Vec<EdSplitter> = ed
                .splitters
                .iter()
                .map(|s| EdSplitter {
                    x: s.rect.x,
                    y: s.rect.y,
                    w: s.rect.w,
                    h: s.rect.h,
                    vertical: s.dir == layout::Dir::Column,
                })
                .collect();
            (ed.panes_model.clone(), count_changed, sp, to_open, to_close)
        };
        if count_changed {
            let n = self.state.borrow().editor.panes.len();
            model.set_vec((0..n).map(|_| EdPane::default()).collect::<Vec<_>>());
        }
        self.with_ui(|ui| {
            ui.global::<EditorApi>()
                .set_splitters(ModelRc::new(VecModel::from(splitters)));
        });
        for w in windows_to_close {
            let _ = w.hide();
        }
        for g in windows_to_open {
            self.open_window(g);
        }
        // Pane indices may have moved: tell every window which pane it shows.
        let st = self.state.borrow();
        for (id, w) in &st.editor.windows {
            if let Some(i) = st.editor.layout.index_of(*id) {
                w.set_pane(to_i32(i));
            }
        }
    }

    fn open_window(&self, group: GroupId) {
        let Ok(win) = EditorWindow::new() else {
            tracing::warn!("cannot create an editor window");
            return;
        };
        let Some(app) = self.state.borrow().editor.app.upgrade() else {
            return;
        };
        let model = self.state.borrow().editor.panes_model.clone();
        win.global::<EditorApi>().set_panes(ModelRc::from(model));
        if let Some(ui) = self.ui.upgrade() {
            copy_theme(&ui.global::<AppTheme>(), &win.global::<AppTheme>());
        }
        app.wire_api(&win.global::<EditorApi>());
        let weak = Rc::downgrade(&app);
        win.window().on_close_requested(move || {
            if let Some(app) = weak.upgrade() {
                app.dock_group(group);
            }
            slint::CloseRequestResponse::HideWindow
        });
        if let Some(i) = self.state.borrow().editor.layout.index_of(group) {
            win.set_pane(to_i32(i));
        }
        if let Err(e) = win.show() {
            tracing::warn!(%e, "showing an editor window");
        }
        self.state.borrow_mut().editor.windows.push((group, win));
    }

    /// Copies the current theme into detached windows.
    pub(crate) fn theme_windows(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let st = self.state.borrow();
        for (_, w) in &st.editor.windows {
            copy_theme(&ui.global::<AppTheme>(), &w.global::<AppTheme>());
        }
    }

    fn dock_group(&self, group: GroupId) {
        self.state.borrow_mut().editor.layout.dock(group);
        self.rebuild_panes();
        self.render_all_panes();
    }

    /// `EditorApi.area-resized`.
    pub(crate) fn area_resized(&self, x: f32, y: f32, w: f32, h: f32) {
        {
            let mut st = self.state.borrow_mut();
            if st.editor.area == (x, y, w, h) {
                return;
            }
            st.editor.area = (x, y, w, h);
        }
        self.rebuild_panes();
        self.render_all_panes();
    }

    /// `EditorApi.tab-clicked`.
    pub(crate) fn group_tab_clicked(&self, pane: i32, tab: i32) {
        {
            let mut st = self.state.borrow_mut();
            let layout = &mut st.editor.layout;
            let Some(id) = layout.groups.get(pane as usize).map(|g| g.id) else {
                return;
            };
            if layout.is_floating(id) {
                if let Some(g) = layout.groups.iter_mut().find(|g| g.id == id) {
                    if (tab as usize) < g.tabs.len() {
                        g.active = Some(tab as usize);
                    }
                }
            } else {
                layout.activate(id, tab.max(0) as usize);
            }
        }
        self.focus_pane(pane);
        self.load_editor();
    }

    /// `EditorApi.tab-close`.
    pub(crate) fn group_tab_close(&self, pane: i32, tab: i32) {
        let (path, elsewhere, ws_index) = {
            let st = self.state.borrow();
            let Some(g) = st.editor.layout.groups.get(pane as usize) else {
                return;
            };
            let Some(path) = g.tabs.get(tab.max(0) as usize).cloned() else {
                return;
            };
            let elsewhere = st
                .editor
                .layout
                .groups
                .iter()
                .filter(|o| o.id != g.id)
                .any(|o| o.tabs.contains(&path));
            let idx = st.workspace.as_ref().and_then(|ws| ws.tab_index(&path));
            (path, elsewhere, idx)
        };
        if elsewhere {
            {
                let mut st = self.state.borrow_mut();
                let id = st.editor.layout.groups[pane as usize].id;
                st.editor.layout.close(id, &path);
            }
            self.rebuild_panes();
            self.render_all_panes();
        } else if let Some(i) = ws_index {
            self.close_tab(i);
        }
    }

    /// `EditorApi.group-action`.
    pub(crate) fn group_action(&self, pane: i32, action: &str) {
        let target = {
            let st = self.state.borrow();
            st.editor.layout.groups.get(pane as usize).map(|g| {
                (
                    g.id,
                    g.active_path().map(|p| p.to_path_buf()),
                    g.tabs.clone(),
                )
            })
        };
        let Some((id, path, tabs)) = target else {
            return;
        };
        match action {
            "split-right" | "split-down" => {
                let zone = if action == "split-right" {
                    Zone::Right
                } else {
                    Zone::Bottom
                };
                let mut st = self.state.borrow_mut();
                st.editor.layout.split(id, zone, path.as_deref(), true);
            }
            "new-window" => {
                let Some(path) = path else { return };
                self.state.borrow_mut().editor.layout.detach(id, &path);
            }
            "dock" => {
                self.dock_group(id);
                return;
            }
            "close-others" => {
                for t in tabs {
                    if Some(&t) != path.as_ref() {
                        let mut st = self.state.borrow_mut();
                        let elsewhere = st
                            .editor
                            .layout
                            .groups
                            .iter()
                            .filter(|o| o.id != id)
                            .any(|o| o.tabs.contains(&t));
                        let idx = st.workspace.as_ref().and_then(|ws| ws.tab_index(&t));
                        let dirty = idx
                            .and_then(|i| {
                                st.workspace.as_ref().map(|ws| ws.tabs[i].buffer.is_dirty())
                            })
                            .unwrap_or(false);
                        if elsewhere || !dirty {
                            st.editor.layout.close(id, &t);
                            if !elsewhere {
                                drop(st);
                                if let Some(i) = idx {
                                    self.close_tab(i);
                                }
                            }
                        }
                    }
                }
            }
            _ => return,
        }
        self.rebuild_panes();
        let new_pane = {
            let st = self.state.borrow();
            st.editor.layout.index_of(st.editor.layout.focused)
        };
        if let Some(p) = new_pane {
            self.focus_pane(to_i32(p));
            if let Some(pu) = self.state.borrow_mut().editor.panes.get_mut(p) {
                pu.focus_serial += 1;
            }
        }
        self.render_all_panes();
    }

    /// Editor commands that change the layout (`editor.splitRight`, ...).
    pub(crate) fn layout_command(&self, pane: i32, id: &str) -> bool {
        let action = match id {
            "editor.splitRight" => "split-right",
            "editor.splitDown" => "split-down",
            "editor.moveToNewWindow" => "new-window",
            _ => return false,
        };
        self.group_action(pane, action);
        true
    }

    /// `EditorApi.splitter-drag`.
    pub(crate) fn splitter_drag(&self, index: i32, delta: f32) {
        {
            let mut st = self.state.borrow_mut();
            let ed = &mut st.editor;
            let Some(s) = ed.splitters.get(index.max(0) as usize).cloned() else {
                return;
            };
            ed.layout.resize(&s.path, s.index, delta, s.extent);
        }
        self.rebuild_panes();
        self.render_all_panes();
    }

    /// `EditorApi.tab-drag`: shows the drop preview while moving and
    /// applies the drop on release.
    pub(crate) fn tab_drag(&self, pane: i32, tab: i32, wx: f32, wy: f32, released: bool) {
        let plan = {
            let mut st = self.state.borrow_mut();
            let ed = &mut st.editor;
            let Some(src) = ed.layout.groups.get(pane as usize) else {
                return;
            };
            if ed.layout.is_floating(src.id) {
                return;
            }
            let Some(path) = src.tabs.get(tab.max(0) as usize).cloned() else {
                return;
            };
            let from = src.id;
            let (ax, ay) = (wx - ed.area.0, wy - ed.area.1);
            let (rects, _) = ed.layout.rects(ed.area.2, ed.area.3);
            let hit = rects
                .iter()
                .find(|(_, r)| ax >= r.x && ax < r.x + r.w && ay >= r.y && ay < r.y + r.h);
            let target = hit.map(|(id, r)| {
                if ay - r.y < STRIP_HEIGHT {
                    let titles: Vec<String> = ed
                        .layout
                        .group(*id)
                        .map(|g| g.tabs.iter().map(|t| title_of(t)).collect())
                        .unwrap_or_default();
                    let index = insert_index(&titles, ax - r.x);
                    (
                        *id,
                        Zone::Center,
                        index,
                        Rect {
                            h: STRIP_HEIGHT,
                            ..*r
                        },
                    )
                } else {
                    let zone = layout::zone_at(*r, ax, ay);
                    (*id, zone, usize::MAX, layout::zone_rect(*r, zone))
                }
            });
            (from, path, target, (ax, ay))
        };
        let (from, path, target, (ax, ay)) = plan;
        if !released {
            self.with_ui(|ui| {
                let api = ui.global::<EditorApi>();
                match target {
                    Some((_, _, _, r)) => {
                        api.set_drop_x(r.x);
                        api.set_drop_y(r.y);
                        api.set_drop_w(r.w);
                        api.set_drop_h(r.h);
                        api.set_drop_visible(true);
                    }
                    None => api.set_drop_visible(false),
                }
            });
            return;
        }
        self.with_ui(|ui| ui.global::<EditorApi>().set_drop_visible(false));
        {
            let mut st = self.state.borrow_mut();
            let (w, h) = (st.editor.area.2, st.editor.area.3);
            let layout = &mut st.editor.layout;
            match target {
                Some((to, Zone::Center, index, _)) => layout.move_tab(from, &path, to, index),
                Some((to, zone, _, _)) => {
                    if let Some(new) = layout.split_empty(to, zone) {
                        layout.move_tab(from, &path, new, 0);
                    }
                }
                None if ax < -20.0 || ay < -20.0 || ax > w + 20.0 || ay > h + 20.0 => {
                    layout.detach(from, &path);
                }
                None => return,
            }
        }
        self.rebuild_panes();
        let focused = {
            let st = self.state.borrow();
            st.editor.layout.index_of(st.editor.layout.focused)
        };
        if let Some(p) = focused {
            self.focus_pane(to_i32(p));
        }
        self.render_all_panes();
    }

    /// Tab strip contents and geometry for pane `p`.
    pub(crate) fn group_fields(&self, pane: &mut EdPane, index: usize) {
        let st = self.state.borrow();
        let ed = &st.editor;
        let Some(g) = ed.layout.groups.get(index) else {
            return;
        };
        let dirty = |path: &std::path::Path| {
            st.workspace
                .as_ref()
                .and_then(|ws: &Workspace| ws.tab_index(path).map(|i| ws.tabs[i].buffer.is_dirty()))
                .unwrap_or(false)
        };
        let tabs: Vec<EdTab> = g
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let title = title_of(t);
                EdTab {
                    width: tab_width(&title),
                    title: SharedString::from(title),
                    dirty: dirty(t),
                    active: g.active == Some(i),
                }
            })
            .collect();
        pane.tabs = ModelRc::new(VecModel::from(tabs));
        let r = ed.panes.get(index).map(|p| p.rect).unwrap_or(Rect {
            x: 0.0,
            y: 0.0,
            w: ed.area.2,
            h: ed.area.3,
        });
        pane.gx = r.x;
        pane.gy = r.y;
        pane.gw = r.w;
        pane.gh = r.h;
        pane.floating = ed.layout.is_floating(g.id);
        pane.focused = pane.floating || ed.layout.focused == g.id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_index_follows_tab_widths() {
        let titles = vec!["a.py".to_owned(), "longer_name.py".to_owned()];
        assert_eq!(insert_index(&titles, 0.0), 0);
        assert_eq!(insert_index(&titles, tab_width("a.py") - 1.0), 1);
        assert_eq!(insert_index(&titles, 10_000.0), 2);
        assert!(tab_width("x") >= 80.0 && tab_width(&"x".repeat(200)) <= 240.0);
    }
}
