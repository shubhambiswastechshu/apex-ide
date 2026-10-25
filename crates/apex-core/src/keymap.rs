//! The command registry and keymap: the single source of truth for every
//! IDE command, its menu category, and its keyboard shortcuts.
//!
//! - [`COMMANDS`] lists every built-in command with its default bindings.
//! - [`KeySeq`] is a normalized shortcut such as `Ctrl+Shift+P` or the
//!   two-chord `Ctrl+K Ctrl+S`.
//! - [`Keymap`] holds the effective bindings: defaults plus user overrides
//!   from `keymap.toml` (see [`Keymap::apply_overrides_toml`]), and reports
//!   [conflicts](Keymap::conflicts).
//!
//! The user file looks like this; a command may take one shortcut, a list,
//! or an empty list to unbind it:
//!
//! ```toml
//! [bindings]
//! "file.save" = "Ctrl+S"
//! "edit.duplicate_line" = ["Ctrl+D", "Ctrl+Shift+D"]
//! "view.zoom_in" = []
//! ```

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::Path;

use serde::Deserialize;

/// File name of the user keymap inside the config directory.
pub const KEYMAP_FILE_NAME: &str = "keymap.toml";

/// One built-in command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    /// Stable id, `<category>.<name>`, used in `keymap.toml`.
    pub id: &'static str,
    /// Title shown in menus and the command palette.
    pub title: &'static str,
    /// Menu the command belongs to (`File`, `Edit`, ...).
    pub category: &'static str,
    /// Default shortcuts, most prominent first.
    pub keys: &'static [&'static str],
}

const fn cmd(
    id: &'static str,
    title: &'static str,
    category: &'static str,
    keys: &'static [&'static str],
) -> Command {
    Command {
        id,
        title,
        category,
        keys,
    }
}

