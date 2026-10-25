//! Plugin runtimes: how plugin code gets loaded and run.
//!
//! A [`PluginRuntime`] turns a [`PluginDescriptor`] into a running
//! [`LoadedPlugin`], giving the plugin a [`PluginHost`] to call back into the
//! IDE. Today there is one runtime, [`NativeBuiltinRuntime`], which runs Rust
//! plugins compiled into the app (implementors of [`BuiltinPlugin`]).
//!
//! Plugins with no `entry` are *declarative*: their manifest contributions
//! (themes, languages, snippets, ...) are used as-is and no runtime is
//! involved.
//!
//! # Seam for a future WASM runtime
//!
//! Third-party code plugins are meant to ship a WebAssembly component as
//! their manifest `entry` (e.g. `entry = "plugin.wasm"`). Supporting them
//! needs **no changes to the manager or the host API** — only a new type that
//! implements [`PluginRuntime`] and is added with
//! [`PluginManager::add_runtime`](crate::PluginManager::add_runtime):
//!
//! * `can_load` returns `true` for [`PluginSource::Directory`] plugins whose
//!   [`PluginDescriptor::entry_path`] ends in `.wasm`.
//! * `load` compiles the component with `wasmtime` (component model) and
//!   links a WIT world, roughly:
//!
//!   ```text
//!   package apex:plugin@0.1.0;
//!   interface host {
//!       enum log-level { trace, debug, info, warn, error }
//!       log: func(level: log-level, message: string);
//!       show-notification: func(kind: u8, message: string);
//!       register-command: func(id: string) -> result<_, string>;
//!       read-file: func(path: string) -> result<list<u8>, string>;
//!   }
//!   world plugin {
//!       import host;
//!       export activate: func() -> result<_, string>;
//!       export execute-command: func(id: string, args: list<string>) -> result<_, string>;
//!       export deactivate: func();
//!   }
//!   ```
//!
//!   Each import forwards to the `&mut dyn PluginHost` passed to `load`, so
//!   permission checks stay in one place ([`ScopedHost`](crate::ScopedHost)).
//!   For `register-command`, the runtime registers a
//!   [`CommandHandler`](crate::CommandHandler) closure holding an
//!   `Arc<Mutex<Store>>` that calls the guest's `execute-command` export.
//! * WASI capabilities are derived from [`Permission`](crate::Permission)s:
//!   no preopened directories without `fs-read`/`fs-write`, no sockets
//!   without `network`, no process spawning at all (the `process` permission
//!   maps to a host function, never raw WASI).
//! * The returned [`LoadedPlugin`] owns the `Store`/instance; `deactivate`
//!   calls the guest export and drops it.
//!
//! `wasmtime` is deliberately not a dependency yet.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::descriptor::{PluginDescriptor, PluginSource};
use crate::error::PluginError;
use crate::host::{HostError, PluginHost};
use crate::id::PluginId;
use crate::manifest::{ManifestError, PluginManifest};

/// Errors raised by runtimes and plugin code.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// This runtime does not know the plugin.
    #[error("plugin `{0}` is not provided by this runtime")]
    NotFound(PluginId),
    /// A host call made by the plugin failed (e.g. permission denied).
    #[error(transparent)]
    Host(#[from] HostError),
    /// The plugin reported a failure.
    #[error("{0}")]
    Failed(String),
}

impl RuntimeError {
    /// Convenience constructor for [`RuntimeError::Failed`].
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }
}

/// A plugin instance that has been activated by a runtime.
pub trait LoadedPlugin: Send {
    /// Called once when the plugin is deactivated (disabled or app shutdown).
    /// Its command handlers are removed by the manager afterwards either way.
    fn deactivate(&mut self, host: &mut dyn PluginHost) -> Result<(), RuntimeError> {
        let _ = host;
        Ok(())
    }
}

/// Loads plugin code. See the [module docs](self) for the WASM seam.
pub trait PluginRuntime: Send {
    /// Short name for diagnostics, e.g. `native` or `wasm`.
    fn name(&self) -> &str;

