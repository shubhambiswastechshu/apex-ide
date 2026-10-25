//! The project model.
//!
//! A project is a folder. Its Apex-specific configuration lives in
//! `<root>/.apex/project.toml`; folders without that file can still be
//! opened, in which case sensible defaults are derived from their contents.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::fs_util;

/// Name of the per-project configuration folder.
pub const CONFIG_DIR_NAME: &str = ".apex";
/// Name of the project configuration file inside [`CONFIG_DIR_NAME`].
pub const CONFIG_FILE_NAME: &str = "project.toml";
/// Current schema version written to `project.toml`.
pub const CONFIG_VERSION: u32 = 1;

/// The kind of template a project was created from. Stored as a kebab-case
/// string; unrecognised strings round-trip through [`TemplateKind::Other`],
/// so templates added by plugins or newer versions are never lost.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum TemplateKind {
    /// A plain Python project (the default).
    #[default]
    PurePython,
    /// A Django web project.
    Django,
    /// A Flask web project.
    Flask,
    /// A FastAPI web project.
    FastApi,
    /// A data-science project (notebooks, pandas, ...).
    DataScience,
    /// A data-analytics project (SQL, dashboards, reporting).
    DataAnalytics,
    /// Any other template, identified by its id.
    Other(String),
}

impl TemplateKind {
    /// The built-in template kinds, in the order a UI should list them.
    pub const BUILT_IN: [TemplateKind; 6] = [
        TemplateKind::PurePython,
        TemplateKind::Django,
        TemplateKind::Flask,
        TemplateKind::FastApi,
        TemplateKind::DataScience,
        TemplateKind::DataAnalytics,
    ];

    /// The stable id stored in `project.toml`.
    pub fn id(&self) -> &str {
        match self {
            Self::PurePython => "pure-python",
            Self::Django => "django",
            Self::Flask => "flask",
            Self::FastApi => "fastapi",
            Self::DataScience => "data-science",
            Self::DataAnalytics => "data-analytics",
            Self::Other(id) => id,
        }
    }

    /// A human-readable name for display.
    pub fn label(&self) -> &str {
        match self {
            Self::PurePython => "Pure Python",
            Self::Django => "Django",
            Self::Flask => "Flask",
            Self::FastApi => "FastAPI",
            Self::DataScience => "Data Science",
            Self::DataAnalytics => "Data Analytics",
            Self::Other(id) => id,
        }
    }
}

impl From<String> for TemplateKind {
    fn from(s: String) -> Self {
        match s.as_str() {
            "pure-python" => Self::PurePython,
            "django" => Self::Django,
            "flask" => Self::Flask,
            "fastapi" => Self::FastApi,
            "data-science" => Self::DataScience,
            "data-analytics" => Self::DataAnalytics,
            _ => Self::Other(s),
        }
    }
}

impl From<TemplateKind> for String {
    fn from(k: TemplateKind) -> Self {
        k.id().to_owned()
    }
}

impl FromStr for TemplateKind {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self::from(s.to_owned()))
    }
}

impl fmt::Display for TemplateKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// What a run configuration executes.
///
/// In TOML this appears as either `script = "main.py"` or
/// `module = "pkg.cli"` directly inside the run configuration table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunTarget {
    /// A script file, `python <path>`. Relative paths are relative to the
    /// project root.
    Script(PathBuf),
    /// A module, `python -m <module>`.
    Module(String),
}

/// A named way to run something in the project (like a PyCharm run
/// configuration).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunConfiguration {
    /// Display name, unique within the project by convention.
    pub name: String,
    /// The script or module to run.
    #[serde(flatten)]
    pub target: RunTarget,
    /// Command-line arguments passed after the script/module.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment variables.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Working directory; `None` means the project root. Relative paths are
    /// relative to the project root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,
}

impl RunConfiguration {
    /// A configuration that runs a script file.
    pub fn script(name: impl Into<String>, script: impl Into<PathBuf>) -> Self {
        Self::new(name, RunTarget::Script(script.into()))
    }

    /// A configuration that runs a module with `python -m`.
    pub fn module(name: impl Into<String>, module: impl Into<String>) -> Self {
        Self::new(name, RunTarget::Module(module.into()))
    }

