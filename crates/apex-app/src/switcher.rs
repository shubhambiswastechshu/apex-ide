//! The Ctrl+Tab switcher: a popup listing the open files, most recently
//! used first, as in Visual Studio and PyCharm. Ctrl+Tab opens it on the
//! previous file; Tab / Shift+Tab (Ctrl held) or the arrows move; releasing
//! Ctrl (or Enter, or a click) switches; Escape cancels.

use std::path::{Path, PathBuf};

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::app::{to_i32, App};
use crate::{ShellApi, SwitcherItem};

/// Moves `path` to the front of the most-recently-used list.
pub fn touch(mru: &mut Vec<PathBuf>, path: &Path) {
    mru.retain(|p| p != path);
    mru.insert(0, path.to_path_buf());
}

/// The open files in switcher order: most recently used first, then any
/// never-activated ones in tab order. Returns indices into `open`.
pub fn order(mru: &[PathBuf], open: &[PathBuf]) -> Vec<usize> {
    let mut out: Vec<usize> = mru
        .iter()
        .filter_map(|p| open.iter().position(|o| o == p))
        .collect();
    out.dedup();
    for i in 0..open.len() {
        if !out.contains(&i) {
            out.push(i);
        }
    }
    out
}

/// The row selected when the switcher opens: the previous file for
/// Ctrl+Tab, the least recent one for Ctrl+Shift+Tab.
pub fn initial(len: usize, forward: bool) -> usize {
    match (len, forward) {
        (0 | 1, _) => 0,
        (_, true) => 1,
        (n, false) => n - 1,
    }
}

/// `index` moved by `delta`, wrapping around `len` rows.
pub fn step(index: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (index as isize + delta).rem_euclid(len as isize) as usize
}

/// Whether `text` is the Tab key (Shift+Tab may arrive as Backtab).
pub fn is_tab(text: &str) -> bool {
    text == "\t" || text == String::from(char::from(slint::platform::Key::Backtab))
}

impl App {
    /// Ctrl+Tab / Ctrl+Shift+Tab: opens the switcher, or moves through it
    /// while it is open. Returns whether the key was used.
    pub(crate) fn switcher_step(&self, forward: bool) -> bool {
        let Some(ui) = self.ui.upgrade() else {
            return false;
        };
        let api = ui.global::<ShellApi>();
        let delta = if forward { 1 } else { -1 };
        if api.get_switcher_open() {
            let n = api.get_switcher_items().iter().count();
            let at = usize::try_from(api.get_switcher_index()).unwrap_or(0);
            api.set_switcher_index(to_i32(step(at, n, delta)));
            return true;
        }
        let (items, paths) = {
            let st = self.state.borrow();
            let Some(ws) = st.workspace.as_ref() else {
                return false;
            };
            let open: Vec<PathBuf> = ws.tabs.iter().map(|t| t.path.clone()).collect();
            let rows = order(&ws.mru, &open);
            let root = &ws.project.root;
            let items: Vec<SwitcherItem> = rows
                .iter()
                .map(|&i| {
                    let t = &ws.tabs[i];
                    let rel = t.path.strip_prefix(root).unwrap_or(&t.path);
                    SwitcherItem {
                        title: t.title().into(),
                        detail: rel
                            .parent()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                            .into(),
                        icon: crate::tree_view::file_icon_kind(&t.title(), false).into(),
                        dirty: t.buffer.is_dirty(),
                    }
                })
                .collect();
            let paths: Vec<PathBuf> = rows.iter().map(|&i| open[i].clone()).collect();
            (items, paths)
        };
        if items.is_empty() {
            return false;
        }
        let first = initial(items.len(), forward);
        self.state.borrow_mut().switcher = paths;
        api.set_switcher_items(ModelRc::new(VecModel::from(items)));
        api.set_switcher_index(to_i32(first));
        api.set_switcher_open(true);
        true
    }

