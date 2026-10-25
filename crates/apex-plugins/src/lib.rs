//! # apex-plugins
//!
//! The plugin system for Apex IDE.
//!
//! * **Manifests** — every plugin is described by an `apex-plugin.toml`
//!   ([`PluginManifest`]), validated on load: reverse-DNS [`PluginId`],
//!   semver `version`, an `apex_api` requirement checked against
//!   [`HOST_API_VERSION`], declared [`Permission`]s and static
//!   [`Contributions`] (themes, commands, languages, snippets, DB drivers).
//! * **Discovery** — [`discover_plugins`] scans a plugins folder; one bad
//!   plugin never fails the scan. Enable/disable choices live in
//!   `plugins.toml` next to the plugin folders ([`PluginState`]).
//! * **Runtimes** — plugin code runs behind [`PluginRuntime`] and talks to
//!   the IDE only through [`PluginHost`], whose standard implementation
//!   [`ScopedHost`] enforces permissions. [`NativeBuiltinRuntime`] runs
//!   [`BuiltinPlugin`]s compiled into the app; the [`runtime`] module docs
//!   describe the seam for a future WASM runtime.
//! * **Manager** — [`PluginManager`] ties it together: registry,
//!   enable/disable, activation, aggregated [`ContributionSet`] and command
//!   dispatch by id.

#![warn(missing_docs)]

mod builtin;
mod descriptor;
mod discovery;
mod error;
mod host;
mod id;
mod manager;
mod manifest;
pub mod runtime;
mod state;

pub use builtin::{builtin_plugins, HelloPlugin};
pub use descriptor::{PluginDescriptor, PluginSource};
pub use discovery::{discover_plugins, DiscoveryReport, PluginIssue};
pub use error::PluginError;
pub use host::{
    command_handler, CommandError, CommandHandler, HostBackend, HostError, LogLevel, Notification,
    NotificationKind, PluginHost, ScopedHost, TracingBackend, WorkspaceRoot,
};
pub use id::{InvalidPluginId, PluginId};
pub use manager::{
    ContributedCommand, ContributedDbDriver, ContributedLanguage, ContributedPath, ContributionSet,
    PluginInfo, PluginManager, PluginStatus, ScanSummary, CORRUPT_STATE_SUFFIX,
};
pub use manifest::{
    CommandContribution, Contributions, LanguageContribution, ManifestError, Permission,
    PluginManifest, HOST_API_VERSION, MANIFEST_FILE_NAME,
};
pub use runtime::{BuiltinPlugin, LoadedPlugin, NativeBuiltinRuntime, PluginRuntime, RuntimeError};
pub use state::{PluginState, StateError, STATE_FILE_NAME};
