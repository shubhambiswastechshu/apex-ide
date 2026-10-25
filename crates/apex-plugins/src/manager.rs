//! [`PluginManager`]: registry, lifecycle, contributions and command dispatch.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::builtin::builtin_plugins;
use crate::descriptor::{PluginDescriptor, PluginSource};
use crate::discovery::{discover_plugins, PluginIssue};
use crate::error::PluginError;
use crate::host::{HostBackend, ScopedHost, WorkspaceRoot};
use crate::id::PluginId;
use crate::manifest::Permission;
use crate::runtime::{BuiltinPlugin, LoadedPlugin, NativeBuiltinRuntime, PluginRuntime};
use crate::state::{PluginState, StateError, STATE_FILE_NAME};

/// Suffix of the backup made of an unreadable `plugins.toml`.
pub const CORRUPT_STATE_SUFFIX: &str = ".corrupt";

/// Lifecycle status of a registered plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginStatus {
    /// Registered but not running (never activated, or deactivated).
    Inactive,
    /// Running; its contributions are live.
    Active,
    /// The last activation attempt failed with this message.
    Failed(String),
}

/// Read-only view of one registered plugin.
#[derive(Debug, Clone, Copy)]
pub struct PluginInfo<'a> {
    /// Manifest and source.
    pub descriptor: &'a PluginDescriptor,
    /// Whether the user has the plugin enabled.
    pub enabled: bool,
    /// Lifecycle status.
    pub status: &'a PluginStatus,
}

/// Outcome of [`PluginManager::scan`].
#[derive(Debug, Default)]
pub struct ScanSummary {
    /// Plugins newly added to the registry by this scan.
    pub added: Vec<PluginId>,
    /// Plugins (or the state file) that had problems. They are skipped; the
    /// rest of the scan is unaffected.
    pub issues: Vec<PluginIssue>,
}

/// A command contributed by an active plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributedCommand {
    /// Owning plugin.
    pub plugin: PluginId,
    /// Command id.
    pub id: String,
    /// Palette title.
    pub title: String,
    /// Default keybinding, if any.
    pub keybinding: Option<String>,
    /// Whether the plugin registered a handler (otherwise executing it
    /// returns [`PluginError::CommandNotImplemented`]).
    pub has_handler: bool,
}

/// A file contributed by an active plugin (theme or snippet file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributedPath {
    /// Owning plugin.
    pub plugin: PluginId,
    /// Absolute path for folder plugins; the manifest path unchanged for
    /// built-ins (see [`PluginDescriptor::resolve`]).
    pub path: PathBuf,
}

/// A language contributed by an active plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributedLanguage {
    /// Owning plugin.
    pub plugin: PluginId,
    /// Language id.
    pub id: String,
    /// Lowercased extensions without dots.
    pub extensions: Vec<String>,
}

/// A database driver id contributed by an active plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributedDbDriver {
    /// Owning plugin.
    pub plugin: PluginId,
    /// Driver id.
    pub id: String,
}

/// All contributions of all active plugins, ordered by plugin id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContributionSet {
    /// Commands.
    pub commands: Vec<ContributedCommand>,
    /// Theme files.
    pub themes: Vec<ContributedPath>,
    /// Languages.
    pub languages: Vec<ContributedLanguage>,
    /// Snippet files.
    pub snippets: Vec<ContributedPath>,
    /// Database driver ids.
    pub db_drivers: Vec<ContributedDbDriver>,
}

struct Entry {
    descriptor: PluginDescriptor,
    status: PluginStatus,
}

struct ActivePlugin {
    host: ScopedHost,
    loaded: Option<Box<dyn LoadedPlugin>>,
}

/// Owns every plugin: registry, enable/disable state, activation, aggregated
/// contributions and the command registry.
///
/// Typical start-up:
///
/// ```no_run
/// # fn main() -> Result<(), apex_plugins::PluginError> {
/// use std::sync::Arc;
/// use apex_plugins::{PluginManager, TracingBackend};
///
/// let mut plugins = PluginManager::with_builtins(Arc::new(TracingBackend))?;
/// let summary = plugins.scan("/path/to/plugins".as_ref())?;
/// for issue in &summary.issues {
///     eprintln!("{}: {}", issue.path.display(), issue.error);
/// }
/// for (id, err) in plugins.activate_enabled() {
///     eprintln!("{id}: {err}");
/// }
/// let palette = plugins.contributions().commands;
/// plugins.execute_command("apex.hello.sayHello", &[])?;
/// # let _ = palette; Ok(()) }
/// ```
pub struct PluginManager {
    backend: Arc<dyn HostBackend>,
    state: PluginState,
    state_path: Option<PathBuf>,
    plugins: BTreeMap<PluginId, Entry>,
    builtins: NativeBuiltinRuntime,
    runtimes: Vec<Box<dyn PluginRuntime>>,
    active: BTreeMap<PluginId, ActivePlugin>,
    commands: BTreeMap<String, PluginId>,
    workspace_root: WorkspaceRoot,
}

