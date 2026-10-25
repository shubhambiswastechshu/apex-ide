//! Key bindings of the code editor: maps a Slint key event to an
//! [`Action`]. Pure, so the bindings are unit tested.

use apex_editor::Motion;
use slint::platform::Key;

/// What a key press asks the editor to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Move the caret (`true` extends the selection).
    Move(Motion, bool),
    /// Insert typed text.
    Type(String),
    /// Enter.
    Newline,
    /// Backspace.
    Backspace,
    /// Delete.
    Delete,
    /// Ctrl+Backspace (`false`) / Ctrl+Delete (`true`).
    DeleteWord(bool),
    /// Tab.
    Tab,
    /// Shift+Tab.
    BackTab,
    /// Ctrl+/.
    ToggleComment,
    /// Ctrl+Z.
    Undo,
    /// Ctrl+Y / Ctrl+Shift+Z.
    Redo,
    /// Ctrl+A.
    SelectAll,
    /// Ctrl+C / Ctrl+Insert.
    Copy,
    /// Ctrl+X / Shift+Delete.
    Cut,
    /// Ctrl+V / Shift+Insert.
    Paste,
    /// Ctrl+D.
    Duplicate,
    /// Ctrl+S.
    Save,
    /// Shift+F10.
    Run,
    /// Ctrl+Space.
    Complete,
    /// Escape.
    Escape,
    /// Ctrl+Shift+[ / Ctrl+- : fold at caret; Ctrl+Shift+] / Ctrl+= unfold.
    Fold(bool),
    /// Ctrl+F.
    Find,
    /// F3 (`true`) / Shift+F3 (`false`).
    FindNext(bool),
    /// Ctrl+G.
    GoToLine,
}

fn key(k: Key) -> String {
    String::from(char::from(k))
}

fn is_printable(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(&c) && c != '\u{7f}')
}

