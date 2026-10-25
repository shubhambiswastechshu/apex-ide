//! Application-wide user settings, stored as TOML at
//! [`crate::paths::settings_file`].
//!
//! Loading never fails hard: a missing file yields defaults, and a corrupt
//! file yields defaults plus a warning (the corrupt file is moved aside to
//! `settings.toml.corrupt` so the next save does not destroy it).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::fs_util::{self, path_key};
use crate::paths;

/// Maximum number of entries kept in [`Settings::recent_projects`].
pub const MAX_RECENT_PROJECTS: usize = 10;
/// Smallest editor font size accepted; smaller values are clamped.
pub const MIN_FONT_SIZE: f32 = 6.0;
/// Largest editor font size accepted; larger values are clamped.
pub const MAX_FONT_SIZE: f32 = 72.0;

/// Theme id used when none is configured.
pub const DEFAULT_THEME: &str = "apex-vs-blue";
/// Default width of the project tool window.
pub const DEFAULT_PANEL_WIDTH: f32 = 280.0;
/// Narrowest project tool window accepted.
pub const MIN_PANEL_WIDTH: f32 = 160.0;
/// Widest project tool window accepted.
pub const MAX_PANEL_WIDTH: f32 = 900.0;
/// Font size used when none is configured: 13 logical pixels, the size of
/// Visual Studio's default 10 pt editor font at 100% zoom.
pub const DEFAULT_FONT_SIZE: f32 = 13.0;
/// The default font size of settings files written before
/// [`SETTINGS_VERSION`] 2; such files are migrated to [`DEFAULT_FONT_SIZE`].
pub const LEGACY_DEFAULT_FONT_SIZE: f32 = 14.0;
/// Format version of the settings file written by this build.
pub const SETTINGS_VERSION: u32 = 2;
/// Editor zoom steps in percent, as offered by the zoom drop-down and
/// stepped through by Zoom In / Zoom Out.
pub const ZOOM_STEPS: [u32; 13] = [50, 67, 75, 80, 90, 100, 110, 125, 150, 175, 200, 250, 300];
/// Editor zoom used when none is configured (percent).
pub const DEFAULT_ZOOM: u32 = 100;
/// Smallest editor zoom accepted (percent).
pub const MIN_ZOOM: u32 = 50;
/// Largest editor zoom accepted (percent).
pub const MAX_ZOOM: u32 = 300;

/// The next zoom step above (`up`) or below `zoom`, staying within
/// [`MIN_ZOOM`]..=[`MAX_ZOOM`].
pub fn zoom_step(zoom: u32, up: bool) -> u32 {
    if up {
        ZOOM_STEPS
            .iter()
            .copied()
            .find(|&z| z > zoom)
            .unwrap_or(MAX_ZOOM)
    } else {
        ZOOM_STEPS
            .iter()
            .rev()
            .copied()
            .find(|&z| z < zoom)
            .unwrap_or(MIN_ZOOM)
    }
}

/// The monospace font family used by default. Cascadia Code is bundled with
/// the app (see `apex-app/ui/fonts`), so it is available on every platform.
pub fn default_font_family() -> &'static str {
    "Cascadia Code"
}

/// User settings for the whole application.
///
/// Unknown keys in the file are ignored and missing keys take their default,
/// so older and newer settings files both load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Id of the active UI theme (see `apex-themes`).
    pub theme: String,
    /// Editor font family.
    pub font_family: String,
    /// Editor font size at 100% zoom, in logical pixels, within
    /// [`MIN_FONT_SIZE`]..=[`MAX_FONT_SIZE`].
    pub font_size: f32,
    /// Editor zoom in percent, within [`MIN_ZOOM`]..=[`MAX_ZOOM`].
    pub editor_zoom: u32,
    /// Format version of the file (0 for files written before versions
    /// existed); see [`Settings::migrate`].
    pub version: u32,
    /// Recently opened project roots, most recent first, at most
    /// [`MAX_RECENT_PROJECTS`], without duplicates.
    pub recent_projects: Vec<PathBuf>,
    /// Folder offered as the parent of new projects.
    pub default_projects_dir: PathBuf,
    /// Whether the project tool window (left side panel) is shown.
    pub project_panel_visible: bool,
    /// Width of the project tool window in logical pixels, within
    /// [`MIN_PANEL_WIDTH`]..=[`MAX_PANEL_WIDTH`].
    pub project_panel_width: f32,
    /// Whether the project tool window is docked on the right (like Visual
    /// Studio's Solution Explorer) rather than the left.
    pub project_panel_right: bool,
    /// UI density: compact (default) or comfortable.
    pub density: Density,
    /// Whether the main toolbar is shown.
    pub show_toolbar: bool,
    /// Whether the status bar is shown.
    pub show_status_bar: bool,
}