impl std::fmt::Debug for PluginManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginManager")
            .field("plugins", &self.plugins.keys().collect::<Vec<_>>())
            .field("active", &self.active.keys().collect::<Vec<_>>())
            .field("state_path", &self.state_path)
            .finish_non_exhaustive()
    }
}

impl PluginManager {
    /// Creates an empty manager. Host calls from plugins go to `backend`.
    pub fn new(backend: Arc<dyn HostBackend>) -> Self {
        Self {
            backend,
            state: PluginState::default(),
            state_path: None,
            plugins: BTreeMap::new(),
            builtins: NativeBuiltinRuntime::new(),
            runtimes: Vec::new(),
            active: BTreeMap::new(),
            commands: BTreeMap::new(),
            workspace_root: WorkspaceRoot::new(),
        }
    }

    /// Sets the open project folder. Plugins with a granted `fs-read` may
    /// read inside it (and inside their own folder), nowhere else.
    pub fn set_workspace_root(&self, root: Option<PathBuf>) {
        self.workspace_root.set(root);
    }

    /// Creates a manager with every plugin from
    /// [`builtin_plugins`](crate::builtin_plugins) registered.
    pub fn with_builtins(backend: Arc<dyn HostBackend>) -> Result<Self, PluginError> {
        let mut manager = Self::new(backend);
        for plugin in builtin_plugins() {
            manager.register_builtin(plugin)?;
        }
        Ok(manager)
    }

    /// Registers an in-process Rust plugin. Its manifest is validated like
    /// any other.
    pub fn register_builtin(
        &mut self,
        plugin: Arc<dyn BuiltinPlugin>,
    ) -> Result<PluginId, PluginError> {
        let manifest = plugin
            .manifest()
            .map_err(|source| PluginError::InvalidManifest {
                origin: PluginSource::Builtin.to_string(),
                source,
            })?;
        if let Some(existing) = self.plugins.get(manifest.id()) {
            return Err(PluginError::DuplicatePlugin {
                id: manifest.id().clone(),
                existing: existing.descriptor.source.to_string(),
            });
        }
        let registered = self.builtins.register(plugin)?;
        let id = registered.id().clone();
        self.plugins.insert(
            id.clone(),
            Entry {
                descriptor: PluginDescriptor {
                    manifest: registered,
                    source: PluginSource::Builtin,
                },
                status: PluginStatus::Inactive,
            },
        );
        Ok(id)
    }

    /// Adds a runtime for plugins with an `entry` (e.g. a future WASM
    /// runtime). Runtimes are tried in the order they were added.
    pub fn add_runtime(&mut self, runtime: Box<dyn PluginRuntime>) {
        self.runtimes.push(runtime);
    }

    /// Loads `<dir>/plugins.toml` and registers the plugins found in `dir`.
    ///
    /// From now on, [`set_enabled`](Self::set_enabled) persists to that
    /// file. A corrupt state file is reported in [`ScanSummary::issues`],
    /// backed up as `plugins.toml.corrupt`, and handled fail-closed: built-in
    /// plugins stay enabled, folder plugins are disabled until the user
    /// enables them again. Re-scanning registers newly added folders and
    /// leaves already-known plugins untouched. Nothing is activated.
    ///
    /// Fails only if `dir` exists but cannot be listed.
    pub fn scan(&mut self, dir: &Path) -> Result<ScanSummary, PluginError> {
        let report = discover_plugins(dir)?;
        let mut summary = ScanSummary::default();

        let state_path = dir.join(STATE_FILE_NAME);
        let mut fail_closed = false;
        match PluginState::load(&state_path) {
            Ok(state) => self.state = state,
            Err(err) => {
                if matches!(err, StateError::Parse { .. }) {
                    fail_closed = true;
                    self.state = PluginState::default();
                    let mut backup = state_path.as_os_str().to_owned();
                    backup.push(CORRUPT_STATE_SUFFIX);
                    if let Err(e) = std::fs::rename(&state_path, PathBuf::from(backup)) {
                        tracing::warn!(%e, "backing up corrupt plugin state");
                    }
                }
                summary.issues.push(PluginIssue {
                    path: state_path.clone(),
                    error: err.into(),
                });
            }
        }
        self.state_path = Some(state_path);
        summary.issues.extend(report.issues);

        for descriptor in report.plugins {
            if let Some(existing) = self.plugins.get(descriptor.id()) {
                if existing.descriptor.source != descriptor.source {
                    summary.issues.push(PluginIssue {
                        path: descriptor.root().map(Path::to_path_buf).unwrap_or_default(),
                        error: PluginError::DuplicatePlugin {
                            id: descriptor.id().clone(),
                            existing: existing.descriptor.source.to_string(),
                        },
                    });
                }
                continue;
            }
            let id = descriptor.id().clone();
            self.plugins.insert(
                id.clone(),
                Entry {
                    descriptor,
                    status: PluginStatus::Inactive,
                },
            );
            summary.added.push(id);
        }
        if fail_closed {
            // Without the user's saved choices, never run folder code they
            // may have disabled.
            for (id, entry) in &self.plugins {
                if entry.descriptor.source != PluginSource::Builtin {
                    self.state.set_enabled(id, false);
                }
            }
        }
        Ok(summary)
    }

