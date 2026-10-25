//! The API surface the IDE exposes to plugins, and its permission gate.
//!
//! There are two layers:
//!
//! * [`HostBackend`] is implemented once by the application. It performs the
//!   real work (write to the log panel, show a toast, read a file) and knows
//!   nothing about permissions.
//! * [`PluginHost`] is what a plugin talks to. [`ScopedHost`] is the standard
//!   implementation: one instance per active plugin, which attributes every
//!   call to that plugin and checks its declared [`Permission`]s before
//!   forwarding to the backend. Runtimes (native or a future WASM runtime)
//!   only ever hand plugins a `PluginHost`, never the backend.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use crate::id::PluginId;
use crate::manifest::{CommandContribution, Permission, PluginManifest};

/// Severity of a plugin log message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    /// Very verbose diagnostics.
    Trace,
    /// Debugging information.
    Debug,
    /// Normal operational messages.
    Info,
    /// Something unexpected that the plugin recovered from.
    Warn,
    /// A failure.
    Error,
}

/// Severity of a user-visible notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NotificationKind {
    /// Informational.
    Info,
    /// Warning.
    Warning,
    /// Error.
    Error,
}

/// A user-visible notification (toast / balloon).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// Severity.
    pub kind: NotificationKind,
    /// Message text.
    pub message: String,
}

impl Notification {
    /// An informational notification.
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            kind: NotificationKind::Info,
            message: message.into(),
        }
    }

    /// A warning notification.
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            kind: NotificationKind::Warning,
            message: message.into(),
        }
    }

    /// An error notification.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            kind: NotificationKind::Error,
            message: message.into(),
        }
    }
}

/// Errors returned by [`PluginHost`] functions.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The plugin did not declare the permission the call requires.
    #[error("plugin `{plugin}` lacks the `{permission}` permission")]
    PermissionDenied {
        /// Calling plugin.
        plugin: PluginId,
        /// Missing permission.
        permission: Permission,
    },
    /// The plugin tried to register a command it did not declare in
    /// `contributes.commands`.
    #[error("plugin `{plugin}` did not declare command `{command}` in its manifest")]
    UndeclaredCommand {
        /// Calling plugin.
        plugin: PluginId,
        /// Command id.
        command: String,
    },
    /// A handler for this command is already registered.
    #[error("command `{0}` already has a handler")]
    CommandAlreadyRegistered(String),
    /// The path is outside the folders the plugin may access (its own
    /// folder and the open project).
    #[error("plugin `{plugin}` may not access {}", path.display())]
    OutsideScope {
        /// Calling plugin.
        plugin: PluginId,
        /// Requested path.
        path: PathBuf,
    },
    /// A file system call failed.
    #[error("I/O error at {}: {source}", path.display())]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// Error returned by a command handler.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct CommandError {
    message: String,
}

impl CommandError {
    /// Creates an error with a human-readable message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<HostError> for CommandError {
    fn from(err: HostError) -> Self {
        Self::new(err.to_string())
    }
}

/// A command implementation. It receives the owning plugin's host (so it can
/// log, notify or read files under the same permissions) and string
/// arguments supplied by the caller.
pub type CommandHandler =
    Arc<dyn Fn(&dyn PluginHost, &[String]) -> Result<(), CommandError> + Send + Sync>;

/// Wraps a closure as a [`CommandHandler`], helping type inference:
///
/// ```
/// use apex_plugins::{command_handler, Notification};
/// let handler = command_handler(|host, _args| {
///     host.show_notification(Notification::info("hi"));
///     Ok(())
/// });
/// # let _ = handler;
/// ```
pub fn command_handler<F>(f: F) -> CommandHandler
where
    F: Fn(&dyn PluginHost, &[String]) -> Result<(), CommandError> + Send + Sync + 'static,
{
    Arc::new(f)
}

/// Application services backing every [`PluginHost`]. Implemented by the app
/// (e.g. forwarding to the Slint UI); must be cheap to share across threads.
///
/// Implementations do **not** check permissions — [`ScopedHost`] does that
/// before calling in.
pub trait HostBackend: Send + Sync {
    /// Records a log line from `plugin`.
    fn log(&self, plugin: &PluginId, level: LogLevel, message: &str);

    /// Shows `notification` to the user on behalf of `plugin`.
    fn show_notification(&self, plugin: &PluginId, notification: &Notification);

    /// Reads a whole file. The default reads from the local file system.
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}

/// A [`HostBackend`] that forwards logs and notifications to `tracing`.
/// Useful as a default before the UI is wired up, and in tools/tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingBackend;

