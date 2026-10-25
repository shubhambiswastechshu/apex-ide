//! Plugin start-up and the UI side of the plugin host.

use std::path::Path;
use std::sync::Arc;

use apex_plugins::{
    ContributedCommand, HostBackend, LogLevel, Notification, NotificationKind, PluginId,
    PluginManager, PluginSource, PluginStatus,
};
use apex_themes::{ThemeRegistry, ThemeSource};

use crate::events::{EventSink, NoticeKind};

/// Routes plugin notifications to the status bar and logs to `tracing`.
pub struct UiBackend {
    sink: EventSink,
}

impl UiBackend {
    /// A backend that posts to `sink`.
    pub fn new(sink: EventSink) -> Self {
        Self { sink }
    }
}

impl HostBackend for UiBackend {
    fn log(&self, plugin: &PluginId, level: LogLevel, message: &str) {
        match level {
            LogLevel::Error => tracing::error!(plugin = %plugin, "{message}"),
            LogLevel::Warn => tracing::warn!(plugin = %plugin, "{message}"),
            LogLevel::Info => tracing::info!(plugin = %plugin, "{message}"),
            _ => tracing::debug!(plugin = %plugin, "{message}"),
        }
    }

    fn show_notification(&self, plugin: &PluginId, notification: &Notification) {
        let kind = match notification.kind {
            NotificationKind::Error => NoticeKind::Error,
            NotificationKind::Warning => NoticeKind::Warning,
            _ => NoticeKind::Info,
        };
        self.sink
            .notice(kind, format!("{plugin}: {}", notification.message));
    }
}

/// Creates the manager with built-in plugins, scans `plugins_dir` (if
/// known) and activates everything enabled. Returns the manager and one
/// message per problem; problems never stop start-up.
pub fn start(sink: EventSink, plugins_dir: Option<&Path>) -> (Option<PluginManager>, Vec<String>) {
    let mut problems = Vec::new();
    let mut manager = match PluginManager::with_builtins(Arc::new(UiBackend::new(sink))) {
        Ok(m) => m,
        Err(e) => {
            problems.push(format!("plugins disabled: {e}"));
            return (None, problems);
        }
    };
    if let Some(dir) = plugins_dir {
        match manager.scan(dir) {
            Ok(summary) => problems.extend(
                summary
                    .issues
                    .iter()
                    .map(|i| format!("plugin at {}: {}", i.path.display(), i.error)),
            ),
            Err(e) => problems.push(format!("scanning {}: {e}", dir.display())),
        }
    }
    for (id, err) in manager.activate_enabled() {
        problems.push(format!("plugin {id} failed to activate: {err}"));
    }
    (Some(manager), problems)
}

/// A plugin as the Plugins panel shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginView {
    /// Plugin id.
    pub id: PluginId,
    /// Display name.
    pub name: String,
    /// Version text.
    pub version: String,
    /// Description (may be empty).
    pub description: String,
    /// User's enabled flag.
    pub enabled: bool,
    /// "Active", "Inactive" or "Failed: ...".
    pub status: String,
    /// Whether activation failed.
    pub failed: bool,
    /// What the plugin may do, e.g. `"Permissions: fs-read (not granted)"`;
    /// empty when it requests none.
    pub permissions: String,
    /// Whether some requested permission still awaits the user's consent.
    pub needs_grant: bool,
}

/// Every registered plugin, in manager order.
pub fn plugin_views(manager: &PluginManager) -> Vec<PluginView> {
    manager
        .plugins()
        .map(|info| {
            let manifest = &info.descriptor.manifest;
            let (status, failed) = match info.status {
                PluginStatus::Active => ("Active".to_owned(), false),
                PluginStatus::Inactive => ("Inactive".to_owned(), false),
                PluginStatus::Failed(msg) => (format!("Failed: {msg}"), true),
            };
            let id = info.descriptor.id();
            let missing = manager.missing_permissions(id);
            let permissions = permissions_text(
                manifest
                    .permissions()
                    .iter()
                    .map(|p| (p.as_str(), !missing.contains(p))),
            );
            PluginView {
                id: id.clone(),
                name: manifest.name().to_owned(),
                version: manifest.version().to_string(),
                description: manifest.description().to_owned(),
                enabled: info.enabled,
                status,
                failed,
                permissions,
                needs_grant: !missing.is_empty(),
            }
        })
        .collect()
}

