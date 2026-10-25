//! The `apex-plugin.toml` manifest format and its validation.
//!
//! ```toml
//! id = "com.example.sample-theme"
//! name = "Sample Theme"
//! version = "0.1.0"
//! apex_api = "^0.1"
//! description = "A dark theme"
//! authors = ["Jane Doe <jane@example.com>"]
//! entry = "plugin.wasm"           # optional, reserved for the future WASM runtime
//! permissions = ["fs-read"]       # fs-read | fs-write | process | network | clipboard
//!
//! [contributes]
//! themes = ["themes/sample-dark.json"]
//! snippets = ["snippets/python.json"]
//! db_drivers = ["duckdb"]
//!
//! [[contributes.commands]]
//! id = "sample.reload"
//! title = "Sample: Reload"
//! keybinding = "Ctrl+Alt+R"      # optional
//!
//! [[contributes.languages]]
//! id = "jinja"
//! extensions = ["j2", "jinja"]
//! ```

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use crate::id::{InvalidPluginId, PluginId};

/// The plugin API version implemented by this build of Apex IDE.
///
/// A plugin is loadable when its `apex_api` requirement matches this version.
pub const HOST_API_VERSION: Version = Version::new(0, 1, 0);

/// File name of a plugin manifest inside a plugin folder.
pub const MANIFEST_FILE_NAME: &str = "apex-plugin.toml";

/// A capability a plugin must declare before the host lets it use the
/// matching host function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    /// Read files from disk (`fs-read`).
    FsRead,
    /// Create, modify or delete files (`fs-write`).
    FsWrite,
    /// Spawn child processes (`process`).
    Process,
    /// Open network connections (`network`).
    Network,
    /// Read or write the system clipboard (`clipboard`).
    Clipboard,
}

impl Permission {
    /// Every permission, in declaration order.
    pub const ALL: [Permission; 5] = [
        Permission::FsRead,
        Permission::FsWrite,
        Permission::Process,
        Permission::Network,
        Permission::Clipboard,
    ];

    /// The manifest spelling, e.g. `fs-read`.
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::FsRead => "fs-read",
            Permission::FsWrite => "fs-write",
            Permission::Process => "process",
            Permission::Network => "network",
            Permission::Clipboard => "clipboard",
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A command a plugin adds to the command palette / keymap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandContribution {
    /// Globally unique command id, e.g. `apex.hello.sayHello`.
    pub id: String,
    /// Title shown in the command palette.
    pub title: String,
    /// Optional default keybinding, e.g. `Ctrl+Alt+H`. Parsed by the app.
    pub keybinding: Option<String>,
}

/// A language (file type) a plugin teaches the editor about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageContribution {
    /// Language id, e.g. `jinja`.
    pub id: String,
    /// File extensions without the leading dot, lowercased, e.g. `["j2"]`.
    pub extensions: Vec<String>,
}

/// Everything a plugin declares statically in `[contributes]`.
///
/// Paths are relative to the plugin folder and are guaranteed not to escape
/// it (no absolute paths, no `..`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Contributions {
    /// Theme files (JSON).
    pub themes: Vec<PathBuf>,
    /// Commands.
    pub commands: Vec<CommandContribution>,
    /// Languages / file types.
    pub languages: Vec<LanguageContribution>,
    /// Snippet files.
    pub snippets: Vec<PathBuf>,
    /// Ids of database drivers the plugin provides (matched against `apex-db`).
    pub db_drivers: Vec<String>,
}

impl Contributions {
    /// Returns the declared command with `id`, if any.
    pub fn command(&self, id: &str) -> Option<&CommandContribution> {
        self.commands.iter().find(|c| c.id == id)
    }
}

/// A parsed and validated `apex-plugin.toml`.
///
/// Instances can only be obtained through parsing, so every value upholds the
/// validation rules described on [`ManifestError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    id: PluginId,
    name: String,
    version: Version,
    apex_api: VersionReq,
    description: String,
    authors: Vec<String>,
    entry: Option<PathBuf>,
    permissions: BTreeSet<Permission>,
    contributes: Contributions,
}

