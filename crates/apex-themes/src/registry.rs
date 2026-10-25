//! [`ThemeRegistry`]: built-in, user and plugin-contributed themes in one place.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::builtin::{builtin_themes, default_theme, is_builtin};
use crate::error::{ThemeError, ThemeLoadError};
use crate::theme::{slugify_theme_id, Theme};
use crate::vscode::{import_vscode_theme, looks_like_vscode_theme, strip_jsonc};

/// Where a registered theme came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeSource {
    /// Embedded in the binary.
    BuiltIn,
    /// Loaded from a file in a user theme directory.
    User(PathBuf),
    /// Registered at runtime, typically by a plugin (the string is the contributor's id).
    Plugin(String),
}

/// Result of [`ThemeRegistry::load_dir`]: what loaded and what was skipped.
#[derive(Debug, Default)]
pub struct LoadReport {
    /// Ids of the themes that were registered, in load order.
    pub loaded: Vec<String>,
    /// Files that were skipped, with the reason.
    pub errors: Vec<ThemeLoadError>,
}

impl LoadReport {
    /// `true` when no file failed.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

#[derive(Debug, Clone)]
struct Entry {
    theme: Theme,
    source: ThemeSource,
}

/// All themes available to the application.
///
/// The four built-ins are always present, always listed first and cannot be
/// replaced or removed; `apex-vs-blue` is the fallback for unknown ids. Other
/// themes are listed after them in registration order.
#[derive(Debug, Clone, Default)]
pub struct ThemeRegistry {
    extra: Vec<Entry>,
}

impl ThemeRegistry {
    /// A registry holding only the built-in themes.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry with the built-ins plus every theme in `dir` (see [`Self::load_dir`]).
    pub fn with_user_dir(dir: &Path) -> (Self, LoadReport) {
        let mut registry = Self::new();
        let report = registry.load_dir(dir);
        (registry, report)
    }

    /// Loads every `*.json` file directly inside `dir` (not recursive), in file
    /// name order.
    ///
    /// Each file may be an Apex theme or a VS Code color theme (JSON with
    /// comments and trailing commas is accepted). Invalid files are skipped and
    /// reported; they never abort the rest of the load. A missing directory is
    /// not an error and yields an empty report. A theme whose id is already
    /// registered from a non-built-in source replaces it.
    pub fn load_dir(&mut self, dir: &Path) -> LoadReport {
        let mut report = LoadReport::default();
        let read = match std::fs::read_dir(dir) {
            Ok(read) => read,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return report,
            Err(e) => {
                report.errors.push(ThemeLoadError {
                    path: dir.to_path_buf(),
                    error: ThemeError::Io {
                        path: dir.to_path_buf(),
                        source: e,
                    },
                });
                return report;
            }
        };

        let mut files: Vec<PathBuf> = Vec::new();
        for entry in read {
            match entry {
                Ok(entry) => {
                    let path = entry.path();
                    if path.is_file() && has_json_extension(&path) {
                        files.push(path);
                    }
                }
                Err(e) => report.errors.push(ThemeLoadError {
                    path: dir.to_path_buf(),
                    error: ThemeError::Io {
                        path: dir.to_path_buf(),
                        source: e,
                    },
                }),
            }
        }
        // Case-insensitive order so the result is identical on every platform.
        files.sort_by_cached_key(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        });

        let mut seen: HashSet<String> = HashSet::new();
        for path in files {
            let result = read_theme_file(&path).and_then(|theme| {
                if !seen.insert(theme.id.clone()) {
                    return Err(ThemeError::DuplicateId { id: theme.id });
                }
                let id = theme.id.clone();
                self.add_from(theme, ThemeSource::User(path.clone()))?;
                Ok(id)
            });
            match result {
                Ok(id) => report.loaded.push(id),
                Err(error) => report.errors.push(ThemeLoadError { path, error }),
            }
        }
        report
    }

    /// Loads a single theme file and registers it as a user theme; returns
    /// its id.
    pub fn load_file(&mut self, path: &Path) -> Result<String, ThemeError> {
        self.load_file_as(path, ThemeSource::User(path.to_path_buf()))
    }

    /// Loads a single theme file and registers it with `source` (for example
    /// [`ThemeSource::Plugin`] for a plugin-contributed theme, so it can be
    /// dropped with [`remove_by_source`](Self::remove_by_source)); returns its id.
    pub fn load_file_as(&mut self, path: &Path, source: ThemeSource) -> Result<String, ThemeError> {
        let theme = read_theme_file(path)?;
        let id = theme.id.clone();
        self.add_from(theme, source)?;
        Ok(id)
    }

    /// Registers a plugin-contributed theme. Equivalent to
    /// `add_from(theme, ThemeSource::Plugin(String::new()))`.
    pub fn add(&mut self, theme: Theme) -> Result<Option<Theme>, ThemeError> {
        self.add_from(theme, ThemeSource::Plugin(String::new()))
    }

