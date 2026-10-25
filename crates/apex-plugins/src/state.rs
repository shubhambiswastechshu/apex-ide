//! Persistent plugin state (`plugins.toml`): which plugins are disabled and
//! which permissions the user granted.
//!
//! The file lives in the plugins directory next to the plugin folders:
//!
//! ```toml
//! disabled = ["com.example.noisy"]
//!
//! [granted]
//! "com.example.reader" = ["fs-read"]
//! ```
//!
//! Plugins are enabled by default; only explicit opt-outs are stored, so a
//! newly installed plugin is active without touching this file. Folder
//! plugins get no permissions until the user grants them. Keys this version
//! does not know (written by a newer Apex) are kept when the file is saved.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::id::PluginId;
use crate::manifest::Permission;

/// File name of the state file inside the plugins directory.
pub const STATE_FILE_NAME: &str = "plugins.toml";

const HEADER: &str =
    "# Managed by Apex IDE. Plugins the user disabled, and permissions the user granted.\n";

/// Errors reading or writing `plugins.toml`.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// Reading, writing or renaming the file failed.
    #[error("plugin state I/O error at {}: {source}", path.display())]
    Io {
        /// File involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file exists but is not valid.
    #[error("invalid plugin state file {}: {source}", path.display())]
    Parse {
        /// File involved.
        path: PathBuf,
        /// Parser error.
        #[source]
        source: Box<toml::de::Error>,
    },
    /// The state could not be serialised.
    #[error("failed to serialise plugin state: {0}")]
    Serialize(#[from] toml::ser::Error),
}

/// Which plugins the user has disabled, and the permissions granted to each.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginState {
    #[serde(default)]
    disabled: BTreeSet<PluginId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    granted: BTreeMap<PluginId, BTreeSet<Permission>>,
    /// Keys from newer versions, kept so saving does not drop them.
    #[serde(flatten)]
    other: BTreeMap<String, toml::Value>,
}

impl PluginState {
    /// Loads state from `path`. A missing file yields the default state
    /// (everything enabled).
    pub fn load(path: &Path) -> Result<Self, StateError> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|source| StateError::Parse {
                path: path.to_path_buf(),
                source: Box::new(source),
            }),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(StateError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Writes state to `path`, creating parent directories. The write goes to
    /// a temporary sibling file that is then renamed over `path`, so a crash
    /// never leaves a half-written file behind.
    pub fn save(&self, path: &Path) -> Result<(), StateError> {
        let io_err = |path: &Path| {
            let path = path.to_path_buf();
            move |source| StateError::Io { path, source }
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(io_err(parent))?;
        }
        let body = format!("{HEADER}{}", toml::to_string(self)?);
        let mut tmp_name = path.as_os_str().to_owned();
        tmp_name.push(".tmp");
        let tmp = PathBuf::from(tmp_name);
        fs::write(&tmp, body).map_err(io_err(&tmp))?;
        // `std::fs::rename` replaces an existing destination on all supported
        // platforms (MoveFileExW with MOVEFILE_REPLACE_EXISTING on Windows).
        fs::rename(&tmp, path).map_err(|source| {
            let _ = fs::remove_file(&tmp);
            StateError::Io {
                path: path.to_path_buf(),
                source,
            }
        })
    }

    /// Whether `id` is enabled.
    pub fn is_enabled(&self, id: &PluginId) -> bool {
        !self.disabled.contains(id)
    }

    /// Enables or disables `id`. Returns `true` if the state changed.
    pub fn set_enabled(&mut self, id: &PluginId, enabled: bool) -> bool {
        if enabled {
            self.disabled.remove(id)
        } else {
            self.disabled.insert(id.clone())
        }
    }

    /// Ids of disabled plugins, sorted.
    pub fn disabled(&self) -> impl Iterator<Item = &PluginId> {
        self.disabled.iter()
    }

    /// Permissions the user granted to `id` (empty if none).
    pub fn granted(&self, id: &PluginId) -> BTreeSet<Permission> {
        self.granted.get(id).cloned().unwrap_or_default()
    }

    /// Replaces the permissions granted to `id`. Returns `true` if the state
    /// changed.
    pub fn set_granted(&mut self, id: &PluginId, permissions: BTreeSet<Permission>) -> bool {
        if self.granted(id) == permissions {
            return false;
        }
        if permissions.is_empty() {
            self.granted.remove(id);
        } else {
            self.granted.insert(id.clone(), permissions);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> PluginId {
        PluginId::new(s).unwrap()
    }

    #[test]
    fn missing_file_means_everything_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let state = PluginState::load(&dir.path().join(STATE_FILE_NAME)).unwrap();
        assert!(state.is_enabled(&id("a.b")));
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(STATE_FILE_NAME);
        let mut state = PluginState::default();
        assert!(state.set_enabled(&id("a.b"), false));
        assert!(!state.set_enabled(&id("a.b"), false));
        assert!(state.set_enabled(&id("c.d"), false));
        assert!(state.set_enabled(&id("c.d"), true));
        state.save(&path).unwrap();
        // Saving twice exercises the replace-existing rename path.
        state.save(&path).unwrap();
        assert!(!path.with_extension("toml.tmp").exists());

        let loaded = PluginState::load(&path).unwrap();
        assert_eq!(loaded, state);
        assert!(!loaded.is_enabled(&id("a.b")));
        assert!(loaded.is_enabled(&id("c.d")));
        assert_eq!(loaded.disabled().count(), 1);
    }

    #[test]
    fn grants_round_trip_and_unknown_keys_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE_NAME);
        fs::write(
            &path,
            "disabled = [\"com.example.noisy\"]\nfuture_flag = true\n\n\
             [granted]\n\"com.example.reader\" = [\"fs-read\"]\n\n[future_table]\nx = 1\n",
        )
        .unwrap();
        let mut state = PluginState::load(&path).unwrap();
        assert!(!state.is_enabled(&id("com.example.noisy")));
        assert_eq!(
            state.granted(&id("com.example.reader")),
            [Permission::FsRead].into_iter().collect()
        );
        assert!(state.granted(&id("com.example.noisy")).is_empty());

        assert!(state.set_enabled(&id("a.b"), false));
        assert!(state.set_granted(&id("a.b"), [Permission::Network].into_iter().collect()));
        assert!(!state.set_granted(&id("a.b"), [Permission::Network].into_iter().collect()));
        state.save(&path).unwrap();

        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("future_flag"), "{saved}");
        assert!(saved.contains("future_table"), "{saved}");
        let loaded = PluginState::load(&path).unwrap();
        assert_eq!(loaded, state);
        assert!(!loaded.is_enabled(&id("com.example.noisy")));
        let mut cleared = loaded.clone();
        assert!(cleared.set_granted(&id("a.b"), BTreeSet::new()));
        assert!(cleared.granted(&id("a.b")).is_empty());
    }

    #[test]
    fn corrupt_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE_NAME);
        fs::write(&path, "disabled = [\"Not An Id\"]").unwrap();
        assert!(matches!(
            PluginState::load(&path),
            Err(StateError::Parse { .. })
        ));
        fs::write(&path, "disabled = [").unwrap();
        assert!(matches!(
            PluginState::load(&path),
            Err(StateError::Parse { .. })
        ));
    }
}