/// Every built-in command, in menu order within each category.
pub const COMMANDS: &[Command] = &[
    // File
    cmd(
        "file.new_project",
        "New Project...",
        "File",
        &["Ctrl+Shift+N"],
    ),
    cmd("file.open_folder", "Open Folder...", "File", &["Ctrl+O"]),
    cmd("file.new_file", "New File...", "File", &["Ctrl+N"]),
    cmd("file.new_folder", "New Folder...", "File", &[]),
    cmd("file.save", "Save", "File", &["Ctrl+S"]),
    cmd("file.save_all", "Save All", "File", &["Ctrl+Shift+S"]),
    cmd(
        "file.close_tab",
        "Close Tab",
        "File",
        &["Ctrl+F4", "Ctrl+W"],
    ),
    cmd(
        "file.close_all_tabs",
        "Close All Tabs",
        "File",
        &["Ctrl+Shift+F4"],
    ),
    cmd("file.close_project", "Close Project", "File", &[]),
    cmd("file.settings", "Settings...", "File", &["Ctrl+Alt+S"]),
    cmd("file.edit_keymap", "Edit Keymap (keymap.toml)", "File", &[]),
    cmd("file.reload_keymap", "Reload Keymap", "File", &[]),
    cmd("file.exit", "Exit", "File", &["Alt+F4"]),
    // Edit
    cmd("edit.undo", "Undo", "Edit", &["Ctrl+Z"]),
    cmd("edit.redo", "Redo", "Edit", &["Ctrl+Y", "Ctrl+Shift+Z"]),
    cmd("edit.cut", "Cut", "Edit", &["Ctrl+X"]),
    cmd("edit.copy", "Copy", "Edit", &["Ctrl+C"]),
    cmd("edit.paste", "Paste", "Edit", &["Ctrl+V"]),
    cmd("edit.select_all", "Select All", "Edit", &["Ctrl+A"]),
    cmd(
        "edit.toggle_comment",
        "Toggle Line Comment",
        "Edit",
        &["Ctrl+/"],
    ),
    cmd("edit.duplicate_line", "Duplicate Line", "Edit", &["Ctrl+D"]),
    cmd("edit.find", "Find...", "Edit", &["Ctrl+F"]),
    cmd("edit.find_next", "Find Next", "Edit", &["F3"]),
    cmd("edit.find_prev", "Find Previous", "Edit", &["Shift+F3"]),
    cmd(
        "edit.find_in_files",
        "Find in Files...",
        "Edit",
        &["Ctrl+Shift+F"],
    ),
    cmd("edit.fold", "Collapse Block", "Edit", &["Ctrl+Shift+["]),
    cmd("edit.unfold", "Expand Block", "Edit", &["Ctrl+Shift+]"]),
    // View
    cmd(
        "view.command_palette",
        "Command Palette...",
        "View",
        &["Ctrl+Shift+P"],
    ),
    cmd("view.project", "Project Explorer", "View", &["Alt+1"]),
    cmd("view.output", "Output", "View", &["Alt+4"]),
    cmd("view.problems", "Problems", "View", &["Alt+6"]),
    cmd("view.databases", "Databases", "View", &["Alt+7"]),
    cmd("view.plugins", "Plugins", "View", &["Alt+8"]),
    cmd(
        "view.hide_tool_windows",
        "Hide Bottom Panel",
        "View",
        &["Ctrl+Shift+F12"],
    ),
    cmd("view.dock_left", "Dock Explorer Left", "View", &[]),
    cmd("view.dock_right", "Dock Explorer Right", "View", &[]),
    // Ctrl+Shift+= is Ctrl++ on layouts where + is shifted =.
    cmd(
        "view.zoom_in",
        "Zoom In",
        "View",
        &["Ctrl+=", "Ctrl+Shift+="],
    ),
    cmd("view.zoom_out", "Zoom Out", "View", &["Ctrl+-"]),
    cmd("view.zoom_reset", "Reset Zoom", "View", &["Ctrl+0"]),
    cmd("view.toolbar", "Toolbar", "View", &[]),
    cmd("view.status_bar", "Status Bar", "View", &[]),
    cmd("view.compact", "Compact Density", "View", &[]),
    // Navigate
    // Search Everywhere is also opened by pressing Shift twice.
    cmd(
        "nav.search_everywhere",
        "Search Everywhere",
        "Navigate",
        &[],
    ),
    cmd(
        "nav.goto_file",
        "Go to File...",
        "Navigate",
        &["Ctrl+Shift+O"],
    ),
    cmd("nav.goto_line", "Go to Line...", "Navigate", &["Ctrl+G"]),
    cmd(
        "nav.search_explorer",
        "Search Project Explorer",
        "Navigate",
        &["Ctrl+;"],
    ),
    cmd("nav.next_problem", "Next Problem", "Navigate", &["F8"]),
    cmd(
        "nav.prev_problem",
        "Previous Problem",
        "Navigate",
        &["Shift+F8"],
    ),
    // Code
    cmd("code.complete", "Complete Code", "Code", &["Ctrl+Space"]),
    cmd(
        "code.install_packages",
        "Install Missing Packages",
        "Code",
        &[],
    ),
    // Run
    cmd("run.run", "Run", "Run", &["Shift+F10"]),
    cmd("run.stop", "Stop", "Run", &["Ctrl+F2"]),
    cmd("run.clear_output", "Clear Output", "Run", &[]),
    // Tools
    cmd("tools.databases", "Database Console", "Tools", &[]),
    cmd("tools.plugins", "Plugins", "Tools", &[]),
    cmd(
        "tools.refresh_project",
        "Refresh Project Files",
        "Tools",
        &["Ctrl+Alt+Y"],
    ),
    // Window
    // Ctrl+Tab / Ctrl+Shift+Tab open the open-files switcher (most
    // recently used first; release Ctrl to switch).
    cmd(
        "window.switcher",
        "Switch Between Open Files",
        "Window",
        &["Ctrl+Tab"],
    ),
    cmd("window.next_tab", "Next Tab", "Window", &["Ctrl+PageDown"]),
    cmd(
        "window.prev_tab",
        "Previous Tab",
        "Window",
        &["Ctrl+PageUp"],
    ),
    cmd("window.split_right", "Split Right", "Window", &["Ctrl+\\"]),
    cmd("window.split_down", "Split Down", "Window", &[]),
    cmd(
        "window.move_to_new_window",
        "Move Tab to New Window",
        "Window",
        &[],
    ),
    // Help
    cmd(
        "help.shortcuts",
        "Keyboard Shortcuts",
        "Help",
        &["Ctrl+K Ctrl+S"],
    ),
    cmd("help.about", "About Apex IDE", "Help", &[]),
];

/// The menu categories in menu-bar order.
pub const CATEGORIES: [&str; 9] = [
    "File", "Edit", "View", "Navigate", "Code", "Run", "Tools", "Window", "Help",
];

/// Looks up a built-in command by id.
pub fn command(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.id == id)
}