    fn new(name: impl Into<String>, target: RunTarget) -> Self {
        Self {
            name: name.into(),
            target,
            args: Vec::new(),
            env: BTreeMap::new(),
            working_dir: None,
        }
    }

    /// Builder: sets the arguments.
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Builder: adds one environment variable.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// The working directory to use, resolved against `project_root`.
    pub fn resolved_working_dir(&self, project_root: &Path) -> PathBuf {
        match &self.working_dir {
            Some(dir) => project_root.join(dir),
            None => project_root.to_path_buf(),
        }
    }
}

/// The contents of `.apex/project.toml`.
///
/// Missing keys take their defaults and unknown keys are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    /// Schema version, see [`CONFIG_VERSION`].
    pub version: u32,
    /// Project display name. [`Project::save`] keeps this in sync with
    /// [`Project::name`].
    pub name: String,
    /// Path of the Python interpreter. A relative path (e.g.
    /// `.venv/bin/python`) is relative to the project root; prefer that for
    /// in-project virtual environments so the project survives being moved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interpreter: Option<PathBuf>,
    /// The template the project was created from.
    pub template: TemplateKind,
    /// Run configurations, in display order. Stored as `[[run]]` tables.
    #[serde(rename = "run", skip_serializing_if = "Vec::is_empty")]
    pub run_configurations: Vec<RunConfiguration>,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            name: String::new(),
            interpreter: None,
            template: TemplateKind::default(),
            run_configurations: Vec::new(),
        }
    }
}

/// An open project: a root folder plus its configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    /// Absolute path of the project folder.
    pub root: PathBuf,
    /// Display name.
    pub name: String,
    /// Configuration, saved to `<root>/.apex/project.toml` by [`Project::save`].
    pub config: ProjectConfig,
}

impl Project {
    /// Creates a new project at `root` (creating the folder if needed) and
    /// writes its configuration.
    ///
    /// Fails with [`CoreError::ProjectExists`] if `root` already holds a
    /// `.apex/project.toml`, and [`CoreError::InvalidProjectName`] if `name`
    /// is empty or contains control characters. `config.name` is overwritten
    /// with `name`.
    pub fn create(
        root: impl AsRef<Path>,
        name: impl Into<String>,
        config: ProjectConfig,
    ) -> Result<Self> {
        let name = validate_name(name.into())?;
        let root = fs_util::absolute(root.as_ref())?;
        if root.exists() && !root.is_dir() {
            return Err(CoreError::NotADirectory(root));
        }
        let config_path = Self::config_path_for(&root);
        if config_path.exists() {
            return Err(CoreError::ProjectExists(root));
        }
        std::fs::create_dir_all(&root).map_err(|e| CoreError::io(&root, e))?;

        let mut config = config;
        config.name.clone_from(&name);
        let project = Self { root, name, config };
        project.save()?;
        Ok(project)
    }

    /// Opens the project folder `root`.
    ///
    /// If `.apex/project.toml` exists it is loaded (a corrupt file is an
    /// error, so run configurations are never silently discarded). Otherwise
    /// defaults are derived: the folder name, an in-project `.venv`/`venv`
    /// interpreter if one exists, and the Django template if `manage.py` is
    /// present. Opening never writes to disk.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = fs_util::absolute(root.as_ref())?;
        if !root.is_dir() {
            return Err(CoreError::NotADirectory(root));
        }
        let config_path = Self::config_path_for(&root);
        let config = match fs_util::read_optional(&config_path)? {
            Some(text) => toml::from_str(&text).map_err(|e| CoreError::parse(config_path, e))?,
            None => derive_config(&root),
        };
        let name = if config.name.trim().is_empty() {
            folder_name(&root)
        } else {
            config.name.clone()
        };
        Ok(Self { root, name, config })
    }

    /// Writes the configuration to `<root>/.apex/project.toml` atomically,
    /// first copying [`Project::name`] into `config.name`.
    pub fn save(&self) -> Result<()> {
        let mut config = self.config.clone();
        config.name.clone_from(&self.name);
        config.version = CONFIG_VERSION;
        let text = toml::to_string_pretty(&config)?;
        fs_util::write_atomic(&self.config_path(), &text)
    }

    /// Path of this project's `.apex` folder.
    pub fn config_dir(&self) -> PathBuf {
        self.root.join(CONFIG_DIR_NAME)
    }

    /// Path of this project's `project.toml`.
    pub fn config_path(&self) -> PathBuf {
        Self::config_path_for(&self.root)
    }

    /// Path where a project rooted at `root` keeps its `project.toml`.
    pub fn config_path_for(root: &Path) -> PathBuf {
        root.join(CONFIG_DIR_NAME).join(CONFIG_FILE_NAME)
    }

    /// Whether `root` contains an Apex project configuration.
    pub fn is_project(root: &Path) -> bool {
        Self::config_path_for(root).is_file()
    }

    /// The configured interpreter resolved to an absolute path (relative
    /// paths are joined onto the project root).
    pub fn interpreter_path(&self) -> Option<PathBuf> {
        self.config.interpreter.as_ref().map(|p| self.root.join(p))
    }

    /// Looks up a run configuration by name.
    pub fn run_configuration(&self, name: &str) -> Option<&RunConfiguration> {
        self.config
            .run_configurations
            .iter()
            .find(|rc| rc.name == name)
    }
}