    /// Whether this runtime can load `plugin`.
    fn can_load(&self, plugin: &PluginDescriptor) -> bool;

    /// Loads and activates `plugin`. The plugin registers its command
    /// handlers through `host` during this call.
    fn load(
        &mut self,
        plugin: &PluginDescriptor,
        host: &mut dyn PluginHost,
    ) -> Result<Box<dyn LoadedPlugin>, RuntimeError>;
}

/// A plugin written in Rust and compiled into the application.
///
/// Built-ins use the same manifest format and validation as plugins on disk,
/// usually by embedding a TOML string:
///
/// ```
/// use apex_plugins::{BuiltinPlugin, ManifestError, PluginHost, PluginManifest, RuntimeError};
///
/// struct Mine;
/// impl BuiltinPlugin for Mine {
///     fn manifest(&self) -> Result<PluginManifest, ManifestError> {
///         PluginManifest::from_toml_str(
///             r#"id = "apex.mine"
///                name = "Mine"
///                version = "0.1.0"
///                apex_api = "^0.1""#,
///         )
///     }
///     fn activate(&self, _host: &mut dyn PluginHost) -> Result<(), RuntimeError> {
///         Ok(())
///     }
/// }
/// ```
pub trait BuiltinPlugin: Send + Sync {
    /// The plugin's manifest. Called once at registration.
    fn manifest(&self) -> Result<PluginManifest, ManifestError>;

    /// Activates the plugin: register command handlers, set up state.
    /// May be called again after a deactivation.
    fn activate(&self, host: &mut dyn PluginHost) -> Result<(), RuntimeError>;

    /// Tears down state created in `activate`.
    fn deactivate(&self, host: &mut dyn PluginHost) -> Result<(), RuntimeError> {
        let _ = host;
        Ok(())
    }
}

/// Runtime for in-process [`BuiltinPlugin`]s, keyed by plugin id.
#[derive(Default)]
pub struct NativeBuiltinRuntime {
    plugins: BTreeMap<PluginId, Arc<dyn BuiltinPlugin>>,
}

impl std::fmt::Debug for NativeBuiltinRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeBuiltinRuntime")
            .field("plugins", &self.plugins.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl NativeBuiltinRuntime {
    /// Creates an empty runtime.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a built-in plugin and returns its validated manifest.
    pub fn register(
        &mut self,
        plugin: Arc<dyn BuiltinPlugin>,
    ) -> Result<PluginManifest, PluginError> {
        let manifest = plugin
            .manifest()
            .map_err(|source| PluginError::InvalidManifest {
                origin: PluginSource::Builtin.to_string(),
                source,
            })?;
        if self.plugins.contains_key(manifest.id()) {
            return Err(PluginError::DuplicatePlugin {
                id: manifest.id().clone(),
                existing: PluginSource::Builtin.to_string(),
            });
        }
        self.plugins.insert(manifest.id().clone(), plugin);
        Ok(manifest)
    }

    /// Whether a plugin with `id` is registered.
    pub fn contains(&self, id: &PluginId) -> bool {
        self.plugins.contains_key(id)
    }
}

struct NativeLoaded {
    plugin: Arc<dyn BuiltinPlugin>,
}

impl LoadedPlugin for NativeLoaded {
    fn deactivate(&mut self, host: &mut dyn PluginHost) -> Result<(), RuntimeError> {
        self.plugin.deactivate(host)
    }
}

impl PluginRuntime for NativeBuiltinRuntime {
    fn name(&self) -> &str {
        "native"
    }

    fn can_load(&self, plugin: &PluginDescriptor) -> bool {
        plugin.source == PluginSource::Builtin && self.contains(plugin.id())
    }

    fn load(
        &mut self,
        plugin: &PluginDescriptor,
        host: &mut dyn PluginHost,
    ) -> Result<Box<dyn LoadedPlugin>, RuntimeError> {
        let builtin = self
            .plugins
            .get(plugin.id())
            .cloned()
            .ok_or_else(|| RuntimeError::NotFound(plugin.id().clone()))?;
        builtin.activate(host)?;
        Ok(Box::new(NativeLoaded { plugin: builtin }))
    }
}