/// How tightly the UI packs rows, toolbars and controls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    /// 22px rows, 28px toolbars: as much code on screen as possible.
    #[default]
    Compact,
    /// Roomier rows and controls.
    Comfortable,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: DEFAULT_THEME.to_owned(),
            font_family: default_font_family().to_owned(),
            font_size: DEFAULT_FONT_SIZE,
            editor_zoom: DEFAULT_ZOOM,
            version: SETTINGS_VERSION,
            recent_projects: Vec::new(),
            default_projects_dir: paths::default_projects_dir()
                .unwrap_or_else(|_| PathBuf::from(paths::DEFAULT_PROJECTS_DIR_NAME)),
            project_panel_visible: true,
            project_panel_width: DEFAULT_PANEL_WIDTH,
            project_panel_right: true,
            density: Density::Compact,
            show_toolbar: true,
            show_status_bar: true,
        }
    }
}

/// The result of loading settings: always usable settings, plus a warning
/// when the file could not be read or parsed and defaults were used instead.
#[derive(Debug)]
pub struct LoadedSettings {
    /// The settings to use.
    pub settings: Settings,
    /// Why defaults were used instead of the file's contents, if they were.
    pub warning: Option<CoreError>,
}

impl Settings {
    /// Loads settings from the standard location. Never fails; see
    /// [`Settings::load_from`].
    pub fn load() -> LoadedSettings {
        match paths::settings_file() {
            Ok(path) => Self::load_from(&path),
            Err(e) => {
                tracing::warn!("using default settings: {e}");
                LoadedSettings {
                    settings: Self::default(),
                    warning: Some(e),
                }
            }
        }
    }

    /// Loads settings from `path`. Never fails:
    ///
    /// - missing file: defaults, no warning;
    /// - unreadable or corrupt file: defaults plus a warning (also logged via
    ///   `tracing`); a corrupt file is renamed to `<name>.corrupt`;
    /// - out-of-range values are repaired (see [`Settings::sanitize`]).
    pub fn load_from(path: &Path) -> LoadedSettings {
        match Self::try_load_from(path) {
            Ok(settings) => LoadedSettings {
                settings: settings.unwrap_or_default(),
                warning: None,
            },
            Err(err) => {
                tracing::warn!("using default settings: {err}");
                if matches!(err, CoreError::Parse { .. }) {
                    let backup = corrupt_backup_path(path);
                    if let Err(e) = fs::rename(path, &backup) {
                        tracing::warn!(
                            "could not move corrupt settings to {}: {e}",
                            backup.display()
                        );
                    }
                }
                LoadedSettings {
                    settings: Self::default(),
                    warning: Some(err),
                }
            }
        }
    }

    /// Strict variant of [`Settings::load_from`]: returns `Ok(None)` when the
    /// file does not exist and an error when it cannot be read or parsed.
    pub fn try_load_from(path: &Path) -> Result<Option<Self>> {
        let Some(text) = fs_util::read_optional(path)? else {
            return Ok(None);
        };
        let mut settings: Self = toml::from_str(&text).map_err(|e| CoreError::parse(path, e))?;
        // A file without `version` predates versioning: read it as 0, not
        // as the current version that `#[serde(default)]` would give.
        let has_version = text
            .parse::<toml::Table>()
            .is_ok_and(|t| t.contains_key("version"));
        if !has_version {
            settings.version = 0;
        }
        settings.migrate();
        settings.sanitize();
        Ok(Some(settings))
    }

    /// Saves settings to the standard location.
    pub fn save(&self) -> Result<()> {
        self.save_to(&paths::settings_file()?)
    }

