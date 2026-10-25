//! Plugins compiled into Apex IDE.

use std::sync::Arc;

use crate::host::{command_handler, LogLevel, Notification, PluginHost};
use crate::manifest::{ManifestError, PluginManifest};
use crate::runtime::{BuiltinPlugin, RuntimeError};

/// Every built-in plugin, ready for
/// [`PluginManager::register_builtin`](crate::PluginManager::register_builtin).
pub fn builtin_plugins() -> Vec<Arc<dyn BuiltinPlugin>> {
    vec![Arc::new(HelloPlugin)]
}

/// Example built-in plugin. Contributes one command,
/// [`HelloPlugin::SAY_HELLO`], which shows a greeting notification. Passing a
/// first argument personalises it (`Hello, <arg>!`).
#[derive(Debug, Clone, Copy, Default)]
pub struct HelloPlugin;

impl HelloPlugin {
    /// Plugin id.
    pub const ID: &'static str = "apex.hello";
    /// Id of the greeting command.
    pub const SAY_HELLO: &'static str = "apex.hello.sayHello";

    const MANIFEST: &'static str = r#"
id = "apex.hello"
name = "Hello"
version = "0.1.0"
apex_api = "^0.1"
description = "Example built-in plugin: greets you from the command palette."
authors = ["Apex IDE"]

[[contributes.commands]]
id = "apex.hello.sayHello"
title = "Hello: Say Hello"
keybinding = "Ctrl+Alt+H"
"#;

    /// The greeting the command shows for `args`.
    pub fn greeting(args: &[String]) -> String {
        match args.first().map(|a| a.trim()).filter(|a| !a.is_empty()) {
            Some(name) => format!("Hello, {name}!"),
            None => "Hello from Apex IDE!".to_owned(),
        }
    }
}

impl BuiltinPlugin for HelloPlugin {
    fn manifest(&self) -> Result<PluginManifest, ManifestError> {
        PluginManifest::from_toml_str(Self::MANIFEST)
    }

    fn activate(&self, host: &mut dyn PluginHost) -> Result<(), RuntimeError> {
        host.register_command(
            Self::SAY_HELLO,
            command_handler(|host, args| {
                let greeting = HelloPlugin::greeting(args);
                host.log(LogLevel::Info, &format!("saying: {greeting}"));
                host.show_notification(Notification::info(greeting));
                Ok(())
            }),
        )?;
        host.log(LogLevel::Debug, "activated");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_is_valid_and_declares_command() {
        let m = HelloPlugin.manifest().unwrap();
        assert_eq!(m.id().as_str(), HelloPlugin::ID);
        assert!(m.contributes().command(HelloPlugin::SAY_HELLO).is_some());
        assert!(m.permissions().is_empty());
    }

    #[test]
    fn greeting_uses_first_argument() {
        assert_eq!(HelloPlugin::greeting(&[]), "Hello from Apex IDE!");
        assert_eq!(
            HelloPlugin::greeting(&["  ".into()]),
            "Hello from Apex IDE!"
        );
        assert_eq!(
            HelloPlugin::greeting(&["Ada".into(), "x".into()]),
            "Hello, Ada!"
        );
    }
}
