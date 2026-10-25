//! A manifest together with where the plugin lives.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::id::PluginId;
use crate::manifest::PluginManifest;

/// Where a plugin comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSource {
    /// Compiled into the application and run in-process by
    /// [`NativeBuiltinRuntime`](crate::NativeBuiltinRuntime).
    Builtin,
    /// A folder on disk containing `apex-plugin.toml`.
    Directory(PathBuf),
}

impl fmt::Display for PluginSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PluginSource::Builtin => f.write_str("built-in"),
            PluginSource::Directory(dir) => write!(f, "{}", dir.display()),
        }
    }
}

/// A validated manifest plus its [`PluginSource`]. This is what runtimes are
/// given when asked to load a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDescriptor {
    /// The validated manifest.
    pub manifest: PluginManifest,
    /// Where the plugin lives.
    pub source: PluginSource,
}

impl PluginDescriptor {
    /// Shorthand for `self.manifest.id()`.
    pub fn id(&self) -> &PluginId {
        self.manifest.id()
    }

    /// The plugin folder, or `None` for built-in plugins.
    pub fn root(&self) -> Option<&Path> {
        match &self.source {
            PluginSource::Builtin => None,
            PluginSource::Directory(dir) => Some(dir),
        }
    }

    /// Resolves a manifest-relative path against the plugin folder.
    ///
    /// Built-in plugins have no folder, so their paths are returned unchanged
    /// (the app decides how to interpret them, e.g. as embedded resources).
    pub fn resolve(&self, relative: &Path) -> PathBuf {
        match self.root() {
            Some(root) => root.join(relative),
            None => relative.to_path_buf(),
        }
    }

    /// The resolved `entry` path, if the manifest declares one.
    pub fn entry_path(&self) -> Option<PathBuf> {
        self.manifest.entry().map(|e| self.resolve(e))
    }
}