    /// Every registered plugin, ordered by id.
    pub fn plugins(&self) -> impl Iterator<Item = PluginInfo<'_>> {
        self.plugins.iter().map(|(id, entry)| self.info(id, entry))
    }

    /// One registered plugin.
    pub fn plugin(&self, id: &PluginId) -> Option<PluginInfo<'_>> {
        self.plugins.get(id).map(|entry| self.info(id, entry))
    }

    fn info<'a>(&self, id: &PluginId, entry: &'a Entry) -> PluginInfo<'a> {
        PluginInfo {
            descriptor: &entry.descriptor,
            enabled: self.state.is_enabled(id),
            status: &entry.status,
        }
    }

    /// Whether `id` is enabled (unknown ids count as enabled).
    pub fn is_enabled(&self, id: &PluginId) -> bool {
        self.state.is_enabled(id)
    }

    /// Whether `id` is currently active.
    pub fn is_active(&self, id: &PluginId) -> bool {
        self.active.contains_key(id)
    }

    /// The permissions `id` actually has: everything its manifest declares
    /// for built-in plugins (they ship with the IDE), and only what the user
    /// granted (within the declared set) for folder plugins.
    pub fn granted_permissions(&self, id: &PluginId) -> BTreeSet<Permission> {
        let Some(entry) = self.plugins.get(id) else {
            return BTreeSet::new();
        };
        let declared = entry.descriptor.manifest.permissions();
        if entry.descriptor.source == PluginSource::Builtin {
            return declared.clone();
        }
        declared
            .intersection(&self.state.granted(id))
            .copied()
            .collect()
    }

    /// Declared permissions the user has not granted yet.
    pub fn missing_permissions(&self, id: &PluginId) -> BTreeSet<Permission> {
        let Some(entry) = self.plugins.get(id) else {
            return BTreeSet::new();
        };
        let granted = self.granted_permissions(id);
        entry
            .descriptor
            .manifest
            .permissions()
            .difference(&granted)
            .copied()
            .collect()
    }

    /// Grants `permissions` (limited to what the manifest declares) to a
    /// folder plugin, persists the choice and, if the plugin is active,
    /// restarts it so the new grant takes effect. Built-in plugins already
    /// have their declared permissions.
    pub fn grant_permissions(
        &mut self,
        id: &PluginId,
        permissions: &BTreeSet<Permission>,
    ) -> Result<(), PluginError> {
        let entry = self
            .plugins
            .get(id)
            .ok_or_else(|| PluginError::UnknownPlugin(id.clone()))?;
        if entry.descriptor.source == PluginSource::Builtin {
            return Ok(());
        }
        let granted: BTreeSet<Permission> = entry
            .descriptor
            .manifest
            .permissions()
            .intersection(permissions)
            .copied()
            .collect();
        let mut next = self.state.clone();
        if !next.set_granted(id, granted) {
            return Ok(());
        }
        if let Some(path) = &self.state_path {
            next.save(path)?;
        }
        self.state = next;
        if self.is_active(id) {
            self.deactivate(id)?;
            self.activate(id)?;
        }
        Ok(())
    }

    /// Enables or disables a plugin, persists the choice (after a
    /// [`scan`](Self::scan)), and activates or deactivates it to match.
    ///
    /// The state is only changed in memory if persisting succeeds. An
    /// activation failure is returned, but the plugin stays enabled.
    pub fn set_enabled(&mut self, id: &PluginId, enabled: bool) -> Result<(), PluginError> {
        if !self.plugins.contains_key(id) {
            return Err(PluginError::UnknownPlugin(id.clone()));
        }
        let mut next = self.state.clone();
        if next.set_enabled(id, enabled) {
            if let Some(path) = &self.state_path {
                next.save(path)?;
            }
            self.state = next;
        }
        if enabled {
            self.activate(id)
        } else {
            self.deactivate(id)
        }
    }

    /// Activates every enabled, inactive plugin: built-ins first, then folder
    /// plugins, each in id order (earlier plugins win command-id conflicts).
    /// Returns the plugins that failed; their status becomes
    /// [`PluginStatus::Failed`].
    pub fn activate_enabled(&mut self) -> Vec<(PluginId, PluginError)> {
        let mut order: Vec<(bool, PluginId)> = self
            .plugins
            .iter()
            .filter(|(id, _)| self.state.is_enabled(id) && !self.active.contains_key(*id))
            .map(|(id, e)| (e.descriptor.source != PluginSource::Builtin, id.clone()))
            .collect();
        order.sort();
        order
            .into_iter()
            .filter_map(|(_, id)| self.activate(&id).err().map(|e| (id, e)))
            .collect()
    }

    /// Activates one plugin. A no-op if it is already active.
    pub fn activate(&mut self, id: &PluginId) -> Result<(), PluginError> {
        let entry = self
            .plugins
            .get(id)
            .ok_or_else(|| PluginError::UnknownPlugin(id.clone()))?;
        if self.active.contains_key(id) {
            return Ok(());
        }
        if !self.state.is_enabled(id) {
            return Err(PluginError::Disabled(id.clone()));
        }
        let descriptor = entry.descriptor.clone();
        let result = self.try_activate(&descriptor);
        if let Some(entry) = self.plugins.get_mut(id) {
            entry.status = match &result {
                Ok(()) => PluginStatus::Active,
                Err(err) => PluginStatus::Failed(err.to_string()),
            };
        }
        if let Err(err) = &result {
            tracing::warn!(plugin = id.as_str(), "plugin activation failed: {err}");
        }
        result
    }

    fn try_activate(&mut self, descriptor: &PluginDescriptor) -> Result<(), PluginError> {
        let id = descriptor.id();
        let declared = &descriptor.manifest.contributes().commands;
        if let Some((command, owner)) = declared
            .iter()
            .find_map(|c| self.commands.get(&c.id).map(|owner| (&c.id, owner)))
        {
            return Err(PluginError::CommandConflict {
                command: command.clone(),
                owner: owner.clone(),
                plugin: id.clone(),
            });
        }

        let mut host = ScopedHost::new(&descriptor.manifest, Arc::clone(&self.backend))
            .with_fs_scope(
                descriptor.root().map(Path::to_path_buf),
                self.workspace_root.clone(),
            )
            .with_permissions(&self.granted_permissions(id));
        let loaded = if self.builtins.can_load(descriptor) {
            Some(self.builtins.load(descriptor, &mut host))
        } else if descriptor.manifest.entry().is_none() {
            None // Declarative plugin: contributions only.
        } else {
            let runtime = self
                .runtimes
                .iter_mut()
                .find(|r| r.can_load(descriptor))
                .ok_or_else(|| PluginError::NoRuntime(id.clone()))?;
            Some(runtime.load(descriptor, &mut host))
        };
        let loaded = loaded.transpose().map_err(|source| PluginError::Runtime {
            plugin: id.clone(),
            source,
        })?;

        for command in declared {
            self.commands.insert(command.id.clone(), id.clone());
        }
        self.active
            .insert(id.clone(), ActivePlugin { host, loaded });
        Ok(())
    }

    /// Deactivates one plugin and removes its commands. A no-op if it is not
    /// active. The plugin's own `deactivate` error, if any, is returned after
    /// clean-up has happened.
    pub fn deactivate(&mut self, id: &PluginId) -> Result<(), PluginError> {
        let entry = self
            .plugins
            .get_mut(id)
            .ok_or_else(|| PluginError::UnknownPlugin(id.clone()))?;
        entry.status = PluginStatus::Inactive;
        let Some(mut active) = self.active.remove(id) else {
            return Ok(());
        };
        self.commands.retain(|_, owner| owner != id);
        match active.loaded.as_mut() {
            Some(loaded) => {
                loaded
                    .deactivate(&mut active.host)
                    .map_err(|source| PluginError::Runtime {
                        plugin: id.clone(),
                        source,
                    })
            }
            None => Ok(()),
        }
    }

    /// Deactivates every active plugin (for shutdown). Returns failures.
    pub fn deactivate_all(&mut self) -> Vec<(PluginId, PluginError)> {
        let ids: Vec<PluginId> = self.active.keys().cloned().collect();
        ids.into_iter()
            .filter_map(|id| self.deactivate(&id).err().map(|e| (id, e)))
            .collect()
    }

    /// Contributions of all active plugins.
    pub fn contributions(&self) -> ContributionSet {
        let mut set = ContributionSet::default();
        for (id, active) in &self.active {
            let Some(entry) = self.plugins.get(id) else {
                continue;
            };
            let descriptor = &entry.descriptor;
            let contributes = descriptor.manifest.contributes();
            set.commands
                .extend(contributes.commands.iter().map(|c| ContributedCommand {
                    plugin: id.clone(),
                    id: c.id.clone(),
                    title: c.title.clone(),
                    keybinding: c.keybinding.clone(),
                    has_handler: active.host.handler(&c.id).is_some(),
                }));
            let paths = |list: &[PathBuf]| {
                list.iter()
                    .map(|p| ContributedPath {
                        plugin: id.clone(),
                        path: descriptor.resolve(p),
                    })
                    .collect::<Vec<_>>()
            };
            set.themes.extend(paths(&contributes.themes));
            set.snippets.extend(paths(&contributes.snippets));
            set.languages
                .extend(contributes.languages.iter().map(|l| ContributedLanguage {
                    plugin: id.clone(),
                    id: l.id.clone(),
                    extensions: l.extensions.clone(),
                }));
            set.db_drivers
                .extend(contributes.db_drivers.iter().map(|d| ContributedDbDriver {
                    plugin: id.clone(),
                    id: d.clone(),
                }));
        }
        set
    }

    /// The active command with `id`, if any.
    pub fn command(&self, id: &str) -> Option<ContributedCommand> {
        let owner = self.commands.get(id)?;
        let active = self.active.get(owner)?;
        let entry = self.plugins.get(owner)?;
        let declared = entry.descriptor.manifest.contributes().command(id)?;
        Some(ContributedCommand {
            plugin: owner.clone(),
            id: declared.id.clone(),
            title: declared.title.clone(),
            keybinding: declared.keybinding.clone(),
            has_handler: active.host.handler(id).is_some(),
        })
    }

    /// Runs command `id` with `args`, dispatching to the owning plugin's
    /// handler with that plugin's (permission-gated) host.
    pub fn execute_command(&self, id: &str, args: &[String]) -> Result<(), PluginError> {
        let unknown = || PluginError::UnknownCommand(id.to_owned());
        let owner = self.commands.get(id).ok_or_else(unknown)?;
        let active = self.active.get(owner).ok_or_else(unknown)?;
        let handler =
            active
                .host
                .handler(id)
                .ok_or_else(|| PluginError::CommandNotImplemented {
                    command: id.to_owned(),
                    plugin: owner.clone(),
                })?;
        handler(&active.host, args).map_err(|source| PluginError::CommandFailed {
            command: id.to_owned(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::builtin::HelloPlugin;
    use crate::host::test_support::RecordingBackend;
    use crate::host::{command_handler, CommandError, HostError, Notification, PluginHost};
    use crate::manifest::{ManifestError, PluginManifest, MANIFEST_FILE_NAME};
    use crate::runtime::RuntimeError;

    fn id(s: &str) -> PluginId {
        PluginId::new(s).unwrap()
    }

    fn manager() -> (PluginManager, Arc<RecordingBackend>) {
        let backend = Arc::new(RecordingBackend::default());
        let manager = PluginManager::with_builtins(backend.clone()).unwrap();
        (manager, backend)
    }

    fn write_plugin(root: &Path, folder: &str, body: &str) -> PathBuf {
        let dir = root.join(folder);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MANIFEST_FILE_NAME), body).unwrap();
        dir
    }

    fn header(id: &str) -> String {
        format!("id = \"{id}\"\nname = \"{id}\"\nversion = \"0.1.0\"\napex_api = \"^0.1\"\n")
    }

    /// Test runtime standing in for the future WASM runtime.
    struct FakeWasmRuntime {
        loads: Arc<AtomicUsize>,
        fail: bool,
    }

    struct FakeLoaded;
    impl LoadedPlugin for FakeLoaded {}

    impl PluginRuntime for FakeWasmRuntime {
        fn name(&self) -> &str {
            "fake-wasm"
        }

        fn can_load(&self, plugin: &PluginDescriptor) -> bool {
            plugin
                .entry_path()
                .is_some_and(|p| p.extension().is_some_and(|e| e == "wasm"))
        }

        fn load(
            &mut self,
            plugin: &PluginDescriptor,
            host: &mut dyn PluginHost,
        ) -> Result<Box<dyn LoadedPlugin>, RuntimeError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(RuntimeError::failed("guest trapped"));
            }
            let target = plugin.resolve(Path::new("data.txt"));
            host.register_command(
                "fake.read",
                command_handler(move |host, _| {
                    let bytes = host.read_file(&target)?;
                    host.show_notification(Notification::info(String::from_utf8_lossy(&bytes)));
                    Ok(())
                }),
            )?;
            Ok(Box::new(FakeLoaded))
        }
    }

    fn fake_plugin(root: &Path, plugin_id: &str, permissions: &str) -> PathBuf {
        let body = format!(
            "{}entry = \"plugin.wasm\"\npermissions = [{permissions}]\n[[contributes.commands]]\nid = \"fake.read\"\ntitle = \"Fake: Read\"\n",
            header(plugin_id)
        );
        let dir = write_plugin(root, plugin_id, &body);
        fs::write(dir.join("data.txt"), "secret").unwrap();
        dir
    }

    #[test]
    fn hello_builtin_activates_and_dispatches() {
        let (mut m, backend) = manager();
        assert!(m.activate_enabled().is_empty());
        assert!(m.is_active(&id(HelloPlugin::ID)));

        m.execute_command(HelloPlugin::SAY_HELLO, &[]).unwrap();
        m.execute_command(HelloPlugin::SAY_HELLO, &["Ada".to_owned()])
            .unwrap();
        let shown: Vec<_> = backend
            .notifications()
            .into_iter()
            .map(|(_, n)| n.message)
            .collect();
        assert_eq!(shown, vec!["Hello from Apex IDE!", "Hello, Ada!"]);
        assert!(backend
            .notifications()
            .iter()
            .all(|(p, _)| p.as_str() == HelloPlugin::ID));

        let cmd = m.command(HelloPlugin::SAY_HELLO).unwrap();
        assert!(cmd.has_handler);
        assert_eq!(cmd.keybinding.as_deref(), Some("Ctrl+Alt+H"));
        assert_eq!(m.contributions().commands, vec![cmd]);

        assert!(matches!(
            m.execute_command("nope.cmd", &[]),
            Err(PluginError::UnknownCommand(_))
        ));
    }

    #[test]
    fn duplicate_builtin_is_rejected() {
        let (mut m, _) = manager();
        assert!(matches!(
            m.register_builtin(Arc::new(HelloPlugin)),
            Err(PluginError::DuplicatePlugin { .. })
        ));
    }

    #[test]
    fn disable_persists_and_unregisters_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let hello = id(HelloPlugin::ID);
        {
            let (mut m, _) = manager();
            m.scan(tmp.path()).unwrap();
            assert!(m.activate_enabled().is_empty());
            m.set_enabled(&hello, false).unwrap();
            assert!(!m.is_active(&hello));
            assert!(matches!(
                m.execute_command(HelloPlugin::SAY_HELLO, &[]),
                Err(PluginError::UnknownCommand(_))
            ));
            assert!(m.contributions().commands.is_empty());
        }
        let saved = fs::read_to_string(tmp.path().join(STATE_FILE_NAME)).unwrap();
        assert!(saved.contains("apex.hello"), "{saved}");

        let (mut m, _) = manager();
        m.scan(tmp.path()).unwrap();
        assert!(!m.is_enabled(&hello));
        assert!(m.activate_enabled().is_empty());
        assert!(!m.is_active(&hello));
        assert!(matches!(m.activate(&hello), Err(PluginError::Disabled(_))));

        m.set_enabled(&hello, true).unwrap();
        assert!(m.is_active(&hello));
        m.execute_command(HelloPlugin::SAY_HELLO, &[]).unwrap();
        assert!(PluginState::load(&tmp.path().join(STATE_FILE_NAME))
            .unwrap()
            .is_enabled(&hello));

        assert!(matches!(
            m.set_enabled(&id("no.such"), false),
            Err(PluginError::UnknownPlugin(_))
        ));
    }

    #[test]
    fn scan_registers_folder_plugins_and_aggregates_contributions() {
        let tmp = tempfile::tempdir().unwrap();
        let body = format!(
            "{}[contributes]\nthemes = [\"themes/dark.json\"]\nsnippets = [\"snip/py.json\"]\ndb_drivers = [\"duckdb\"]\n[[contributes.languages]]\nid = \"jinja\"\nextensions = [\"j2\"]\n",
            header("com.example.decl")
        );
        let root = write_plugin(tmp.path(), "decl", &body);
        write_plugin(tmp.path(), "broken", "id = 1");

        let (mut m, _) = manager();
        let summary = m.scan(tmp.path()).unwrap();
        assert_eq!(summary.added, vec![id("com.example.decl")]);
        assert_eq!(summary.issues.len(), 1);
        assert!(summary.issues[0].path.ends_with("broken"));

        // Nothing is contributed before activation.
        assert!(m.contributions().themes.is_empty());
        assert!(m.activate_enabled().is_empty());

        let c = m.contributions();
        assert_eq!(
            c.themes,
            vec![ContributedPath {
                plugin: id("com.example.decl"),
                path: root.join("themes/dark.json"),
            }]
        );
        assert_eq!(c.snippets[0].path, root.join("snip/py.json"));
        assert_eq!(c.languages[0].extensions, vec!["j2".to_owned()]);
        assert_eq!(c.db_drivers[0].id, "duckdb");
        assert_eq!(c.commands.len(), 1, "only the hello command");

        let info = m.plugin(&id("com.example.decl")).unwrap();
        assert!(info.enabled);
        assert_eq!(info.status, &PluginStatus::Active);
        assert_eq!(m.plugins().count(), 2);

        // Re-scanning is idempotent.
        let again = m.scan(tmp.path()).unwrap();
        assert!(again.added.is_empty());
        assert_eq!(again.issues.len(), 1);
    }

    #[test]
    fn folder_plugin_cannot_shadow_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        write_plugin(tmp.path(), "fake-hello", &header(HelloPlugin::ID));
        let (mut m, _) = manager();
        let summary = m.scan(tmp.path()).unwrap();
        assert!(summary.added.is_empty());
        assert!(matches!(
            summary.issues[0].error,
            PluginError::DuplicatePlugin { .. }
        ));
        assert_eq!(
            m.plugin(&id(HelloPlugin::ID)).unwrap().descriptor.source,
            PluginSource::Builtin
        );
    }

    #[test]
    fn command_conflicts_fail_the_later_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let body = format!(
            "{}[[contributes.commands]]\nid = \"{}\"\ntitle = \"Steal\"\n",
            header("com.example.thief"),
            HelloPlugin::SAY_HELLO
        );
        write_plugin(tmp.path(), "thief", &body);
        let (mut m, _) = manager();
        m.scan(tmp.path()).unwrap();

        let failures = m.activate_enabled();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, id("com.example.thief"));
        assert!(
            matches!(failures[0].1, PluginError::CommandConflict { ref owner, .. } if owner.as_str() == HelloPlugin::ID)
        );
        assert!(matches!(
            m.plugin(&id("com.example.thief")).unwrap().status,
            PluginStatus::Failed(_)
        ));
        assert_eq!(
            m.command(HelloPlugin::SAY_HELLO).unwrap().plugin.as_str(),
            HelloPlugin::ID
        );
    }

    #[test]
    fn declared_command_without_handler() {
        let tmp = tempfile::tempdir().unwrap();
        let body = format!(
            "{}[[contributes.commands]]\nid = \"decl.only\"\ntitle = \"Declared only\"\n",
            header("com.example.declonly")
        );
        write_plugin(tmp.path(), "declonly", &body);
        let mut m = PluginManager::new(Arc::new(RecordingBackend::default()));
        m.scan(tmp.path()).unwrap();
        assert!(m.activate_enabled().is_empty());
        assert!(!m.command("decl.only").unwrap().has_handler);
        assert!(matches!(
            m.execute_command("decl.only", &[]),
            Err(PluginError::CommandNotImplemented { .. })
        ));
    }

    #[test]
    fn entry_plugins_need_a_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        fake_plugin(tmp.path(), "com.example.wasm", "\"fs-read\"");
        let mut m = PluginManager::new(Arc::new(RecordingBackend::default()));
        m.scan(tmp.path()).unwrap();
        let failures = m.activate_enabled();
        assert!(matches!(failures[..], [(_, PluginError::NoRuntime(_))]));
        assert!(m.contributions().commands.is_empty());
    }

    #[test]
    fn custom_runtime_dispatch_is_permission_gated() {
        let tmp = tempfile::tempdir().unwrap();
        fake_plugin(tmp.path(), "com.example.allowed", "\"fs-read\"");
        let backend = Arc::new(RecordingBackend::default());
        let loads = Arc::new(AtomicUsize::new(0));

        let mut m = PluginManager::new(backend.clone());
        m.add_runtime(Box::new(FakeWasmRuntime {
            loads: loads.clone(),
            fail: false,
        }));
        m.scan(tmp.path()).unwrap();
        assert!(m.activate_enabled().is_empty());
        let allowed = id("com.example.allowed");

        // Declared but not yet granted by the user: the read is refused.
        assert_eq!(
            m.missing_permissions(&allowed),
            [Permission::FsRead].into_iter().collect()
        );
        match m.execute_command("fake.read", &[]) {
            Err(PluginError::CommandFailed { source, .. }) => {
                assert!(source.message().contains("fs-read"), "{source}");
            }
            other => panic!("expected permission failure, got {other:?}"),
        }
        assert!(backend.notifications().is_empty());

        // After consent (persisted) the plugin is restarted with the grant.
        let fs_read: BTreeSet<Permission> = [Permission::FsRead].into_iter().collect();
        m.grant_permissions(&allowed, &fs_read).unwrap();
        assert!(m.missing_permissions(&allowed).is_empty());
        assert!(m.is_active(&allowed));
        m.execute_command("fake.read", &[]).unwrap();
        assert_eq!(backend.notifications()[0].1.message, "secret");
        assert_eq!(
            PluginState::load(&tmp.path().join(STATE_FILE_NAME))
                .unwrap()
                .granted(&allowed),
            fs_read
        );

        // Same plugin without fs-read: the handler's read is refused.
        let tmp2 = tempfile::tempdir().unwrap();
        fake_plugin(tmp2.path(), "com.example.denied", "\"network\"");
        let mut m2 = PluginManager::new(backend.clone());
        m2.add_runtime(Box::new(FakeWasmRuntime {
            loads: loads.clone(),
            fail: false,
        }));
        m2.scan(tmp2.path()).unwrap();
        // Granting an undeclared permission has no effect.
        m2.grant_permissions(&id("com.example.denied"), &fs_read)
            .unwrap();
        assert!(m2.granted_permissions(&id("com.example.denied")).is_empty());
        assert!(m2.activate_enabled().is_empty());
        match m2.execute_command("fake.read", &[]) {
            Err(PluginError::CommandFailed { source, .. }) => {
                assert!(source.message().contains("fs-read"), "{source}");
            }
            other => panic!("expected permission failure, got {other:?}"),
        }
        assert_eq!(backend.notifications().len(), 1);
        // allowed: activate + restart after the grant; denied: activate.
        assert_eq!(loads.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn runtime_failure_marks_plugin_failed() {
        let tmp = tempfile::tempdir().unwrap();
        fake_plugin(tmp.path(), "com.example.crashy", "");
        let mut m = PluginManager::new(Arc::new(RecordingBackend::default()));
        m.add_runtime(Box::new(FakeWasmRuntime {
            loads: Arc::new(AtomicUsize::new(0)),
            fail: true,
        }));
        m.scan(tmp.path()).unwrap();
        let failures = m.activate_enabled();
        assert!(matches!(failures[..], [(_, PluginError::Runtime { .. })]));
        assert!(m.command("fake.read").is_none());
        assert!(!m.is_active(&id("com.example.crashy")));
    }

    struct Counting {
        activations: AtomicUsize,
        deactivations: AtomicUsize,
    }

    impl BuiltinPlugin for Counting {
        fn manifest(&self) -> Result<PluginManifest, ManifestError> {
            PluginManifest::from_toml_str(&format!(
                "{}[[contributes.commands]]\nid = \"count.fail\"\ntitle = \"Fail\"\n",
                header("test.counting")
            ))
        }

        fn activate(&self, host: &mut dyn PluginHost) -> Result<(), RuntimeError> {
            self.activations.fetch_add(1, Ordering::SeqCst);
            host.register_command(
                "count.fail",
                command_handler(|_, _| Err(CommandError::new("boom"))),
            )?;
            // Undeclared commands are refused even for built-ins.
            let undeclared = host.register_command("count.sneaky", command_handler(|_, _| Ok(())));
            assert!(matches!(
                undeclared,
                Err(HostError::UndeclaredCommand { .. })
            ));
            Ok(())
        }

        fn deactivate(&self, _host: &mut dyn PluginHost) -> Result<(), RuntimeError> {
            self.deactivations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn builtin_lifecycle_and_handler_errors() {
        let plugin = Arc::new(Counting {
            activations: AtomicUsize::new(0),
            deactivations: AtomicUsize::new(0),
        });
        let mut m = PluginManager::new(Arc::new(RecordingBackend::default()));
        let pid = m.register_builtin(plugin.clone()).unwrap();

        m.activate(&pid).unwrap();
        m.activate(&pid).unwrap(); // no-op
        assert!(matches!(
            m.execute_command("count.fail", &[]),
            Err(PluginError::CommandFailed { ref source, .. }) if source.message() == "boom"
        ));
        assert!(m.deactivate_all().is_empty());
        m.deactivate(&pid).unwrap(); // no-op
        m.activate(&pid).unwrap();
        assert_eq!(plugin.activations.load(Ordering::SeqCst), 2);
        assert_eq!(plugin.deactivations.load(Ordering::SeqCst), 1);
        assert_eq!(m.plugin(&pid).unwrap().status, &PluginStatus::Active);
    }

    #[test]
    fn corrupt_state_file_fails_closed_and_is_backed_up() {
        let tmp = tempfile::tempdir().unwrap();
        let corrupt = "disabled = 42";
        fs::write(tmp.path().join(STATE_FILE_NAME), corrupt).unwrap();
        write_plugin(tmp.path(), "decl", &header("com.example.decl"));
        let (mut m, _) = manager();
        let summary = m.scan(tmp.path()).unwrap();
        assert!(matches!(
            summary.issues[..],
            [PluginIssue {
                error: PluginError::State(_),
                ..
            }]
        ));
        let backup = tmp
            .path()
            .join(format!("{STATE_FILE_NAME}{CORRUPT_STATE_SUFFIX}"));
        assert_eq!(fs::read_to_string(&backup).unwrap(), corrupt);

        // Built-ins still run; the folder plugin waits for the user.
        assert!(m.activate_enabled().is_empty());
        assert!(m.is_active(&id(HelloPlugin::ID)));
        assert!(!m.is_active(&id("com.example.decl")));
        assert!(!m.is_enabled(&id("com.example.decl")));

        // The next change writes a valid file; the backup is kept.
        m.set_enabled(&id(HelloPlugin::ID), false).unwrap();
        assert!(!PluginState::load(&tmp.path().join(STATE_FILE_NAME))
            .unwrap()
            .is_enabled(&id(HelloPlugin::ID)));
        assert!(backup.is_file());
    }

    #[test]
    fn unknown_state_keys_neither_reenable_plugins_nor_get_lost() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(STATE_FILE_NAME),
            "disabled = [\"com.example.noisy\"]\nnewer_setting = \"x\"\n",
        )
        .unwrap();
        write_plugin(tmp.path(), "noisy", &header("com.example.noisy"));
        let (mut m, _) = manager();
        assert!(m.scan(tmp.path()).unwrap().issues.is_empty());
        assert!(m.activate_enabled().is_empty());
        assert!(!m.is_active(&id("com.example.noisy")));

        m.set_enabled(&id(HelloPlugin::ID), false).unwrap();
        let saved = fs::read_to_string(tmp.path().join(STATE_FILE_NAME)).unwrap();
        assert!(saved.contains("newer_setting"), "{saved}");
        assert!(saved.contains("com.example.noisy"), "{saved}");
    }
}
