//! The crate-level error type.

use std::path::PathBuf;

use crate::host::CommandError;
use crate::id::PluginId;
use crate::manifest::ManifestError;
use crate::runtime::RuntimeError;
use crate::state::StateError;

/// Errors produced by discovery, the [`PluginManager`](crate::PluginManager)
/// and command dispatch.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// A manifest failed validation. `origin` is the manifest path, or
    /// `built-in` for plugins compiled into the app.
    #[error("invalid plugin manifest ({origin}): {source}")]
    InvalidManifest {
        /// Where the manifest came from.
        origin: String,
        /// Validation failure.
        #[source]
        source: ManifestError,
    },
    /// A file system operation failed.
    #[error("I/O error at {}: {source}", path.display())]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// `plugins.toml` could not be read or written.
    #[error(transparent)]
    State(#[from] StateError),
    /// No plugin with this id is registered.
    #[error("unknown plugin `{0}`")]
    UnknownPlugin(PluginId),
    /// Two plugins share an id.
    #[error("plugin `{id}` is already registered from {existing}")]
    DuplicatePlugin {
        /// The clashing id.
        id: PluginId,
        /// Where the already-registered plugin came from.
        existing: String,
    },
    /// The plugin is disabled, so it cannot be activated.
    #[error("plugin `{0}` is disabled")]
    Disabled(PluginId),
    /// The plugin has an `entry` but no registered runtime can load it.
    #[error("no runtime can load plugin `{0}` (WASM plugins are not supported yet)")]
    NoRuntime(PluginId),
    /// A runtime failed to load, activate or deactivate the plugin.
    #[error("plugin `{plugin}` failed: {source}")]
    Runtime {
        /// The failing plugin.
        plugin: PluginId,
        /// Runtime error.
        #[source]
        source: RuntimeError,
    },
    /// A plugin declares a command id that an active plugin already owns.
    #[error("command `{command}` of plugin `{plugin}` is already provided by `{owner}`")]
    CommandConflict {
        /// The clashing command id.
        command: String,
        /// Current owner.
        owner: PluginId,
        /// Plugin that tried to claim it.
        plugin: PluginId,
    },
    /// No active plugin provides this command.
    #[error("unknown command `{0}`")]
    UnknownCommand(String),
    /// The command is declared in a manifest but its plugin registered no handler.
    #[error("command `{command}` is declared by `{plugin}` but has no handler")]
    CommandNotImplemented {
        /// Command id.
        command: String,
        /// Owning plugin.
        plugin: PluginId,
    },
    /// The command handler returned an error.
    #[error("command `{command}` failed: {source}")]
    CommandFailed {
        /// Command id.
        command: String,
        /// Handler error.
        #[source]
        source: CommandError,
    },
}
