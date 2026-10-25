//! Finding plugins on disk.
//!
//! A plugins directory looks like:
//!
//! ```text
//! plugins/
//! ├── plugins.toml                 # enable/disable state (see `state`)
//! ├── sample-theme-plugin/
//! │   ├── apex-plugin.toml
//! │   └── themes/sample-dark.json
//! └── another-plugin/
//!     └── apex-plugin.toml
//! ```
//!
//! Every immediate subfolder containing `apex-plugin.toml` is a plugin.
//! Hidden folders (name starting with `.`) and folders without a manifest are
//! skipped. A bad plugin produces a [`PluginIssue`] and never aborts the scan.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::descriptor::{PluginDescriptor, PluginSource};
use crate::error::PluginError;
use crate::id::PluginId;
use crate::manifest::{PluginManifest, MANIFEST_FILE_NAME};

/// A problem with one plugin (or the state file) found while scanning.
#[derive(Debug)]
pub struct PluginIssue {
    /// The folder or file the issue is about.
    pub path: PathBuf,
    /// What went wrong.
    pub error: PluginError,
}

/// Result of [`discover_plugins`].
#[derive(Debug, Default)]
pub struct DiscoveryReport {
    /// Valid, API-compatible plugins, sorted by folder name.
    pub plugins: Vec<PluginDescriptor>,
    /// Plugins that were skipped, with the reason.
    pub issues: Vec<PluginIssue>,
}

/// Scans `dir` for plugin folders.
///
/// A missing `dir` yields an empty report. Only failing to list an existing
/// `dir` is an error; everything else is reported per plugin in
/// [`DiscoveryReport::issues`].
pub fn discover_plugins(dir: &Path) -> Result<DiscoveryReport, PluginError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(DiscoveryReport::default()),
        Err(source) => {
            return Err(PluginError::Io {
                path: dir.to_path_buf(),
                source,
            })
        }
    };

    let mut report = DiscoveryReport::default();
    let mut folders = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                let hidden = entry.file_name().to_string_lossy().starts_with('.');
                // `is_dir` follows symlinks, so linked plugin folders work.
                if !hidden && path.is_dir() {
                    folders.push(path);
                }
            }
            Err(source) => report.issues.push(PluginIssue {
                path: dir.to_path_buf(),
                error: PluginError::Io {
                    path: dir.to_path_buf(),
                    source,
                },
            }),
        }
    }
    folders.sort();

    let mut seen: BTreeMap<PluginId, PathBuf> = BTreeMap::new();
    for folder in folders {
        let manifest_path = folder.join(MANIFEST_FILE_NAME);
        if !manifest_path.is_file() {
            tracing::debug!(folder = %folder.display(), "skipping folder without {MANIFEST_FILE_NAME}");
            continue;
        }
        match PluginManifest::load_from_dir(&folder) {
            Ok(manifest) => {
                if let Some(first) = seen.get(manifest.id()) {
                    report.issues.push(PluginIssue {
                        path: folder.clone(),
                        error: PluginError::DuplicatePlugin {
                            id: manifest.id().clone(),
                            existing: first.display().to_string(),
                        },
                    });
                    continue;
                }
                seen.insert(manifest.id().clone(), folder.clone());
                report.plugins.push(PluginDescriptor {
                    manifest,
                    source: PluginSource::Directory(folder),
                });
            }
            Err(source) => report.issues.push(PluginIssue {
                path: folder,
                error: PluginError::InvalidManifest {
                    origin: manifest_path.display().to_string(),
                    source,
                },
            }),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ManifestError;

    fn write_plugin(root: &Path, folder: &str, manifest: &str) -> PathBuf {
        let dir = root.join(folder);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MANIFEST_FILE_NAME), manifest).unwrap();
        dir
    }

    fn manifest(id: &str, api: &str) -> String {
        format!("id = \"{id}\"\nname = \"{id}\"\nversion = \"0.1.0\"\napex_api = \"{api}\"\n")
    }

    #[test]
    fn missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let report = discover_plugins(&dir.path().join("nope")).unwrap();
        assert!(report.plugins.is_empty() && report.issues.is_empty());
    }

    #[test]
    fn scans_and_reports_per_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_plugin(root, "b-good", &manifest("com.example.good", "^0.1"));
        write_plugin(root, "a-broken", "this is = = not toml");
        write_plugin(root, "c-future", &manifest("com.example.future", "^2"));
        write_plugin(root, "d-dupe", &manifest("com.example.good", "^0.1"));
        write_plugin(root, ".hidden", &manifest("com.example.hidden", "^0.1"));
        fs::create_dir_all(root.join("e-no-manifest")).unwrap();
        fs::write(root.join("plugins.toml"), "disabled = []").unwrap();
        fs::write(root.join("stray-file.txt"), "x").unwrap();

        let report = discover_plugins(root).unwrap();
        let ids: Vec<_> = report.plugins.iter().map(|p| p.id().as_str()).collect();
        assert_eq!(ids, vec!["com.example.good"]);
        assert_eq!(
            report.plugins[0].root(),
            Some(root.join("b-good").as_path())
        );

        assert_eq!(report.issues.len(), 3, "{:#?}", report.issues);
        let by_folder = |name: &str| {
            report
                .issues
                .iter()
                .find(|i| i.path.ends_with(name))
                .map(|i| &i.error)
        };
        assert!(matches!(
            by_folder("a-broken"),
            Some(PluginError::InvalidManifest {
                source: ManifestError::Toml(_),
                ..
            })
        ));
        assert!(matches!(
            by_folder("c-future"),
            Some(PluginError::InvalidManifest {
                source: ManifestError::IncompatibleApi { .. },
                ..
            })
        ));
        assert!(matches!(
            by_folder("d-dupe"),
            Some(PluginError::DuplicatePlugin { .. })
        ));
    }

    #[test]
    fn shipped_example_plugin_is_valid() {
        let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        let report = discover_plugins(&examples).unwrap();
        assert!(report.issues.is_empty(), "{:#?}", report.issues);
        let plugin = report
            .plugins
            .iter()
            .find(|p| p.id().as_str() == "com.example.sample-theme")
            .expect("sample theme plugin present");
        for theme in &plugin.manifest.contributes().themes {
            let path = plugin.resolve(theme);
            assert!(path.is_file(), "missing theme file {}", path.display());
        }
    }
}