impl HostBackend for TracingBackend {
    fn log(&self, plugin: &PluginId, level: LogLevel, message: &str) {
        let plugin = plugin.as_str();
        match level {
            LogLevel::Trace => tracing::trace!(plugin, "{message}"),
            LogLevel::Debug => tracing::debug!(plugin, "{message}"),
            LogLevel::Info => tracing::info!(plugin, "{message}"),
            LogLevel::Warn => tracing::warn!(plugin, "{message}"),
            LogLevel::Error => tracing::error!(plugin, "{message}"),
        }
    }

    fn show_notification(&self, plugin: &PluginId, notification: &Notification) {
        tracing::info!(
            plugin = plugin.as_str(),
            kind = ?notification.kind,
            "notification: {}",
            notification.message
        );
    }
}

/// The functions a plugin may call. Every call is attributed to one plugin
/// and gated by the permissions it declared.
///
/// Runtimes pass `&mut dyn PluginHost` to plugins during activation (so they
/// can [`register_command`](PluginHost::register_command)) and `&dyn
/// PluginHost` to command handlers.
pub trait PluginHost {
    /// The plugin this host acts for.
    fn plugin_id(&self) -> &PluginId;

    /// Permissions granted to the plugin.
    fn permissions(&self) -> &BTreeSet<Permission>;

    /// Returns `Ok(())` if `permission` is granted, otherwise
    /// [`HostError::PermissionDenied`]. New gated host functions should start
    /// with this check.
    fn require(&self, permission: Permission) -> Result<(), HostError> {
        if self.permissions().contains(&permission) {
            Ok(())
        } else {
            Err(HostError::PermissionDenied {
                plugin: self.plugin_id().clone(),
                permission,
            })
        }
    }

    /// Writes to the IDE log. Always allowed.
    fn log(&self, level: LogLevel, message: &str);

    /// Shows a notification. Always allowed.
    fn show_notification(&self, notification: Notification);

    /// Registers the handler for a command declared in the plugin's
    /// `contributes.commands`.
    fn register_command(
        &mut self,
        command_id: &str,
        handler: CommandHandler,
    ) -> Result<(), HostError>;

    /// Reads a file. Requires [`Permission::FsRead`].
    fn read_file(&self, path: &Path) -> Result<Vec<u8>, HostError>;
}

/// The open project folder, shared between the
/// [`PluginManager`](crate::PluginManager) and every [`ScopedHost`] so file
/// access follows the project as it is opened and closed.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceRoot(Arc<RwLock<Option<PathBuf>>>);

impl WorkspaceRoot {
    /// A root with no project open.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets (or clears) the open project folder.
    pub fn set(&self, root: Option<PathBuf>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = root;
    }