/// `"Permissions: fs-read, network (not granted)"` from `(name, granted)`
/// pairs; empty when there are none.
fn permissions_text<'a>(permissions: impl Iterator<Item = (&'a str, bool)>) -> String {
    let parts: Vec<String> = permissions
        .map(|(name, granted)| {
            if granted {
                name.to_owned()
            } else {
                format!("{name} (not granted)")
            }
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("Permissions: {}", parts.join(", "))
    }
}

/// Makes the theme registry match the active plugins: themes of plugins
/// that are no longer active are removed, and every theme file contributed
/// by an active plugin is loaded. Returns one message per problem, including
/// contributions this version does not use yet (languages, snippets,
/// database drivers; command keybindings are only shown as hints).
pub fn sync_plugin_themes(manager: &PluginManager, registry: &mut ThemeRegistry) -> Vec<String> {
    for info in manager.plugins() {
        registry.remove_by_source(&ThemeSource::Plugin(info.descriptor.id().to_string()));
    }
    let contributions = manager.contributions();
    let mut problems = Vec::new();
    for theme in &contributions.themes {
        let source = ThemeSource::Plugin(theme.plugin.to_string());
        if let Err(e) = registry.load_file_as(&theme.path, source) {
            problems.push(format!(
                "plugin {}: theme {}: {e}",
                theme.plugin,
                theme.path.display()
            ));
        }
    }
    let unsupported = [
        (
            "languages",
            contributions
                .languages
                .iter()
                .map(|c| &c.plugin)
                .collect::<Vec<_>>(),
        ),
        (
            "snippets",
            contributions.snippets.iter().map(|c| &c.plugin).collect(),
        ),
        (
            "database drivers",
            contributions.db_drivers.iter().map(|c| &c.plugin).collect(),
        ),
    ];
    for (what, plugins) in unsupported {
        let mut plugins = plugins;
        plugins.dedup();
        for plugin in plugins {
            if manager
                .plugin(plugin)
                .is_some_and(|p| p.descriptor.source != PluginSource::Builtin)
            {
                problems.push(format!(
                    "plugin {plugin} contributes {what}, which this version does not support yet"
                ));
            }
        }
    }
    problems
}

/// Commands of active plugins, sorted by title.
pub fn commands(manager: &PluginManager) -> Vec<ContributedCommand> {
    let mut commands = manager.contributions().commands;
    commands.sort_by(|a, b| a.title.cmp(&b.title));
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{channel, AppEvent};

    #[test]
    fn builtins_activate_and_hello_reaches_the_ui_queue() {
        let (sink, queue) = channel();
        let dir = tempfile::tempdir().unwrap();
        let (manager, problems) = start(sink.clone(), Some(dir.path()));
        assert!(problems.is_empty(), "{problems:?}");
        let mut manager = manager.unwrap();

        let views = plugin_views(&manager);
        let hello = views
            .iter()
            .find(|v| v.id.as_str() == apex_plugins::HelloPlugin::ID)
            .unwrap();
        assert!(hello.enabled);
        assert_eq!(hello.status, "Active");

        let cmds = commands(&manager);
        assert!(cmds
            .iter()
            .any(|c| c.id == apex_plugins::HelloPlugin::SAY_HELLO));
        manager
            .execute_command(apex_plugins::HelloPlugin::SAY_HELLO, &["Apex".into()])
            .unwrap();
        // No Slint event loop in tests: read the queue directly.
        let events = queue.drain(&sink);
        assert!(events.iter().any(|e| matches!(
            e,
            AppEvent::Notice(NoticeKind::Info, text) if text.contains("Hello, Apex!")
        )));

        // Disabling is persisted in the plugins folder and deactivates.
        let id = hello.id.clone();
        manager.set_enabled(&id, false).unwrap();
        assert!(!manager.is_active(&id));
        assert!(dir.path().join(apex_plugins::STATE_FILE_NAME).is_file());
    }

    #[test]
    fn active_plugin_themes_are_selectable_and_leave_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("sample-theme");
        std::fs::create_dir_all(plugin.join("themes")).unwrap();
        std::fs::write(
            plugin.join(apex_plugins::MANIFEST_FILE_NAME),
            "id = \"com.example.theme\"\nname = \"Sample\"\nversion = \"0.1.0\"\n\
             apex_api = \"^0.1\"\n[contributes]\nthemes = [\"themes/sample-dark.json\"]\n",
        )
        .unwrap();
        // A VS Code theme takes its id from the file name.
        std::fs::write(
            plugin.join("themes").join("sample-dark.json"),
            r##"{ "type": "dark", "colors": { "editor.background": "#101010",
                "editor.foreground": "#e0e0e0" }, "tokenColors": [] }"##,
        )
        .unwrap();

        let (sink, _queue) = channel();
        let (manager, problems) = start(sink, Some(dir.path()));
        assert!(problems.is_empty(), "{problems:?}");
        let mut manager = manager.unwrap();
        let mut registry = ThemeRegistry::new();
        let problems = sync_plugin_themes(&manager, &mut registry);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(registry.contains("sample-dark"));
        assert_eq!(
            registry.source("sample-dark"),
            Some(&ThemeSource::Plugin("com.example.theme".into()))
        );

        let id = PluginId::new("com.example.theme").unwrap();
        manager.set_enabled(&id, false).unwrap();
        assert!(sync_plugin_themes(&manager, &mut registry).is_empty());
        assert!(!registry.contains("sample-dark"));

        manager.set_enabled(&id, true).unwrap();
        sync_plugin_themes(&manager, &mut registry);
        assert!(registry.contains("sample-dark"));
    }

    #[test]
    fn permission_text_marks_missing_grants() {
        assert_eq!(permissions_text(std::iter::empty()), "");
        assert_eq!(
            permissions_text([("fs-read", true), ("network", false)].into_iter()),
            "Permissions: fs-read, network (not granted)"
        );
    }
}