    /// Saves settings to `path` atomically, creating parent directories.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        fs_util::write_atomic(path, &text)
    }

    /// Upgrades settings written by older builds to [`SETTINGS_VERSION`].
    ///
    /// Version 2 made 13 px the normal (100%) editor size: older files carry
    /// the old 14 px default, which is reset to the new default (a size the
    /// user picked by hand is kept), and any stored zoom restarts at 100%.
    pub fn migrate(&mut self) {
        if self.version < 2 {
            if self.font_size == LEGACY_DEFAULT_FONT_SIZE {
                self.font_size = DEFAULT_FONT_SIZE;
            }
            self.editor_zoom = DEFAULT_ZOOM;
        }
        self.version = self.version.max(SETTINGS_VERSION);
    }

    /// Repairs values a hand-edited file may have broken: clamps the font
    /// size and zoom, restores empty theme/font names, and dedupes and caps
    /// the recent-projects list.
    pub fn sanitize(&mut self) {
        self.editor_zoom = self.editor_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.font_size = if self.font_size.is_finite() {
            self.font_size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
        } else {
            DEFAULT_FONT_SIZE
        };
        self.project_panel_width = if self.project_panel_width.is_finite() {
            self.project_panel_width
                .clamp(MIN_PANEL_WIDTH, MAX_PANEL_WIDTH)
        } else {
            DEFAULT_PANEL_WIDTH
        };
        if self.theme.trim().is_empty() {
            self.theme = DEFAULT_THEME.to_owned();
        }
        if self.font_family.trim().is_empty() {
            self.font_family = default_font_family().to_owned();
        }
        let mut seen = std::collections::HashSet::new();
        self.recent_projects
            .retain(|p| !p.as_os_str().is_empty() && seen.insert(path_key(p)));
        self.recent_projects.truncate(MAX_RECENT_PROJECTS);
    }

    /// Records `project_root` as the most recently opened project: it moves
    /// to the front (removing any earlier entry for the same folder, compared
    /// case-insensitively on Windows) and the list is capped at
    /// [`MAX_RECENT_PROJECTS`]. Relative paths are made absolute.
    pub fn add_recent_project(&mut self, project_root: impl AsRef<Path>) {
        let root = project_root.as_ref();
        let root = fs_util::absolute(root).unwrap_or_else(|_| root.to_path_buf());
        let key = path_key(&root);
        self.recent_projects.retain(|p| path_key(p) != key);
        self.recent_projects.insert(0, root);
        self.recent_projects.truncate(MAX_RECENT_PROJECTS);
    }

    /// Removes `project_root` from the recent list. Returns whether it was
    /// present.
    pub fn remove_recent_project(&mut self, project_root: impl AsRef<Path>) -> bool {
        let key = path_key(project_root.as_ref());
        let before = self.recent_projects.len();
        self.recent_projects.retain(|p| path_key(p) != key);
        self.recent_projects.len() != before
    }

    /// Drops recent entries whose folder no longer exists. Returns how many
    /// were removed.
    pub fn prune_missing_recent_projects(&mut self) -> usize {
        let before = self.recent_projects.len();
        self.recent_projects.retain(|p| p.is_dir());
        before - self.recent_projects.len()
    }
}