    /// Registers a theme with an explicit source.
    ///
    /// The theme is validated first. Returns the theme it replaced, if any.
    /// Fails with [`ThemeError::BuiltinConflict`] if the id belongs to a built-in.
    pub fn add_from(
        &mut self,
        theme: Theme,
        source: ThemeSource,
    ) -> Result<Option<Theme>, ThemeError> {
        theme.validate()?;
        if is_builtin(&theme.id) {
            return Err(ThemeError::BuiltinConflict { id: theme.id });
        }
        let entry = Entry { theme, source };
        match self.extra.iter_mut().find(|e| e.theme.id == entry.theme.id) {
            Some(existing) => Ok(Some(std::mem::replace(existing, entry).theme)),
            None => {
                self.extra.push(entry);
                Ok(None)
            }
        }
    }

    /// Removes a non-built-in theme. Built-ins cannot be removed and yield `None`.
    pub fn remove(&mut self, id: &str) -> Option<Theme> {
        let index = self.extra.iter().position(|e| e.theme.id == id)?;
        Some(self.extra.remove(index).theme)
    }

    /// Removes every theme registered with the given source; returns how many.
    ///
    /// Useful when a plugin unloads (`ThemeSource::Plugin(id)`).
    pub fn remove_by_source(&mut self, source: &ThemeSource) -> usize {
        let before = self.extra.len();
        self.extra.retain(|e| &e.source != source);
        before - self.extra.len()
    }

    /// Looks a theme up by id.
    pub fn get(&self, id: &str) -> Option<&Theme> {
        builtin_themes()
            .iter()
            .find(|t| t.id == id)
            .or_else(|| self.extra.iter().map(|e| &e.theme).find(|t| t.id == id))
    }

    /// Looks a theme up by id, falling back to [`Self::default_theme`].
    pub fn get_or_default(&self, id: &str) -> &Theme {
        self.get(id).unwrap_or_else(|| self.default_theme())
    }

    /// The fallback theme, `apex-vs-blue`.
    pub fn default_theme(&self) -> &Theme {
        default_theme()
    }

    /// Where the theme with `id` came from.
    pub fn source(&self, id: &str) -> Option<&ThemeSource> {
        if is_builtin(id) {
            return Some(&ThemeSource::BuiltIn);
        }
        self.extra
            .iter()
            .find(|e| e.theme.id == id)
            .map(|e| &e.source)
    }

    /// Whether a theme with `id` is registered.
    pub fn contains(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    /// All themes: built-ins first, then others in registration order.
    pub fn list(&self) -> impl Iterator<Item = &Theme> + '_ {
        builtin_themes()
            .iter()
            .chain(self.extra.iter().map(|e| &e.theme))
    }

    /// Number of registered themes, built-ins included.
    pub fn len(&self) -> usize {
        builtin_themes().len() + self.extra.len()
    }

    /// Always `false`: the built-ins are always registered.
    pub fn is_empty(&self) -> bool {
        false
    }
}

fn has_json_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
}