/// One key press with modifiers, e.g. `Ctrl+Shift+P`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Chord {
    /// Control held.
    pub ctrl: bool,
    /// Alt held.
    pub alt: bool,
    /// Shift held.
    pub shift: bool,
    /// Meta / Windows / Command held.
    pub meta: bool,
    /// Canonical key name: an upper-case letter, a digit or symbol, or a
    /// named key such as `F4`, `Tab`, `Space`, `PageDown`.
    pub key: String,
}

const NAMED_KEYS: [&str; 15] = [
    "Enter",
    "Escape",
    "Tab",
    "Space",
    "Backspace",
    "Delete",
    "Insert",
    "Home",
    "End",
    "PageUp",
    "PageDown",
    "Left",
    "Right",
    "Up",
    "Down",
];

/// Canonical spelling of a key name, or `None` if it is not a key.
fn canonical_key(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let alias = match lower.as_str() {
        "esc" => Some("Escape"),
        "return" => Some("Enter"),
        "del" => Some("Delete"),
        "ins" => Some("Insert"),
        "pgup" => Some("PageUp"),
        "pgdn" => Some("PageDown"),
        "left arrow" | "arrowleft" => Some("Left"),
        "right arrow" | "arrowright" => Some("Right"),
        "up arrow" | "arrowup" => Some("Up"),
        "down arrow" | "arrowdown" => Some("Down"),
        "plus" | "+" => Some("="),
        "minus" => Some("-"),
        _ => None,
    };
    if let Some(a) = alias {
        return Some(a.to_owned());
    }
    if let Some(named) = NAMED_KEYS.iter().find(|k| k.eq_ignore_ascii_case(name)) {
        return Some((*named).to_owned());
    }
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        if (1..=24).contains(&n) {
            return Some(format!("F{n}"));
        }
    }
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_whitespace() && !c.is_control() => {
            Some(c.to_uppercase().collect())
        }
        _ => None,
    }
}

impl Chord {
    /// Parses `Ctrl+Shift+P` (case-insensitive, modifiers in any order).
    pub fn parse(text: &str) -> Result<Self, KeyParseError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(KeyParseError(text.to_owned()));
        }
        // A trailing "+" is the plus key itself ("Ctrl++").
        let (mods, key) = match text.strip_suffix("++") {
            Some(rest) => (rest, "="),
            None => match text.rsplit_once('+') {
                Some((m, k)) if !k.is_empty() => (m, k),
                Some(_) => return Err(KeyParseError(text.to_owned())),
                None => ("", text),
            },
        };
        let mut chord = Chord {
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
            key: canonical_key(key.trim()).ok_or_else(|| KeyParseError(text.to_owned()))?,
        };
        if !mods.is_empty() {
            for m in mods.split('+') {
                match m.trim().to_ascii_lowercase().as_str() {
                    "ctrl" | "control" => chord.ctrl = true,
                    "alt" | "option" => chord.alt = true,
                    "shift" => chord.shift = true,
                    "meta" | "cmd" | "win" | "super" => chord.meta = true,
                    _ => return Err(KeyParseError(text.to_owned())),
                }
            }
        }
        Ok(chord)
    }

    /// Builds a chord from a key event: `key` is a key name or the typed
    /// character. Returns `None` for keys that cannot be bound (a lone
    /// modifier, or text longer than one character).
    pub fn from_event(key: &str, ctrl: bool, alt: bool, shift: bool, meta: bool) -> Option<Self> {
        Some(Chord {
            ctrl,
            alt,
            shift,
            meta,
            key: canonical_key(key)?,
        })
    }

    /// Whether this chord has no modifier other than Shift; such chords
    /// type text, so they are only bindable for function/named keys.
    pub fn is_plain(&self) -> bool {
        !self.ctrl && !self.alt && !self.meta
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("Ctrl+")?;
        }
        if self.alt {
            f.write_str("Alt+")?;
        }
        if self.shift {
            f.write_str("Shift+")?;
        }
        if self.meta {
            f.write_str("Meta+")?;
        }
        f.write_str(&self.key)
    }
}

/// A shortcut: one chord, or several pressed one after another.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeySeq(pub Vec<Chord>);

impl KeySeq {
    /// Parses `Ctrl+K Ctrl+S` (chords separated by spaces).
    pub fn parse(text: &str) -> Result<Self, KeyParseError> {
        let chords = text
            .split_whitespace()
            .map(Chord::parse)
            .collect::<Result<Vec<_>, _>>()?;
        if chords.is_empty() || chords.len() > 2 {
            return Err(KeyParseError(text.to_owned()));
        }
        Ok(Self(chords))
    }
}