/// Reasons a manifest can be rejected.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The manifest file could not be read.
    #[error("failed to read {}: {source}", path.display())]
    Io {
        /// Path of the manifest.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML or does not match the manifest schema
    /// (missing field, unknown field, unknown permission, wrong type...).
    #[error("invalid manifest: {0}")]
    Toml(Box<toml::de::Error>),
    /// `id` is not a valid reverse-DNS plugin id.
    #[error(transparent)]
    InvalidId(#[from] InvalidPluginId),
    /// `name` is empty or whitespace.
    #[error("plugin name must not be empty")]
    EmptyName,
    /// `version` is not a semantic version.
    #[error("invalid plugin version `{value}`: {source}")]
    InvalidVersion {
        /// The rejected value.
        value: String,
        /// Parser error.
        #[source]
        source: semver::Error,
    },
    /// `apex_api` is not a semver requirement.
    #[error("invalid apex_api requirement `{value}`: {source}")]
    InvalidApiRequirement {
        /// The rejected value.
        value: String,
        /// Parser error.
        #[source]
        source: semver::Error,
    },
    /// The plugin targets an API version this host does not provide.
    #[error("plugin requires Apex plugin API `{required}`, but this host provides {host}")]
    IncompatibleApi {
        /// The plugin's requirement.
        required: VersionReq,
        /// The host's API version.
        host: Version,
    },
    /// A path is absolute, empty or escapes the plugin folder.
    #[error("invalid {field} path `{path}`: must be relative and stay inside the plugin folder")]
    InvalidPath {
        /// Manifest field holding the path.
        field: &'static str,
        /// The rejected path.
        path: String,
    },
    /// A command id is empty or contains unsupported characters.
    #[error("invalid command id `{0}`: use ASCII letters, digits, `.`, `-` or `_`")]
    InvalidCommandId(String),
    /// A command has an empty title.
    #[error("command `{0}` must have a non-empty title")]
    EmptyCommandTitle(String),
    /// The same command id is declared twice.
    #[error("command `{0}` is declared more than once")]
    DuplicateCommand(String),
    /// A language contribution is malformed.
    #[error("invalid language contribution `{id}`: {reason}")]
    InvalidLanguage {
        /// Language id as written.
        id: String,
        /// What is wrong.
        reason: &'static str,
    },
    /// A database driver id is empty or contains unsupported characters.
    #[error("invalid database driver id `{0}`")]
    InvalidDbDriver(String),
}

impl From<toml::de::Error> for ManifestError {
    fn from(err: toml::de::Error) -> Self {
        Self::Toml(Box::new(err))
    }
}

#[derive(Deserialize)]
struct ApiProbe {
    apex_api: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    id: String,
    name: String,
    version: String,
    apex_api: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    entry: Option<String>,
    #[serde(default)]
    permissions: Vec<Permission>,
    #[serde(default)]
    contributes: RawContributions,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawContributions {
    #[serde(default)]
    themes: Vec<String>,
    #[serde(default)]
    commands: Vec<RawCommand>,
    #[serde(default)]
    languages: Vec<RawLanguage>,
    #[serde(default)]
    snippets: Vec<String>,
    #[serde(default)]
    db_drivers: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    id: String,
    title: String,
    #[serde(default)]
    keybinding: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLanguage {
    id: String,
    extensions: Vec<String>,
}

impl PluginManifest {
    /// Parses and validates a manifest, checking API compatibility against
    /// [`HOST_API_VERSION`].
    pub fn from_toml_str(source: &str) -> Result<Self, ManifestError> {
        Self::from_toml_str_for_host(source, &HOST_API_VERSION)
    }

    /// Parses and validates a manifest, checking API compatibility against
    /// `host_api` instead of [`HOST_API_VERSION`].
    ///
    /// Compatibility is checked *before* strict schema validation, so a
    /// plugin written for a newer API (which may use fields this host does not
    /// know) is reported as [`ManifestError::IncompatibleApi`] rather than as
    /// an unknown-field error.
    pub fn from_toml_str_for_host(source: &str, host_api: &Version) -> Result<Self, ManifestError> {
        let probe: ApiProbe = toml::from_str(source)?;
        if let Some(raw_req) = probe.apex_api {
            let required = parse_requirement(&raw_req)?;
            if !required.matches(host_api) {
                return Err(ManifestError::IncompatibleApi {
                    required,
                    host: host_api.clone(),
                });
            }
        }
        let raw: RawManifest = toml::from_str(source)?;
        Self::validate(raw)
    }

    /// Reads and validates `<dir>/apex-plugin.toml`.
    pub fn load_from_dir(dir: &Path) -> Result<Self, ManifestError> {
        let path = dir.join(MANIFEST_FILE_NAME);
        let source = fs::read_to_string(&path).map_err(|source| ManifestError::Io {
            path: path.clone(),
            source,
        })?;
        Self::from_toml_str(&source)
    }

    fn validate(raw: RawManifest) -> Result<Self, ManifestError> {
        let id = PluginId::new(raw.id)?;
        let name = raw.name.trim().to_owned();
        if name.is_empty() {
            return Err(ManifestError::EmptyName);
        }
        let version =
            Version::parse(raw.version.trim()).map_err(|source| ManifestError::InvalidVersion {
                value: raw.version.clone(),
                source,
            })?;
        let apex_api = parse_requirement(&raw.apex_api)?;
        let entry = raw.entry.map(|e| relative_path("entry", &e)).transpose()?;
        let authors = raw
            .authors
            .into_iter()
            .map(|a| a.trim().to_owned())
            .filter(|a| !a.is_empty())
            .collect();

        Ok(Self {
            id,
            name,
            version,
            apex_api,
            description: raw.description.trim().to_owned(),
            authors,
            entry,
            permissions: raw.permissions.into_iter().collect(),
            contributes: validate_contributions(raw.contributes)?,
        })
    }

    /// Unique plugin id.
    pub fn id(&self) -> &PluginId {
        &self.id
    }

    /// Display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Plugin version.
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Required host API version range.
    pub fn apex_api(&self) -> &VersionReq {
        &self.apex_api
    }

    /// Whether this plugin can run against host API `host_api`.
    pub fn is_compatible_with(&self, host_api: &Version) -> bool {
        self.apex_api.matches(host_api)
    }

    /// Free-form description (may be empty).
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Author strings (may be empty).
    pub fn authors(&self) -> &[String] {
        &self.authors
    }

    /// Optional code entry point relative to the plugin folder (a future
    /// `.wasm` component). `None` means a purely declarative plugin.
    pub fn entry(&self) -> Option<&Path> {
        self.entry.as_deref()
    }

    /// Permissions the plugin requests.
    pub fn permissions(&self) -> &BTreeSet<Permission> {
        &self.permissions
    }

    /// Whether the plugin requests `permission`.
    pub fn requests(&self, permission: Permission) -> bool {
        self.permissions.contains(&permission)
    }

    /// Static contributions.
    pub fn contributes(&self) -> &Contributions {
        &self.contributes
    }
}

fn parse_requirement(raw: &str) -> Result<VersionReq, ManifestError> {
    VersionReq::parse(raw.trim()).map_err(|source| ManifestError::InvalidApiRequirement {
        value: raw.to_owned(),
        source,
    })
}

/// Accepts only non-empty relative paths made of normal components (and `.`).
fn relative_path(field: &'static str, raw: &str) -> Result<PathBuf, ManifestError> {
    let invalid = || ManifestError::InvalidPath {
        field,
        path: raw.to_owned(),
    };
    let trimmed = raw.trim();
    // Reject Windows-style roots even on Unix, and Unix roots on Windows, so a
    // manifest is valid or invalid identically on every platform.
    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.starts_with('\\')
        || trimmed.contains(':')
    {
        return Err(invalid());
    }
    let path = PathBuf::from(trimmed);
    let mut has_normal = false;
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                // On Unix a backslash is a legal file-name character; treat
                // it as a separator so `..\x` cannot slip through.
                if part.to_string_lossy().split('\\').any(|p| p == "..") {
                    return Err(invalid());
                }
                has_normal = true;
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(invalid())
            }
        }
    }
    if has_normal {
        Ok(path)
    } else {
        Err(invalid())
    }
}