/// Reads an Apex or VS Code theme file. VS Code themes take their id from the file stem.
fn read_theme_file(path: &Path) -> Result<Theme, ThemeError> {
    let text = std::fs::read_to_string(path).map_err(|source| ThemeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // Comments are blanked out position-preservingly, so serde's line/column
    // numbers still point into the original file.
    let clean = strip_jsonc(text.trim_start_matches('\u{feff}'));
    let value: serde_json::Value = serde_json::from_str(&clean)?;
    if looks_like_vscode_theme(&value) {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let id = slugify_theme_id(stem).ok_or_else(|| ThemeError::InvalidId {
            id: stem.to_owned(),
        })?;
        import_vscode_theme(&clean, &id)
    } else {
        Theme::from_json(&clean)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::{BUILTIN_THEME_IDS, DEFAULT_THEME_ID};
    use crate::color::Rgba;

    fn custom(id: &str) -> Theme {
        let mut t = default_theme().clone();
        t.id = id.to_owned();
        t.name = format!("Custom {id}");
        t
    }

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn new_registry_has_builtins_and_fallback() {
        let reg = ThemeRegistry::new();
        let ids: Vec<&str> = reg.list().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, BUILTIN_THEME_IDS);
        assert_eq!(reg.len(), BUILTIN_THEME_IDS.len());
        assert!(!reg.is_empty());
        assert_eq!(reg.get_or_default("does-not-exist").id, DEFAULT_THEME_ID);
        assert_eq!(reg.get_or_default("apex-light").id, "apex-light");
        assert_eq!(reg.source("apex-dark"), Some(&ThemeSource::BuiltIn));
        assert_eq!(reg.source("nope"), None);
    }

    #[test]
    fn add_replace_and_remove_plugin_themes() {
        let mut reg = ThemeRegistry::new();
        assert!(reg.add(custom("solar")).unwrap().is_none());
        assert!(reg.contains("solar"));
        let mut v2 = custom("solar");
        v2.name = "Solar v2".into();
        let old = reg.add(v2).unwrap().expect("replaced");
        assert_eq!(old.name, "Custom solar");
        assert_eq!(reg.get("solar").unwrap().name, "Solar v2");
        assert_eq!(reg.len(), BUILTIN_THEME_IDS.len() + 1);
        assert_eq!(reg.list().last().unwrap().id, "solar");

        assert!(matches!(
            reg.add(custom("apex-dark")),
            Err(ThemeError::BuiltinConflict { .. })
        ));
        assert!(matches!(
            reg.add(custom("Bad Id")),
            Err(ThemeError::InvalidId { .. })
        ));
        assert!(reg.remove("apex-dark").is_none());
        assert!(reg.remove("solar").is_some());
        assert!(!reg.contains("solar"));
    }

    #[test]
    fn remove_by_source_unloads_a_plugin() {
        let mut reg = ThemeRegistry::new();
        let plugin = ThemeSource::Plugin("acme.themes".into());
        reg.add_from(custom("acme-a"), plugin.clone()).unwrap();
        reg.add_from(custom("acme-b"), plugin.clone()).unwrap();
        reg.add(custom("other")).unwrap();
        assert_eq!(reg.source("acme-a"), Some(&plugin));
        assert_eq!(reg.remove_by_source(&plugin), 2);
        assert_eq!(reg.len(), BUILTIN_THEME_IDS.len() + 1);
        assert!(reg.contains("other"));
    }

    #[test]
    fn load_dir_collects_errors_and_keeps_going() {
        let dir = tempfile::tempdir().unwrap();
        let good = custom("my-good");
        write(dir.path(), "a-good.json", &good.to_json_pretty().unwrap());
        write(dir.path(), "b-broken.json", "{ not json");
        let mut bad_color: serde_json::Value =
            serde_json::from_str(&custom("bad-color").to_json_pretty().unwrap()).unwrap();
        bad_color["ui"]["accent"] = "blue".into();
        write(dir.path(), "c-bad-color.json", &bad_color.to_string());
        write(
            dir.path(),
            "d-builtin.json",
            &custom("apex-light").to_json_pretty().unwrap(),
        );
        write(dir.path(), "e-dupe.json", &good.to_json_pretty().unwrap());
        write(dir.path(), "notes.txt", "ignored");
        std::fs::create_dir(dir.path().join("sub.json")).unwrap();

        let (reg, report) = ThemeRegistry::with_user_dir(dir.path());
        assert_eq!(report.loaded, ["my-good"]);
        assert!(!report.is_ok());
        let failed: Vec<String> = report
            .errors
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            failed,
            [
                "b-broken.json",
                "c-bad-color.json",
                "d-builtin.json",
                "e-dupe.json"
            ]
        );
        assert!(report.errors[1].to_string().contains("must start with '#'"));
        assert!(matches!(
            report.errors[2].error,
            ThemeError::BuiltinConflict { .. }
        ));
        assert!(matches!(
            report.errors[3].error,
            ThemeError::DuplicateId { .. }
        ));
        assert_eq!(
            reg.source("my-good"),
            Some(&ThemeSource::User(dir.path().join("a-good.json")))
        );
        // Built-in apex-light is untouched.
        assert_eq!(reg.get("apex-light").unwrap().name, "Apex Light");
    }

    #[test]
    fn load_dir_accepts_jsonc_bom_and_vscode_themes() {
        let dir = tempfile::tempdir().unwrap();
        let native = custom("commented");
        let json = native.to_json_pretty().unwrap();
        let jsonc = format!(
            "\u{feff}// my theme\n{}",
            json.replacen('{', "{ /* hi */", 1)
        );
        write(dir.path(), "commented.JSON", &jsonc);
        write(
            dir.path(),
            "My Vs Theme.json",
            r##"{
                // VS Code style
                "name": "My VS Theme",
                "type": "light",
                "colors": { "editor.background": "#fdf6e3", "editor.foreground": "#4f5b62", },
                "tokenColors": [ { "scope": "keyword", "settings": { "foreground": "#859900" } } ],
            }"##,
        );

        let (reg, report) = ThemeRegistry::with_user_dir(dir.path());
        assert!(report.is_ok(), "{:?}", report.errors);
        assert_eq!(report.loaded, ["commented", "my-vs-theme"]);
        let vs = reg.get("my-vs-theme").unwrap();
        assert_eq!(vs.name, "My VS Theme");
        assert_eq!(vs.syntax.keyword.color, Rgba::rgb(0x85, 0x99, 0x00));
    }

    #[test]
    fn json_errors_point_at_original_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = ThemeRegistry::new();
        let path = write(
            dir.path(),
            "x.json",
            "// line 1\n/* line 2 */\n{ \"id\": 5 }",
        );
        let err = reg.load_file(&path).unwrap_err().to_string();
        assert!(err.contains("line 3"), "{err}");
    }

    #[test]
    fn missing_dir_is_empty_report() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = ThemeRegistry::new();
        let report = reg.load_dir(&dir.path().join("absent"));
        assert!(report.is_ok() && report.loaded.is_empty());
    }

    #[test]
    fn load_file_reports_io_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = ThemeRegistry::new();
        let err = reg.load_file(&dir.path().join("nope.json")).unwrap_err();
        assert!(matches!(err, ThemeError::Io { .. }));
        assert!(err.to_string().contains("nope.json"));
    }
}