impl fmt::Display for KeySeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, c) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

/// A shortcut string that could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a valid shortcut: {0:?}")]
pub struct KeyParseError(pub String);

/// Two or more commands bound to the same shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The shared shortcut.
    pub keys: KeySeq,
    /// The commands bound to it, sorted.
    pub commands: Vec<String>,
}

/// What a key press means given the chords pressed before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Run this command.
    Command(String),
    /// The chord starts a longer shortcut; wait for the next one.
    Prefix,
    /// Not bound.
    None,
}

/// Effective bindings: command id -> shortcuts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    bindings: BTreeMap<String, Vec<KeySeq>>,
}

impl Default for Keymap {
    /// The built-in bindings from [`COMMANDS`].
    fn default() -> Self {
        let bindings = COMMANDS
            .iter()
            .map(|c| {
                let keys = c
                    .keys
                    .iter()
                    .map(|k| KeySeq::parse(k).expect("built-in shortcut parses"))
                    .collect();
                (c.id.to_owned(), keys)
            })
            .collect();
        Self { bindings }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct KeymapFile {
    #[serde(default)]
    bindings: BTreeMap<String, OneOrMany>,
}

impl Keymap {
    /// The shortcuts bound to `id` (empty if none or unknown).
    pub fn keys(&self, id: &str) -> &[KeySeq] {
        self.bindings.get(id).map_or(&[], Vec::as_slice)
    }

    /// The first shortcut of `id` as display text, or `""`.
    pub fn label(&self, id: &str) -> String {
        self.keys(id)
            .first()
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    /// Replaces the shortcuts of `id`.
    pub fn bind(&mut self, id: &str, keys: Vec<KeySeq>) {
        self.bindings.insert(id.to_owned(), keys);
    }

    /// Applies user overrides in `keymap.toml` syntax. Returns one warning
    /// per problem (unknown command, bad shortcut); valid entries still
    /// apply. A file that does not parse at all applies nothing.
    pub fn apply_overrides_toml(&mut self, text: &str) -> Vec<String> {
        let file: KeymapFile = match toml::from_str(text) {
            Ok(f) => f,
            Err(e) => return vec![format!("{KEYMAP_FILE_NAME} could not be parsed: {e}")],
        };
        let mut warnings = Vec::new();
        for (id, value) in file.bindings {
            if command(&id).is_none() {
                warnings.push(format!("{KEYMAP_FILE_NAME}: unknown command {id:?}"));
                continue;
            }
            let texts = match value {
                OneOrMany::One(s) => vec![s],
                OneOrMany::Many(v) => v,
            };
            let mut keys = Vec::new();
            for t in texts {
                match KeySeq::parse(&t) {
                    Ok(k) => keys.push(k),
                    Err(e) => warnings.push(format!("{KEYMAP_FILE_NAME}: {id}: {e}")),
                }
            }
            self.bind(&id, keys);
        }
        warnings
    }

    /// Loads defaults plus the overrides in `path` (a missing file is
    /// fine). Returns the keymap and any warnings.
    pub fn load(path: Option<&Path>) -> (Self, Vec<String>) {
        let mut map = Self::default();
        let Some(path) = path else {
            return (map, Vec::new());
        };
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let warnings = map.apply_overrides_toml(&text);
                (map, warnings)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (map, Vec::new()),
            Err(e) => (map, vec![format!("cannot read {}: {e}", path.display())]),
        }
    }

    /// Shortcuts bound to more than one command.
    pub fn conflicts(&self) -> Vec<Conflict> {
        let mut by_keys: BTreeMap<&KeySeq, Vec<String>> = BTreeMap::new();
        for (id, keys) in &self.bindings {
            for k in keys {
                by_keys.entry(k).or_default().push(id.clone());
            }
        }
        // A sequence whose first chord is also a full shortcut can never
        // be reached either.
        let singles: HashMap<&Chord, &String> = self
            .bindings
            .iter()
            .flat_map(|(id, keys)| {
                keys.iter()
                    .filter(|k| k.0.len() == 1)
                    .map(move |k| (&k.0[0], id))
            })
            .collect();
        let mut out: Vec<Conflict> = by_keys
            .into_iter()
            .filter(|(_, ids)| ids.len() > 1)
            .map(|(k, mut ids)| {
                ids.sort();
                Conflict {
                    keys: k.clone(),
                    commands: ids,
                }
            })
            .collect();
        for (id, keys) in &self.bindings {
            for k in keys.iter().filter(|k| k.0.len() > 1) {
                if let Some(other) = singles.get(&k.0[0]) {
                    let mut commands = vec![id.clone(), (*other).clone()];
                    commands.sort();
                    out.push(Conflict {
                        keys: k.clone(),
                        commands,
                    });
                }
            }
        }
        out
    }

    /// Resolves a key press. `pending` is the chord pressed just before
    /// (the first half of a two-chord shortcut), if any.
    pub fn lookup(&self, pending: Option<&Chord>, chord: &Chord) -> Lookup {
        let mut prefix = false;
        for (id, keys) in &self.bindings {
            for k in keys {
                match (pending, k.0.as_slice()) {
                    (None, [only]) if only == chord => return Lookup::Command(id.clone()),
                    (None, [first, _]) if first == chord => prefix = true,
                    (Some(p), [first, second]) if first == p && second == chord => {
                        return Lookup::Command(id.clone())
                    }
                    _ => {}
                }
            }
        }
        if prefix {
            Lookup::Prefix
        } else {
            Lookup::None
        }
    }

    /// The commented template written when the user first edits the keymap.
    pub fn template() -> String {
        let mut s = String::from(
            "# Apex IDE keymap overrides.\n\
             # Each entry replaces the default shortcuts of one command.\n\
             # Use a string, a list, or [] to unbind. Two-chord shortcuts are\n\
             # written with a space: \"Ctrl+K Ctrl+S\".\n\
             # Saved changes apply after File > Reload Keymap.\n\n[bindings]\n",
        );
        for c in COMMANDS {
            let keys: Vec<String> = c.keys.iter().map(|k| format!("{k:?}")).collect();
            s.push_str(&format!(
                "# {:?} = [{}]  # {}\n",
                c.id,
                keys.join(", "),
                c.title
            ));
        }
        s
    }
}

/// Fuzzy score of `query` against `text` (see [`crate::search::fuzzy_match`]).
pub fn fuzzy_score(query: &str, text: &str) -> Option<i64> {
    crate::search::fuzzy_match(query, text).map(|m| m.score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_command_is_valid_and_unique() {
        let mut ids = std::collections::HashSet::new();
        for c in COMMANDS {
            assert!(ids.insert(c.id), "duplicate command {}", c.id);
            assert!(CATEGORIES.contains(&c.category), "{} has no menu", c.id);
            assert!(c.id.contains('.'));
            for k in c.keys {
                KeySeq::parse(k).unwrap_or_else(|e| panic!("{}: {e}", c.id));
            }
        }
    }

    #[test]
    fn default_keymap_has_no_conflicts() {
        assert_eq!(Keymap::default().conflicts(), Vec::new());
    }

    #[test]
    fn chords_parse_and_normalize() {
        let c = Chord::parse("shift+ctrl+p").unwrap();
        assert_eq!(c.to_string(), "Ctrl+Shift+P");
        assert_eq!(Chord::parse("Alt+f4").unwrap().to_string(), "Alt+F4");
        assert_eq!(Chord::parse("Ctrl++").unwrap().to_string(), "Ctrl+=");
        assert_eq!(
            Chord::parse("ctrl+pgdn").unwrap().to_string(),
            "Ctrl+PageDown"
        );
        assert_eq!(Chord::parse("Ctrl+space").unwrap().key, "Space");
        assert_eq!(Chord::parse("Ctrl+/").unwrap().key, "/");
        assert!(Chord::parse("Hyper+X").is_err());
        assert!(Chord::parse("Ctrl+").is_err());
        assert!(Chord::parse("").is_err());
        assert!(Chord::parse("F25").is_err());
        let seq = KeySeq::parse("ctrl+k  ctrl+s").unwrap();
        assert_eq!(seq.to_string(), "Ctrl+K Ctrl+S");
        assert!(KeySeq::parse("A B C").is_err());
    }

    #[test]
    fn events_map_to_chords() {
        let c = Chord::from_event("p", true, false, true, false).unwrap();
        assert_eq!(c, Chord::parse("Ctrl+Shift+P").unwrap());
        assert!(Chord::from_event("", true, false, false, false).is_none());
        assert!(Chord::from_event("F4", false, true, false, false).is_some());
        assert!(Chord::parse("F8").unwrap().is_plain());
        assert!(!Chord::parse("Ctrl+F8").unwrap().is_plain());
    }

    #[test]
    fn ctrl_plus_and_numpad_plus_zoom_in() {
        let map = Keymap::default();
        let zoom_in = Lookup::Command("view.zoom_in".into());
        // Ctrl and the shifted "+" key, Ctrl and numpad "+", Ctrl and "=".
        for (text, shift) in [("+", true), ("+", false), ("=", false)] {
            let c = Chord::from_event(text, true, false, shift, false).unwrap();
            assert_eq!(map.lookup(None, &c), zoom_in, "{text} shift={shift}");
        }
        let minus = Chord::from_event("-", true, false, false, false).unwrap();
        assert_eq!(
            map.lookup(None, &minus),
            Lookup::Command("view.zoom_out".into())
        );
        let zero = Chord::from_event("0", true, false, false, false).unwrap();
        assert_eq!(
            map.lookup(None, &zero),
            Lookup::Command("view.zoom_reset".into())
        );
    }

    #[test]
    fn lookup_handles_single_and_two_chord_shortcuts() {
        let map = Keymap::default();
        let ctrl_s = Chord::parse("Ctrl+S").unwrap();
        assert_eq!(
            map.lookup(None, &ctrl_s),
            Lookup::Command("file.save".into())
        );
        let ctrl_k = Chord::parse("Ctrl+K").unwrap();
        assert_eq!(map.lookup(None, &ctrl_k), Lookup::Prefix);
        assert_eq!(
            map.lookup(Some(&ctrl_k), &ctrl_s),
            Lookup::Command("help.shortcuts".into())
        );
        assert_eq!(
            map.lookup(None, &Chord::parse("Ctrl+F12").unwrap()),
            Lookup::None
        );
        assert_eq!(map.label("file.save"), "Ctrl+S");
        assert_eq!(map.label("file.close_project"), "");
    }

    #[test]
    fn overrides_replace_unbind_and_warn() {
        let mut map = Keymap::default();
        let warnings = map.apply_overrides_toml(
            r#"
            [bindings]
            "file.save" = "Ctrl+Alt+S"
            "edit.duplicate_line" = ["Ctrl+D", "Ctrl+Shift+D"]
            "view.zoom_in" = []
            "no.such" = "Ctrl+Q"
            "edit.undo" = ["Ctrl+Bogus+Z"]
            "#,
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert_eq!(map.label("file.save"), "Ctrl+Alt+S");
        assert_eq!(map.keys("edit.duplicate_line").len(), 2);
        assert!(map.keys("view.zoom_in").is_empty());
        // Ctrl+Alt+S now belongs to both file.save and file.settings.
        let conflicts = map.conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].commands, ["file.save", "file.settings"]);
        assert_eq!(conflicts[0].keys.to_string(), "Ctrl+Alt+S");

        let bad = map.apply_overrides_toml("[bindings\n");
        assert_eq!(bad.len(), 1);
    }

    #[test]
    fn prefix_shadowing_is_a_conflict() {
        let mut map = Keymap::default();
        map.bind("file.close_project", vec![KeySeq::parse("Ctrl+K").unwrap()]);
        let c = map.conflicts();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].commands, ["file.close_project", "help.shortcuts"]);
    }

    #[test]
    fn load_reads_files_and_tolerates_missing_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEYMAP_FILE_NAME);
        let (map, w) = Keymap::load(Some(&path));
        assert!(w.is_empty());
        assert_eq!(map, Keymap::default());
        std::fs::write(&path, "[bindings]\n\"run.run\" = \"F5\"\n").unwrap();
        let (map, w) = Keymap::load(Some(&path));
        assert!(w.is_empty());
        assert_eq!(map.label("run.run"), "F5");
        // The template is valid TOML that changes nothing.
        let mut t = Keymap::default();
        assert!(t.apply_overrides_toml(&Keymap::template()).is_empty());
        assert_eq!(t, Keymap::default());
    }

    #[test]
    fn fuzzy_matching_ranks_sensible_results() {
        assert!(fuzzy_score("svall", "Save All").is_some());
        assert!(fuzzy_score("xyz", "Save All").is_none());
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        let a = fuzzy_score("save", "Save").unwrap();
        let b = fuzzy_score("save", "Show Available Versions").unwrap();
        assert!(a > b);
        let c = fuzzy_score("mp", "main.py").unwrap();
        let d = fuzzy_score("mp", "templates").unwrap();
        assert!(c > d);
    }
}