/// Relative path of the interpreter inside a virtual environment folder.
pub fn venv_interpreter_relative(venv_dir: &Path) -> PathBuf {
    if cfg!(windows) {
        venv_dir.join("Scripts").join("python.exe")
    } else {
        venv_dir.join("bin").join("python")
    }
}

fn derive_config(root: &Path) -> ProjectConfig {
    let interpreter = [".venv", "venv"]
        .iter()
        .map(|dir| venv_interpreter_relative(Path::new(dir)))
        .find(|rel| root.join(rel).is_file());
    let template = if root.join("manage.py").is_file() {
        TemplateKind::Django
    } else {
        TemplateKind::PurePython
    };
    ProjectConfig {
        name: folder_name(root),
        interpreter,
        template,
        ..ProjectConfig::default()
    }
}

fn folder_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

fn validate_name(name: String) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(CoreError::InvalidProjectName(name, "name is empty"));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(CoreError::InvalidProjectName(
            name,
            "name contains control characters",
        ));
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sample_config() -> ProjectConfig {
        ProjectConfig {
            interpreter: Some(venv_interpreter_relative(Path::new(".venv"))),
            template: TemplateKind::Django,
            run_configurations: vec![
                RunConfiguration::script("main", "main.py")
                    .with_args(["--verbose", "x y"])
                    .with_env("DEBUG", "1"),
                RunConfiguration::module("server", "django")
                    .with_args(["runserver"])
                    .with_env("DJANGO_SETTINGS_MODULE", "site.settings"),
            ],
            ..ProjectConfig::default()
        }
    }

    #[test]
    fn create_writes_config_and_open_reads_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("new").join("proj");
        let created = Project::create(&root, "My Project", sample_config()).unwrap();
        assert!(root.join(".apex").join("project.toml").is_file());
        assert_eq!(created.config.name, "My Project");
        assert_eq!(created.name, "My Project");

        let opened = Project::open(&root).unwrap();
        assert_eq!(opened.name, "My Project");
        assert_eq!(opened.root, created.root);
        assert_eq!(opened.config.name, "My Project");
        assert_eq!(opened.config.template, TemplateKind::Django);
        assert_eq!(
            opened.config.run_configurations,
            sample_config().run_configurations
        );
        assert_eq!(opened.config.interpreter, sample_config().interpreter);
    }

    #[test]
    fn create_refuses_existing_project() {
        let dir = tempfile::tempdir().unwrap();
        Project::create(dir.path(), "a", ProjectConfig::default()).unwrap();
        let err = Project::create(dir.path(), "b", ProjectConfig::default()).unwrap_err();
        assert!(matches!(err, CoreError::ProjectExists(_)));
    }

    #[test]
    fn create_rejects_bad_names_and_file_roots() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["", "   ", "a\nb"] {
            let err = Project::create(dir.path(), bad, ProjectConfig::default()).unwrap_err();
            assert!(matches!(err, CoreError::InvalidProjectName(..)), "{bad:?}");
        }
        let file = dir.path().join("file.txt");
        fs::write(&file, "x").unwrap();
        let err = Project::create(&file, "p", ProjectConfig::default()).unwrap_err();
        assert!(matches!(err, CoreError::NotADirectory(_)));
    }

    #[test]
    fn create_trims_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = Project::create(dir.path(), "  spaced  ", ProjectConfig::default()).unwrap();
        assert_eq!(p.name, "spaced");
    }

    #[test]
    fn open_plain_folder_derives_defaults_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("plain_folder");
        fs::create_dir(&root).unwrap();
        let p = Project::open(&root).unwrap();
        assert_eq!(p.name, "plain_folder");
        assert_eq!(p.config.template, TemplateKind::PurePython);
        assert_eq!(p.config.interpreter, None);
        assert!(p.config.run_configurations.is_empty());
        assert!(!root.join(".apex").exists());
        assert!(!Project::is_project(&root));
    }

    #[test]
    fn open_detects_venv_and_django() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let rel = venv_interpreter_relative(Path::new(".venv"));
        fs::create_dir_all(root.join(&rel).parent().unwrap()).unwrap();
        fs::write(root.join(&rel), "").unwrap();
        fs::write(root.join("manage.py"), "").unwrap();

        let p = Project::open(root).unwrap();
        assert_eq!(p.config.interpreter.as_deref(), Some(rel.as_path()));
        assert_eq!(p.interpreter_path(), Some(p.root.join(&rel)));
        assert_eq!(p.config.template, TemplateKind::Django);
    }

    #[test]
    fn open_missing_or_file_root_fails() {
        let dir = tempfile::tempdir().unwrap();
        let err = Project::open(dir.path().join("nope")).unwrap_err();
        assert!(matches!(err, CoreError::NotADirectory(_)));
    }

    #[test]
    fn open_corrupt_config_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".apex")).unwrap();
        fs::write(dir.path().join(".apex/project.toml"), "name = ").unwrap();
        let err = Project::open(dir.path()).unwrap_err();
        assert!(matches!(err, CoreError::Parse { .. }));
    }

    #[test]
    fn save_persists_changes_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Project::create(dir.path(), "first", ProjectConfig::default()).unwrap();
        p.name = "renamed".into();
        p.config
            .run_configurations
            .push(RunConfiguration::module("tests", "pytest"));
        p.save().unwrap();

        let reopened = Project::open(dir.path()).unwrap();
        assert_eq!(reopened.name, "renamed");
        assert!(reopened.run_configuration("tests").is_some());
        assert!(reopened.run_configuration("missing").is_none());
    }

    #[test]
    fn toml_layout_is_human_friendly() {
        let dir = tempfile::tempdir().unwrap();
        let p = Project::create(dir.path(), "demo", sample_config()).unwrap();
        let text = fs::read_to_string(p.config_path()).unwrap();
        assert!(text.contains("template = \"django\""), "{text}");
        assert!(text.contains("[[run]]"), "{text}");
        assert!(text.contains("script = \"main.py\""), "{text}");
        assert!(text.contains("module = \"django\""), "{text}");
    }

    #[test]
    fn hand_written_config_parses() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".apex")).unwrap();
        fs::write(
            dir.path().join(".apex/project.toml"),
            r#"
name = "hand"
template = "my-plugin-template"
unknown_future_key = 1

[[run]]
name = "app"
script = "src/app.py"
working_dir = "src"

[run.env]
PORT = "8000"
"#,
        )
        .unwrap();
        let p = Project::open(dir.path()).unwrap();
        assert_eq!(p.name, "hand");
        assert_eq!(p.config.version, CONFIG_VERSION);
        assert_eq!(
            p.config.template,
            TemplateKind::Other("my-plugin-template".into())
        );
        let rc = &p.config.run_configurations[0];
        assert_eq!(rc.target, RunTarget::Script("src/app.py".into()));
        assert_eq!(rc.env.get("PORT").map(String::as_str), Some("8000"));
        assert_eq!(rc.resolved_working_dir(&p.root), p.root.join("src"));
    }

    #[test]
    fn template_kind_ids_round_trip() {
        for kind in TemplateKind::BUILT_IN {
            assert_eq!(TemplateKind::from(kind.id().to_owned()), kind);
            assert_eq!(kind.to_string().parse::<TemplateKind>().unwrap(), kind);
        }
        assert_eq!(
            "custom".parse::<TemplateKind>().unwrap(),
            TemplateKind::Other("custom".into())
        );
    }
}