/// Command ids and driver ids: ASCII alphanumeric start, then alphanumerics,
/// `.`, `-`, `_`.
fn is_simple_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

fn validate_contributions(raw: RawContributions) -> Result<Contributions, ManifestError> {
    let themes = raw
        .themes
        .iter()
        .map(|p| relative_path("contributes.themes", p))
        .collect::<Result<_, _>>()?;
    let snippets = raw
        .snippets
        .iter()
        .map(|p| relative_path("contributes.snippets", p))
        .collect::<Result<_, _>>()?;

    let mut seen = HashSet::new();
    let mut commands = Vec::with_capacity(raw.commands.len());
    for cmd in raw.commands {
        let id = cmd.id.trim().to_owned();
        if !is_simple_id(&id) {
            return Err(ManifestError::InvalidCommandId(cmd.id));
        }
        let title = cmd.title.trim().to_owned();
        if title.is_empty() {
            return Err(ManifestError::EmptyCommandTitle(id));
        }
        if !seen.insert(id.clone()) {
            return Err(ManifestError::DuplicateCommand(id));
        }
        let keybinding = cmd
            .keybinding
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty());
        commands.push(CommandContribution {
            id,
            title,
            keybinding,
        });
    }

    let mut languages = Vec::with_capacity(raw.languages.len());
    for lang in raw.languages {
        let id = lang.id.trim().to_owned();
        if !is_simple_id(&id) {
            return Err(ManifestError::InvalidLanguage {
                id: lang.id,
                reason: "id must use ASCII letters, digits, `.`, `-` or `_`",
            });
        }
        if lang.extensions.is_empty() {
            return Err(ManifestError::InvalidLanguage {
                id,
                reason: "at least one file extension is required",
            });
        }
        let mut extensions = Vec::with_capacity(lang.extensions.len());
        for ext in &lang.extensions {
            let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
            if ext.is_empty()
                || ext
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '/' | '\\' | '*'))
            {
                return Err(ManifestError::InvalidLanguage {
                    id,
                    reason: "extensions must be non-empty and contain no separators, wildcards or whitespace",
                });
            }
            if !extensions.contains(&ext) {
                extensions.push(ext);
            }
        }
        languages.push(LanguageContribution { id, extensions });
    }

    let mut db_drivers = Vec::with_capacity(raw.db_drivers.len());
    for driver in raw.db_drivers {
        let id = driver.trim().to_owned();
        if !is_simple_id(&id) {
            return Err(ManifestError::InvalidDbDriver(driver));
        }
        if !db_drivers.contains(&id) {
            db_drivers.push(id);
        }
    }

    Ok(Contributions {
        themes,
        commands,
        languages,
        snippets,
        db_drivers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
        id = "com.example.full"
        name = "  Full Plugin "
        version = "1.2.3-beta.1"
        apex_api = "^0.1"
        description = "Everything set"
        authors = ["Jane", "  "]
        entry = "bin/plugin.wasm"
        permissions = ["fs-read", "network", "fs-read"]

        [contributes]
        themes = ["themes/dark.json", "./themes/light.json"]
        snippets = ["snippets/python.json"]
        db_drivers = ["duckdb", "duckdb"]

        [[contributes.commands]]
        id = "full.run"
        title = "Full: Run"
        keybinding = "Ctrl+Alt+R"

        [[contributes.commands]]
        id = "full.other"
        title = "Full: Other"
        keybinding = "   "

        [[contributes.languages]]
        id = "jinja"
        extensions = [".J2", "jinja", "j2"]
    "#;

    fn minimal(extra: &str) -> String {
        format!("id = \"com.example.min\"\nname = \"Min\"\nversion = \"0.1.0\"\napex_api = \"^0.1\"\n{extra}")
    }

    #[test]
    fn parses_full_manifest() {
        let m = PluginManifest::from_toml_str(FULL).unwrap();
        assert_eq!(m.id().as_str(), "com.example.full");
        assert_eq!(m.name(), "Full Plugin");
        assert_eq!(m.version(), &Version::parse("1.2.3-beta.1").unwrap());
        assert!(m.is_compatible_with(&HOST_API_VERSION));
        assert_eq!(m.authors(), ["Jane".to_owned()]);
        assert_eq!(m.entry(), Some(Path::new("bin/plugin.wasm")));
        assert_eq!(
            m.permissions().iter().copied().collect::<Vec<_>>(),
            vec![Permission::FsRead, Permission::Network]
        );
        assert!(m.requests(Permission::FsRead));
        assert!(!m.requests(Permission::Process));

        let c = m.contributes();
        assert_eq!(c.themes.len(), 2);
        assert_eq!(c.snippets, vec![PathBuf::from("snippets/python.json")]);
        assert_eq!(c.db_drivers, vec!["duckdb".to_owned()]);
        assert_eq!(
            c.command("full.run").and_then(|c| c.keybinding.as_deref()),
            Some("Ctrl+Alt+R")
        );
        assert_eq!(
            c.command("full.other").map(|c| c.keybinding.clone()),
            Some(None)
        );
        assert_eq!(
            c.languages[0].extensions,
            vec!["j2".to_owned(), "jinja".to_owned()]
        );
    }

    #[test]
    fn minimal_manifest_has_empty_defaults() {
        let m = PluginManifest::from_toml_str(&minimal("")).unwrap();
        assert!(m.entry().is_none());
        assert!(m.permissions().is_empty());
        assert_eq!(m.contributes(), &Contributions::default());
    }

    #[test]
    fn rejects_bad_id() {
        let src = minimal("").replace("com.example.min", "NotReverseDns");
        assert!(matches!(
            PluginManifest::from_toml_str(&src),
            Err(ManifestError::InvalidId(_))
        ));
    }

    #[test]
    fn rejects_bad_semver() {
        for bad in ["1.0", "v1.0.0", "latest", ""] {
            let src = minimal("").replace("version = \"0.1.0\"", &format!("version = \"{bad}\""));
            assert!(
                matches!(
                    PluginManifest::from_toml_str(&src),
                    Err(ManifestError::InvalidVersion { .. })
                ),
                "version `{bad}`"
            );
        }
    }

    #[test]
    fn rejects_bad_api_requirement() {
        let src = minimal("").replace("^0.1", "not a range");
        assert!(matches!(
            PluginManifest::from_toml_str(&src),
            Err(ManifestError::InvalidApiRequirement { .. })
        ));
    }

    #[test]
    fn rejects_incompatible_api() {
        for req in ["^0.2", "^1", ">=0.2.0", "=0.0.9"] {
            let src = minimal("").replace("^0.1", req);
            assert!(
                matches!(
                    PluginManifest::from_toml_str(&src),
                    Err(ManifestError::IncompatibleApi { .. })
                ),
                "requirement `{req}`"
            );
        }
    }

    #[test]
    fn incompatible_api_wins_over_unknown_fields() {
        let src = minimal("future_field = true").replace("^0.1", "^0.2");
        assert!(matches!(
            PluginManifest::from_toml_str(&src),
            Err(ManifestError::IncompatibleApi { .. })
        ));
        // A newer host accepts the requirement and then rejects the unknown field.
        let newer = Version::new(0, 2, 0);
        assert!(matches!(
            PluginManifest::from_toml_str_for_host(&src, &newer),
            Err(ManifestError::Toml(_))
        ));
    }

    #[test]
    fn rejects_schema_violations() {
        for extra in [
            "permissions = [\"root\"]",
            "unknown = 1",
            "[contributes]\nwidgets = []",
            "[[contributes.commands]]\nid = \"x\"",
        ] {
            assert!(
                matches!(
                    PluginManifest::from_toml_str(&minimal(extra)),
                    Err(ManifestError::Toml(_))
                ),
                "extra `{extra}`"
            );
        }
        let missing_api = "id = \"a.b\"\nname = \"x\"\nversion = \"0.1.0\"";
        assert!(matches!(
            PluginManifest::from_toml_str(missing_api),
            Err(ManifestError::Toml(_))
        ));
        assert!(matches!(
            PluginManifest::from_toml_str("not = [toml"),
            Err(ManifestError::Toml(_))
        ));
    }

    #[test]
    fn rejects_empty_name() {
        let src = minimal("").replace("name = \"Min\"", "name = \"   \"");
        assert!(matches!(
            PluginManifest::from_toml_str(&src),
            Err(ManifestError::EmptyName)
        ));
    }

    #[test]
    fn rejects_escaping_paths() {
        for path in [
            "../evil.json",
            "themes/../../evil.json",
            "/etc/passwd",
            "\\\\server\\share",
            "C:\\themes\\x.json",
            "C:x.json",
            "..\\evil.json",
            "",
            ".",
        ] {
            let extra = format!("[contributes]\nthemes = [{path:?}]");
            assert!(
                matches!(
                    PluginManifest::from_toml_str(&minimal(&extra)),
                    Err(ManifestError::InvalidPath { .. })
                ),
                "path `{path}`"
            );
        }
        assert!(matches!(
            PluginManifest::from_toml_str(&minimal("entry = \"../x.wasm\"")),
            Err(ManifestError::InvalidPath { field: "entry", .. })
        ));
    }

    #[test]
    fn rejects_bad_commands() {
        let dup = "[[contributes.commands]]\nid = \"a.b\"\ntitle = \"A\"\n[[contributes.commands]]\nid = \"a.b\"\ntitle = \"B\"";
        assert!(matches!(
            PluginManifest::from_toml_str(&minimal(dup)),
            Err(ManifestError::DuplicateCommand(id)) if id == "a.b"
        ));
        let bad_id = "[[contributes.commands]]\nid = \"has space\"\ntitle = \"A\"";
        assert!(matches!(
            PluginManifest::from_toml_str(&minimal(bad_id)),
            Err(ManifestError::InvalidCommandId(_))
        ));
        let no_title = "[[contributes.commands]]\nid = \"a.b\"\ntitle = \" \"";
        assert!(matches!(
            PluginManifest::from_toml_str(&minimal(no_title)),
            Err(ManifestError::EmptyCommandTitle(_))
        ));
    }

    #[test]
    fn rejects_bad_languages_and_drivers() {
        for extra in [
            "[[contributes.languages]]\nid = \"x\"\nextensions = []",
            "[[contributes.languages]]\nid = \"x\"\nextensions = [\".\"]",
            "[[contributes.languages]]\nid = \"x\"\nextensions = [\"*.py\"]",
            "[[contributes.languages]]\nid = \"bad id\"\nextensions = [\"py\"]",
        ] {
            assert!(
                matches!(
                    PluginManifest::from_toml_str(&minimal(extra)),
                    Err(ManifestError::InvalidLanguage { .. })
                ),
                "extra `{extra}`"
            );
        }
        assert!(matches!(
            PluginManifest::from_toml_str(&minimal("[contributes]\ndb_drivers = [\"\"]")),
            Err(ManifestError::InvalidDbDriver(_))
        ));
    }

    #[test]
    fn load_from_dir_reports_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            PluginManifest::load_from_dir(dir.path()),
            Err(ManifestError::Io { .. })
        ));
    }

    #[test]
    fn permission_spelling_matches_serde() {
        for p in Permission::ALL {
            let toml = format!("p = \"{p}\"");
            #[derive(Deserialize)]
            struct W {
                p: Permission,
            }
            assert_eq!(toml::from_str::<W>(&toml).unwrap().p, p);
        }
    }
}