    /// Moves the switcher selection by `delta` rows.
    pub(crate) fn switcher_move(&self, delta: isize) {
        let Some(ui) = self.ui.upgrade() else { return };
        let api = ui.global::<ShellApi>();
        let n = self.state.borrow().switcher.len();
        let at = usize::try_from(api.get_switcher_index()).unwrap_or(0);
        api.set_switcher_index(to_i32(step(at, n, delta)));
    }

    /// Switches to the selected row (`index`, or the current one when
    /// negative) and closes the switcher.
    pub(crate) fn switcher_commit(&self, index: i32) {
        let Some(ui) = self.ui.upgrade() else { return };
        let api = ui.global::<ShellApi>();
        if !api.get_switcher_open() {
            return;
        }
        let at = if index >= 0 {
            index
        } else {
            api.get_switcher_index()
        };
        let path = usize::try_from(at)
            .ok()
            .and_then(|i| self.state.borrow().switcher.get(i).cloned());
        self.switcher_cancel();
        if let Some(path) = path {
            self.open_file(&path);
        }
        self.editor_show_active();
    }

    /// Closes the switcher without switching.
    pub(crate) fn switcher_cancel(&self) {
        self.state.borrow_mut().switcher.clear();
        if let Some(ui) = self.ui.upgrade() {
            let api = ui.global::<ShellApi>();
            api.set_switcher_open(false);
            api.set_switcher_items(ModelRc::default());
        }
    }

    /// Keys while the switcher is open (seen before the focused element).
    /// Returns whether the key was used.
    pub(crate) fn switcher_key(&self, text: &str, ctrl: bool, shift: bool) -> bool {
        use slint::platform::Key;
        let is = |k: Key| text == String::from(char::from(k));
        if is_tab(text) && ctrl {
            return self.switcher_step(!(shift || text != "\t"));
        }
        if is(Key::Escape) {
            self.switcher_cancel();
            self.editor_show_active();
        } else if is(Key::UpArrow) {
            self.switcher_move(-1);
        } else if is(Key::DownArrow) {
            self.switcher_move(1);
        } else if is(Key::Return) || text == "\n" || text == "\r" {
            self.switcher_commit(-1);
        } else if is(Key::Control) || is(Key::Shift) {
            return false;
        }
        // Everything else is swallowed while the list is up.
        true
    }

    /// A key was released: letting go of Ctrl switches to the selection.
    pub(crate) fn switcher_key_released(&self, text: &str) -> bool {
        use slint::platform::Key;
        let open = self
            .ui
            .upgrade()
            .is_some_and(|ui| ui.global::<ShellApi>().get_switcher_open());
        if open && text == String::from(char::from(Key::Control)) {
            self.switcher_commit(-1);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn most_recent_first_then_tab_order() {
        let mut mru = Vec::new();
        touch(&mut mru, &p("a.py"));
        touch(&mut mru, &p("b.py"));
        touch(&mut mru, &p("a.py"));
        assert_eq!(mru, [p("a.py"), p("b.py")]);
        // c.py was never active; d.py is closed.
        touch(&mut mru, &p("d.py"));
        let open = [p("b.py"), p("c.py"), p("a.py")];
        assert_eq!(order(&mru, &open), [2, 0, 1]);
        assert_eq!(order(&[], &open), [0, 1, 2]);
    }

    #[test]
    fn opens_on_the_previous_file_and_wraps() {
        assert_eq!(initial(3, true), 1);
        assert_eq!(initial(3, false), 2);
        assert_eq!(initial(1, true), 0);
        assert_eq!(initial(0, false), 0);
        assert_eq!(step(2, 3, 1), 0);
        assert_eq!(step(0, 3, -1), 2);
        assert_eq!(step(0, 0, 1), 0);
    }

    #[test]
    fn tab_and_backtab_are_tab() {
        assert!(is_tab("\t"));
        assert!(is_tab(&String::from(char::from(
            slint::platform::Key::Backtab
        ))));
        assert!(!is_tab("a"));
    }
}