/// Maps a key event. `None` means the editor does not handle it (it then
/// bubbles to the window, e.g. for global shortcuts).
pub fn map_key(text: &str, ctrl: bool, shift: bool, alt: bool) -> Option<Action> {
    use Action as A;
    use Motion as M;
    let lower = text.to_lowercase();
    let is = |k: Key| text == key(k);

    // Movement.
    let motion = if is(Key::LeftArrow) {
        Some(if ctrl { M::WordLeft } else { M::Left })
    } else if is(Key::RightArrow) {
        Some(if ctrl { M::WordRight } else { M::Right })
    } else if is(Key::UpArrow) && !ctrl {
        Some(M::Up)
    } else if is(Key::DownArrow) && !ctrl {
        Some(M::Down)
    } else if is(Key::Home) {
        Some(if ctrl { M::DocStart } else { M::LineStart })
    } else if is(Key::End) {
        Some(if ctrl { M::DocEnd } else { M::LineEnd })
    } else if is(Key::PageUp) {
        Some(M::PageUp(0))
    } else if is(Key::PageDown) {
        Some(M::PageDown(0))
    } else {
        None
    };
    if let Some(m) = motion {
        // Ctrl+PageUp/PageDown switch tabs (window commands).
        if alt || (ctrl && matches!(m, M::PageUp(_) | M::PageDown(_))) {
            return None;
        }
        return Some(A::Move(m, shift));
    }

    if is(Key::Return) || text == "\n" || text == "\r" {
        return (!ctrl && !alt).then_some(A::Newline);
    }
    if is(Key::Backspace) {
        return Some(if ctrl {
            A::DeleteWord(false)
        } else {
            A::Backspace
        });
    }
    if is(Key::Delete) {
        return Some(if shift && !ctrl {
            A::Cut
        } else if ctrl {
            A::DeleteWord(true)
        } else {
            A::Delete
        });
    }
    if is(Key::Insert) {
        return match (ctrl, shift) {
            (true, false) => Some(A::Copy),
            (false, true) => Some(A::Paste),
            _ => None,
        };
    }
    if is(Key::Backtab) || (text == "\t" && shift) {
        return (!ctrl).then_some(A::BackTab);
    }
    if text == "\t" {
        return (!ctrl && !alt).then_some(A::Tab);
    }
    if is(Key::Escape) {
        return Some(A::Escape);
    }
    if is(Key::F10) && shift && !ctrl {
        return Some(A::Run);
    }
    if is(Key::F3) && !ctrl {
        return Some(A::FindNext(!shift));
    }

    if ctrl && !alt {
        return match lower.as_str() {
            "z" if shift => Some(A::Redo),
            "z" => Some(A::Undo),
            "y" => Some(A::Redo),
            "a" => Some(A::SelectAll),
            "c" => Some(A::Copy),
            "x" => Some(A::Cut),
            "v" => Some(A::Paste),
            "d" => Some(A::Duplicate),
            "f" if !shift => Some(A::Find),
            "g" if !shift => Some(A::GoToLine),
            "s" if !shift => Some(A::Save),
            "/" => Some(A::ToggleComment),
            " " => Some(A::Complete),
            "[" | "{" if shift => Some(A::Fold(true)),
            "]" | "}" if shift => Some(A::Fold(false)),
            // Ctrl+- / Ctrl+= / Ctrl+0 are View > Zoom (command registry).
            _ => None,
        };
    }
    // Alt+key (Alt+1, Alt+4, ...) are tool window shortcuts, not typing
    // (on macOS Option composes characters, so it types there).
    if alt && !ctrl && !cfg!(target_os = "macos") {
        return None;
    }
    // Ctrl+Alt is AltGr on many layouts: treat it as typing, except plain
    // ASCII letters and digits, which AltGr never produces (Ctrl+Alt+S is
    // Settings).
    if ctrl && alt && text.len() == 1 && text.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    if is_printable(text) {
        return Some(A::Type(text.to_owned()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_keys() {
        assert_eq!(
            map_key(&key(Key::LeftArrow), true, true, false),
            Some(Action::Move(Motion::WordLeft, true))
        );
        assert_eq!(
            map_key(&key(Key::Home), true, false, false),
            Some(Action::Move(Motion::DocStart, false))
        );
        assert_eq!(
            map_key(&key(Key::End), false, false, false),
            Some(Action::Move(Motion::LineEnd, false))
        );
        assert_eq!(
            map_key(&key(Key::PageDown), false, true, false),
            Some(Action::Move(Motion::PageDown(0), true))
        );
    }

    #[test]
    fn editing_keys() {
        assert_eq!(map_key("\n", false, false, false), Some(Action::Newline));
        assert_eq!(map_key("\t", false, false, false), Some(Action::Tab));
        assert_eq!(map_key("\t", false, true, false), Some(Action::BackTab));
        assert_eq!(
            map_key(&key(Key::Backspace), true, false, false),
            Some(Action::DeleteWord(false))
        );
        assert_eq!(
            map_key(&key(Key::Delete), false, false, false),
            Some(Action::Delete)
        );
        assert_eq!(
            map_key(&key(Key::Escape), false, false, false),
            Some(Action::Escape)
        );
    }

    #[test]
    fn shortcuts() {
        assert_eq!(map_key("z", true, false, false), Some(Action::Undo));
        assert_eq!(map_key("Z", true, true, false), Some(Action::Redo));
        assert_eq!(map_key("y", true, false, false), Some(Action::Redo));
        assert_eq!(
            map_key("/", true, false, false),
            Some(Action::ToggleComment)
        );
        assert_eq!(map_key(" ", true, false, false), Some(Action::Complete));
        assert_eq!(map_key("s", true, false, false), Some(Action::Save));
        assert_eq!(
            map_key(&key(Key::F10), false, true, false),
            Some(Action::Run)
        );
        // Unknown Ctrl shortcuts bubble up to the window.
        assert_eq!(map_key("f", true, false, false), Some(Action::Find));
        assert_eq!(map_key("g", true, false, false), Some(Action::GoToLine));
        assert_eq!(
            map_key(&key(Key::F3), false, true, false),
            Some(Action::FindNext(false))
        );
        // Unknown Ctrl shortcuts bubble up to the window.
        assert_eq!(map_key("q", true, false, false), None);
        assert_eq!(map_key("s", true, true, false), None);
    }

    #[test]
    fn typing_including_altgr() {
        assert_eq!(
            map_key("a", false, false, false),
            Some(Action::Type("a".into()))
        );
        assert_eq!(
            map_key("é", false, false, false),
            Some(Action::Type("é".into()))
        );
        assert_eq!(
            map_key("@", true, false, true),
            Some(Action::Type("@".into()))
        );
        assert_eq!(map_key(&key(Key::F5), false, false, false), None);
        assert_eq!(map_key("", false, false, false), None);
    }

    #[test]
    fn global_shortcuts_are_not_swallowed() {
        // Alt+1 / Alt+4 open tool windows; they must not type a digit.
        if !cfg!(target_os = "macos") {
            assert_eq!(map_key("1", false, false, true), None);
            assert_eq!(map_key("4", false, false, true), None);
        }
        // Ctrl+Alt+S (Settings) and Ctrl+Alt+Y must reach the window.
        assert_eq!(map_key("s", true, false, true), None);
        assert_eq!(map_key("y", true, false, true), None);
        // Ctrl+PageDown / Ctrl+PageUp switch tabs.
        assert_eq!(map_key(&key(Key::PageDown), true, false, false), None);
        assert_eq!(map_key(&key(Key::PageUp), true, false, false), None);
        // Ctrl+Shift+P / Ctrl+Shift+F bubble too.
        assert_eq!(map_key("P", true, true, false), None);
        assert_eq!(map_key("F", true, true, false), None);
    }
}