    /// The open project folder.
    pub fn get(&self) -> Option<PathBuf> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The standard [`PluginHost`]: scoped to one plugin, enforcing its manifest.
///
/// It grants the permissions the manifest declares, narrowed with
/// [`with_permissions`](Self::with_permissions) to what the user approved.
/// File access is limited to the folders set with
/// [`with_fs_scope`](Self::with_fs_scope) (none by default).
pub struct ScopedHost {
    plugin: PluginId,
    permissions: BTreeSet<Permission>,
    declared_commands: Vec<CommandContribution>,
    handlers: BTreeMap<String, CommandHandler>,
    backend: Arc<dyn HostBackend>,
    plugin_root: Option<PathBuf>,
    workspace_root: WorkspaceRoot,
}

impl fmt::Debug for ScopedHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopedHost")
            .field("plugin", &self.plugin)
            .field("permissions", &self.permissions)
            .field("handlers", &self.handlers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl ScopedHost {
    /// Creates a host for the plugin described by `manifest`, granting the
    /// permissions it declares.
    pub fn new(manifest: &PluginManifest, backend: Arc<dyn HostBackend>) -> Self {
        Self {
            plugin: manifest.id().clone(),
            permissions: manifest.permissions().clone(),
            declared_commands: manifest.contributes().commands.clone(),
            handlers: BTreeMap::new(),
            backend,
            plugin_root: None,
            workspace_root: WorkspaceRoot::new(),
        }
    }

    /// Lets file access reach `plugin_root` (the plugin's own folder) and the
    /// project in `workspace_root`. Relative paths resolve against the
    /// project, never the IDE's working directory.
    pub fn with_fs_scope(
        mut self,
        plugin_root: Option<PathBuf>,
        workspace_root: WorkspaceRoot,
    ) -> Self {
        self.plugin_root = plugin_root;
        self.workspace_root = workspace_root;
        self
    }

    /// Resolves `path` and checks it lies inside an allowed folder.
    fn resolve_in_scope(&self, path: &Path) -> Result<PathBuf, HostError> {
        let outside = |path: PathBuf| HostError::OutsideScope {
            plugin: self.plugin.clone(),
            path,
        };
        let workspace = self.workspace_root.get();
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            match &workspace {
                Some(root) => root.join(path),
                None => return Err(outside(path.to_path_buf())),
            }
        };
        let roots: Vec<&Path> = [self.plugin_root.as_deref(), workspace.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        match std::fs::canonicalize(&full) {
            // Symlinks and `..` are resolved, so the check cannot be escaped.
            Ok(real) => {
                let inside = roots
                    .iter()
                    .filter_map(|root| std::fs::canonicalize(root).ok())
                    .any(|root| real.starts_with(root));
                if inside {
                    Ok(real)
                } else {
                    Err(outside(full))
                }
            }
            // Missing file: report it only if it would be inside a root.
            Err(source) => {
                let plain = !full.components().any(|c| c == Component::ParentDir);
                if plain && roots.iter().any(|root| full.starts_with(root)) {
                    Err(HostError::Io { path: full, source })
                } else {
                    Err(outside(full))
                }
            }
        }
    }

    /// Restricts granted permissions to `granted ∩ declared`. A plugin can
    /// never gain a permission its manifest does not request.
    pub fn with_permissions(mut self, granted: &BTreeSet<Permission>) -> Self {
        self.permissions = self.permissions.intersection(granted).copied().collect();
        self
    }

    /// The handler registered for `command_id`, if any.
    pub fn handler(&self, command_id: &str) -> Option<&CommandHandler> {
        self.handlers.get(command_id)
    }

    /// Ids of commands with a registered handler, sorted.
    pub fn registered_commands(&self) -> impl Iterator<Item = &str> {
        self.handlers.keys().map(String::as_str)
    }
}

impl PluginHost for ScopedHost {
    fn plugin_id(&self) -> &PluginId {
        &self.plugin
    }

    fn permissions(&self) -> &BTreeSet<Permission> {
        &self.permissions
    }

    fn log(&self, level: LogLevel, message: &str) {
        self.backend.log(&self.plugin, level, message);
    }

    fn show_notification(&self, notification: Notification) {
        self.backend.show_notification(&self.plugin, &notification);
    }

    fn register_command(
        &mut self,
        command_id: &str,
        handler: CommandHandler,
    ) -> Result<(), HostError> {
        if !self.declared_commands.iter().any(|c| c.id == command_id) {
            return Err(HostError::UndeclaredCommand {
                plugin: self.plugin.clone(),
                command: command_id.to_owned(),
            });
        }
        if self.handlers.contains_key(command_id) {
            return Err(HostError::CommandAlreadyRegistered(command_id.to_owned()));
        }
        self.handlers.insert(command_id.to_owned(), handler);
        Ok(())
    }

    fn read_file(&self, path: &Path) -> Result<Vec<u8>, HostError> {
        self.require(Permission::FsRead)?;
        let path = self.resolve_in_scope(path)?;
        self.backend
            .read_file(&path)
            .map_err(|source| HostError::Io { path, source })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    use super::*;

    /// Backend that records everything, for assertions.
    #[derive(Default)]
    pub(crate) struct RecordingBackend {
        pub logs: Mutex<Vec<(PluginId, LogLevel, String)>>,
        pub notifications: Mutex<Vec<(PluginId, Notification)>>,
    }

    impl RecordingBackend {
        pub(crate) fn notifications(&self) -> Vec<(PluginId, Notification)> {
            self.notifications.lock().unwrap().clone()
        }

        pub(crate) fn logs(&self) -> Vec<(PluginId, LogLevel, String)> {
            self.logs.lock().unwrap().clone()
        }
    }

    impl HostBackend for RecordingBackend {
        fn log(&self, plugin: &PluginId, level: LogLevel, message: &str) {
            self.logs
                .lock()
                .unwrap()
                .push((plugin.clone(), level, message.to_owned()));
        }

        fn show_notification(&self, plugin: &PluginId, notification: &Notification) {
            self.notifications
                .lock()
                .unwrap()
                .push((plugin.clone(), notification.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::RecordingBackend;
    use super::*;

    fn manifest(permissions: &str) -> PluginManifest {
        PluginManifest::from_toml_str(&format!(
            r#"
            id = "test.host"
            name = "Host test"
            version = "0.1.0"
            apex_api = "^0.1"
            permissions = [{permissions}]
            [[contributes.commands]]
            id = "test.host.run"
            title = "Run"
            "#
        ))
        .unwrap()
    }

    fn noop() -> CommandHandler {
        command_handler(|_, _| Ok(()))
    }

    #[test]
    fn read_file_requires_fs_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.txt");
        std::fs::write(&file, b"hello").unwrap();
        let backend = Arc::new(RecordingBackend::default());
        let project = WorkspaceRoot::new();
        project.set(Some(dir.path().to_path_buf()));
        let scoped = |permissions: &str| {
            ScopedHost::new(&manifest(permissions), backend.clone())
                .with_fs_scope(None, project.clone())
        };

        let denied = scoped("");
        assert!(matches!(
            denied.read_file(&file),
            Err(HostError::PermissionDenied {
                permission: Permission::FsRead,
                ..
            })
        ));

        // Declaring an unrelated permission does not help.
        assert!(scoped("\"network\"").read_file(&file).is_err());

        let allowed = scoped("\"fs-read\"");
        assert_eq!(allowed.read_file(&file).unwrap(), b"hello");
        assert!(matches!(
            allowed.read_file(&dir.path().join("missing.txt")),
            Err(HostError::Io { .. })
        ));
    }

    #[test]
    fn read_file_is_confined_to_the_plugin_and_project_folders() {
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("id_rsa");
        std::fs::write(&secret, b"KEY").unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        std::fs::write(project_dir.path().join("main.py"), b"print()").unwrap();
        let plugin_dir = tempfile::tempdir().unwrap();
        std::fs::write(plugin_dir.path().join("data.txt"), b"mine").unwrap();

        let project = WorkspaceRoot::new();
        let host = ScopedHost::new(
            &manifest("\"fs-read\""),
            Arc::new(RecordingBackend::default()),
        )
        .with_fs_scope(Some(plugin_dir.path().to_path_buf()), project.clone());

        let is_outside =
            |r: Result<Vec<u8>, HostError>| matches!(r, Err(HostError::OutsideScope { .. }));
        // Absolute paths elsewhere, `..` escapes and missing files outside.
        assert!(is_outside(host.read_file(&secret)));
        let escape = plugin_dir
            .path()
            .join("..")
            .join(outside.path().file_name().unwrap())
            .join("id_rsa");
        assert!(is_outside(host.read_file(&escape)));
        assert!(is_outside(host.read_file(&outside.path().join("nope"))));
        // Relative paths need a project; they never use the IDE's CWD.
        assert!(is_outside(host.read_file(Path::new("main.py"))));

        assert_eq!(
            host.read_file(&plugin_dir.path().join("data.txt")).unwrap(),
            b"mine"
        );
        project.set(Some(project_dir.path().to_path_buf()));
        assert_eq!(host.read_file(Path::new("main.py")).unwrap(), b"print()");
        assert!(is_outside(host.read_file(Path::new("../x"))));
        project.set(None);
        assert!(is_outside(host.read_file(Path::new("main.py"))));
    }

    #[test]
    fn with_permissions_only_narrows() {
        let backend = Arc::new(RecordingBackend::default());
        let granted: BTreeSet<_> = [Permission::Network, Permission::Process]
            .into_iter()
            .collect();
        let host = ScopedHost::new(&manifest("\"fs-read\", \"network\""), backend)
            .with_permissions(&granted);
        assert_eq!(
            host.permissions().iter().copied().collect::<Vec<_>>(),
            vec![Permission::Network]
        );
        assert!(host.require(Permission::FsRead).is_err());
        assert!(host.require(Permission::Process).is_err());
    }

    #[test]
    fn register_command_enforces_manifest() {
        let backend = Arc::new(RecordingBackend::default());
        let mut host = ScopedHost::new(&manifest(""), backend);
        assert!(matches!(
            host.register_command("test.host.other", noop()),
            Err(HostError::UndeclaredCommand { .. })
        ));
        host.register_command("test.host.run", noop()).unwrap();
        assert!(matches!(
            host.register_command("test.host.run", noop()),
            Err(HostError::CommandAlreadyRegistered(_))
        ));
        assert_eq!(
            host.registered_commands().collect::<Vec<_>>(),
            vec!["test.host.run"]
        );
        assert!(host.handler("test.host.run").is_some());
    }

    #[test]
    fn calls_are_attributed_to_the_plugin() {
        let backend = Arc::new(RecordingBackend::default());
        let host = ScopedHost::new(&manifest(""), backend.clone());
        host.log(LogLevel::Warn, "careful");
        host.show_notification(Notification::warning("heads up"));
        assert_eq!(backend.logs()[0].0.as_str(), "test.host");
        assert_eq!(backend.logs()[0].1, LogLevel::Warn);
        assert_eq!(
            backend.notifications()[0].1,
            Notification::warning("heads up")
        );
    }
}