fn corrupt_backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| paths::SETTINGS_FILE_NAME.into());
    name.push(".corrupt");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("ApexIDE").join("settings.toml")
    }

    #[test]
    fn missing_file_gives_defaults_without_warning() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = Settings::load_from(&settings_path(&dir));
        assert_eq!(loaded.settings, Settings::default());
        assert!(loaded.warning.is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = settings_path(&dir);
        let mut s = Settings {
            theme: "solarized-light".into(),
            font_family: "JetBrains Mono".into(),
            font_size: 16.5,
            ..Settings::default()
        };
        s.add_recent_project(dir.path().join("proj"));
        s.save_to(&path).unwrap();

        let loaded = Settings::load_from(&path);
        assert!(loaded.warning.is_none());
        assert_eq!(loaded.settings, s);
    }

    #[test]
    fn corrupt_file_gives_defaults_warning_and_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = settings_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "theme = [this is not toml").unwrap();

        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.settings, Settings::default());
        assert!(matches!(loaded.warning, Some(CoreError::Parse { .. })));
        assert!(!path.exists());
        assert!(path.with_file_name("settings.toml.corrupt").exists());
    }

    #[test]
    fn wrong_types_count_as_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        fs::write(&path, "font_size = \"huge\"").unwrap();
        let loaded = Settings::load_from(&path);
        assert!(loaded.warning.is_some());
        assert_eq!(loaded.settings, Settings::default());
    }

    #[test]
    fn partial_file_fills_defaults_and_ignores_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        fs::write(
            &path,
            "theme = \"nord\"\nfont_size = 12\nfuture_key = true\n",
        )
        .unwrap();
        let loaded = Settings::load_from(&path);
        assert!(loaded.warning.is_none());
        assert_eq!(loaded.settings.theme, "nord");
        assert_eq!(loaded.settings.font_size, 12.0);
        assert_eq!(loaded.settings.font_family, default_font_family());
    }

    #[test]
    fn out_of_range_values_are_sanitized_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        fs::write(&path, "theme = \"  \"\nfont_size = 500.0\n").unwrap();
        let s = Settings::load_from(&path).settings;
        assert_eq!(s.font_size, MAX_FONT_SIZE);
        assert_eq!(s.theme, DEFAULT_THEME);

        let mut tiny = Settings {
            font_size: 1.0,
            project_panel_width: 5.0,
            ..Settings::default()
        };
        tiny.sanitize();
        assert_eq!(tiny.font_size, MIN_FONT_SIZE);
        assert_eq!(tiny.project_panel_width, MIN_PANEL_WIDTH);
        assert!(Settings::default().project_panel_visible);
        assert_eq!(Settings::default().density, Density::Compact);
    }

    #[test]
    fn old_files_migrate_to_normal_size_at_100_percent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        // The file written by builds before zoom existed.
        fs::write(
            &path,
            "theme = \"apex-vs-blue\"
font_family = \"Cascadia Code\"
font_size = 14.0
",
        )
        .unwrap();
        let s = Settings::load_from(&path).settings;
        assert_eq!(s.font_size, DEFAULT_FONT_SIZE);
        assert_eq!(s.editor_zoom, 100);
        assert_eq!(s.version, SETTINGS_VERSION);

        // A hand-picked size survives; a stale zoom resets.
        fs::write(
            &path,
            "font_size = 16.0
editor_zoom = 180
",
        )
        .unwrap();
        let s = Settings::load_from(&path).settings;
        assert_eq!(s.font_size, 16.0);
        assert_eq!(s.editor_zoom, 100);

        // Current files keep both, and 14 is a valid explicit choice now.
        let current = Settings {
            font_size: 14.0,
            editor_zoom: 125,
            ..Settings::default()
        };
        current.save_to(&path).unwrap();
        let s = Settings::load_from(&path).settings;
        assert_eq!((s.font_size, s.editor_zoom), (14.0, 125));

        // Out-of-range zoom is clamped.
        fs::write(
            &path,
            "version = 2
editor_zoom = 900
",
        )
        .unwrap();
        assert_eq!(Settings::load_from(&path).settings.editor_zoom, MAX_ZOOM);
    }

    #[test]
    fn zoom_steps_walk_the_preset_list() {
        assert_eq!(zoom_step(100, true), 110);
        assert_eq!(zoom_step(100, false), 90);
        assert_eq!(zoom_step(300, true), 300);
        assert_eq!(zoom_step(50, false), 50);
        // Off-list values snap to the neighbouring step.
        assert_eq!(zoom_step(105, true), 110);
        assert_eq!(zoom_step(105, false), 100);
        assert!(ZOOM_STEPS.contains(&DEFAULT_ZOOM));
    }

    #[test]
    fn recent_projects_dedupe_and_move_to_front() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let mut s = Settings::default();
        s.add_recent_project(&a);
        s.add_recent_project(&b);
        s.add_recent_project(&a);
        assert_eq!(s.recent_projects, vec![a.clone(), b.clone()]);

        // A trailing separator or `.` component is the same folder.
        s.add_recent_project(b.join("."));
        assert_eq!(s.recent_projects.len(), 2);
        assert_eq!(path_key(&s.recent_projects[0]), path_key(&b));
    }

    #[cfg(windows)]
    #[test]
    fn recent_projects_dedupe_is_case_insensitive_on_windows() {
        let mut s = Settings::default();
        s.add_recent_project(r"C:\Work\Proj");
        s.add_recent_project(r"c:/work/proj");
        assert_eq!(s.recent_projects.len(), 1);
    }

    #[test]
    fn recent_projects_capped_at_ten() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Settings::default();
        for i in 0..15 {
            s.add_recent_project(dir.path().join(format!("p{i}")));
        }
        assert_eq!(s.recent_projects.len(), MAX_RECENT_PROJECTS);
        assert_eq!(s.recent_projects[0], dir.path().join("p14"));
        assert_eq!(s.recent_projects[9], dir.path().join("p5"));
    }

    #[test]
    fn duplicate_recent_entries_in_file_are_removed_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let p = dir.path().join("proj");
        let s = Settings {
            recent_projects: vec![p.clone(), p.clone()],
            ..Settings::default()
        };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).settings.recent_projects, vec![p]);
    }

    #[test]
    fn remove_and_prune_recent_projects() {
        let dir = tempfile::tempdir().unwrap();
        let exists = dir.path().join("exists");
        fs::create_dir(&exists).unwrap();
        let gone = dir.path().join("gone");

        let mut s = Settings::default();
        s.add_recent_project(&exists);
        s.add_recent_project(&gone);
        assert_eq!(s.prune_missing_recent_projects(), 1);
        assert_eq!(s.recent_projects, vec![exists.clone()]);

        assert!(s.remove_recent_project(&exists));
        assert!(!s.remove_recent_project(&exists));
        assert!(s.recent_projects.is_empty());
    }

    #[test]
    fn default_projects_dir_is_apex_projects() {
        assert!(Settings::default()
            .default_projects_dir
            .ends_with(paths::DEFAULT_PROJECTS_DIR_NAME));
    }
}
