//! Per-user directories used by Apex IDE.
//!
//! | Function                 | Windows                               | macOS                                         | Linux                         |
//! |--------------------------|---------------------------------------|-----------------------------------------------|-------------------------------|
//! | [`config_dir`]           | `%APPDATA%\ApexIDE`                   | `~/Library/Application Support/ApexIDE`       | `~/.config/ApexIDE`           |
//! | [`data_dir`]             | `%APPDATA%\ApexIDE`                   | `~/Library/Application Support/ApexIDE`       | `~/.local/share/ApexIDE`      |
//! | [`plugins_dir`]          | `<data_dir>\plugins`                  | `<data_dir>/plugins`                          | `<data_dir>/plugins`          |
//! | [`themes_dir`]           | `<data_dir>\themes`                   | `<data_dir>/themes`                           | `<data_dir>/themes`           |
//! | [`default_projects_dir`] | `%USERPROFILE%\ApexProjects`          | `~/ApexProjects`                              | `~/ApexProjects`              |
//!
//! None of these functions create the directory; use [`ensure_dir`] for that.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};

/// Name of the application folder inside the OS config/data directories.
pub const APP_DIR_NAME: &str = "ApexIDE";

/// File name of the application settings inside [`config_dir`].
pub const SETTINGS_FILE_NAME: &str = "settings.toml";

/// Name of the default projects folder inside the user's home directory.
pub const DEFAULT_PROJECTS_DIR_NAME: &str = "ApexProjects";

/// Directory for user configuration (settings).
pub fn config_dir() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|d| d.join(APP_DIR_NAME))
        .ok_or(CoreError::MissingSystemDir("config"))
}

/// Directory for application data (plugins, themes, caches).
pub fn data_dir() -> Result<PathBuf> {
    dirs::data_dir()
        .map(|d| d.join(APP_DIR_NAME))
        .ok_or(CoreError::MissingSystemDir("data"))
}

/// Directory where installed plugins live.
pub fn plugins_dir() -> Result<PathBuf> {
    data_dir().map(|d| d.join("plugins"))
}

/// Directory where user-installed themes live.
pub fn themes_dir() -> Result<PathBuf> {
    data_dir().map(|d| d.join("themes"))
}

/// Full path of the settings file, `<config_dir>/settings.toml`.
pub fn settings_file() -> Result<PathBuf> {
    config_dir().map(|d| d.join(SETTINGS_FILE_NAME))
}

/// Path of the user keymap overrides (`<config_dir>/keymap.toml`).
pub fn keymap_file() -> Result<PathBuf> {
    config_dir().map(|d| d.join(crate::keymap::KEYMAP_FILE_NAME))
}

/// The default parent folder for new projects, `~/ApexProjects`.
pub fn default_projects_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|h| h.join(DEFAULT_PROJECTS_DIR_NAME))
        .ok_or(CoreError::MissingSystemDir("home"))
}

/// Creates `dir` (and its parents) if missing and returns it.
pub fn ensure_dir(dir: impl AsRef<Path>) -> Result<PathBuf> {
    let dir = dir.as_ref();
    fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
    Ok(dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_dirs_end_with_app_name() {
        assert!(config_dir().unwrap().ends_with(APP_DIR_NAME));
        assert!(data_dir().unwrap().ends_with(APP_DIR_NAME));
    }

    #[test]
    fn plugin_and_theme_dirs_live_under_data_dir() {
        let data = data_dir().unwrap();
        assert_eq!(plugins_dir().unwrap(), data.join("plugins"));
        assert_eq!(themes_dir().unwrap(), data.join("themes"));
    }

    #[test]
    fn settings_file_lives_in_config_dir() {
        assert_eq!(
            settings_file().unwrap(),
            config_dir().unwrap().join(SETTINGS_FILE_NAME)
        );
    }

    #[test]
    fn default_projects_dir_is_in_home() {
        let dir = default_projects_dir().unwrap();
        assert!(dir.ends_with(DEFAULT_PROJECTS_DIR_NAME));
        assert_eq!(dir.parent(), dirs::home_dir().as_deref());
    }

    #[test]
    fn ensure_dir_creates_nested_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a").join("b");
        let out = ensure_dir(&nested).unwrap();
        assert!(out.is_dir());
        // Idempotent.
        ensure_dir(&nested).unwrap();
    }
}
