//! The "New Project" pipeline, independent of the UI.
//!
//! [`create_project`] runs every step in order and reports progress through
//! a callback, so the same code drives the New Project screen (on a worker
//! thread) and the `--selftest` command line check:
//!
//! 1. **Scaffold** — the template's starter files (existing files are kept).
//! 2. **Environment** — `<root>/.venv` through uv, or `python -m venv`.
//! 3. **Packages** — the template's packages, installed into the venv.
//! 4. **Post-install** — template commands such as `django startproject`.
//! 5. **Configure** — writes `.apex/project.toml` with the interpreter and
//!    default run configurations.
//!
//! The project file is written last, so a failed attempt can simply be
//! retried: scaffolding skips existing files and the venv is reused.

use std::path::{Path, PathBuf};

use apex_core::project::venv_interpreter_relative;
use apex_core::{CoreError, Project, ProjectConfig, RunConfiguration, TemplateKind};
use std::time::Duration;

use apex_python::{
    create_venv_with, find_uv, install_packages_with, CancelToken, PostInstallCommand,
    ProjectTemplate, PythonError, RunEvent, RunRequest, RunTarget, Runner, ToolOptions, VenvSpec,
    VENV_DIR_NAME,
};

/// Characters that are not allowed in a project folder name on any
/// supported platform (the Windows set, applied everywhere so projects can
/// move between machines).
const FORBIDDEN_NAME_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Which Python the new virtual environment is based on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterpreterChoice {
    /// Let uv find (or download) a Python matching the version request,
    /// such as `"3.12"`. `None` uses uv's default.
    Version(Option<String>),
    /// Base the environment on a specific interpreter.
    Base(PathBuf),
}

/// Everything the New Project screen collects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProjectRequest {
    /// Project name; also the folder name.
    pub name: String,
    /// Parent folder; must be absolute.
    pub location: PathBuf,
    /// Starter template.
    pub template: ProjectTemplate,
    /// Base interpreter for `.venv`.
    pub interpreter: InterpreterChoice,
    /// Whether to install the template's packages (and run its
    /// post-install commands, which depend on them).
    pub install_packages: bool,
}

impl NewProjectRequest {
    /// The folder the project will be created in.
    pub fn project_root(&self) -> PathBuf {
        self.location.join(self.name.trim())
    }
}

/// One stage of [`create_project`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Step {
    /// Writing the template's files.
    Scaffold,
    /// Creating the virtual environment.
    Environment,
    /// Installing packages.
    Packages,
    /// Running template commands.
    PostInstall,
    /// Writing the project configuration.
    Configure,
}

impl Step {
    /// Every step, in execution order.
    pub const ALL: [Step; 5] = [
        Step::Scaffold,
        Step::Environment,
        Step::Packages,
        Step::PostInstall,
        Step::Configure,
    ];

    /// Human-readable name for progress UI.
    pub fn label(self) -> &'static str {
        match self {
            Step::Scaffold => "Create project files",
            Step::Environment => "Create virtual environment",
            Step::Packages => "Install packages",
            Step::PostInstall => "Run template commands",
            Step::Configure => "Save project settings",
        }
    }

    /// Position in [`Step::ALL`].
    pub fn index(self) -> usize {
        Step::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// A progress report from [`create_project`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// A step began.
    Started(Step),
    /// A step completed.
    Finished(Step),
    /// A step was not needed; the string says why.
    Skipped(Step, String),
    /// One line of detail (file created, tool output, ...).
    Line(String),
}

/// The result of a successful [`create_project`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedProject {
    /// The project folder.
    pub root: PathBuf,
    /// The file to open first (absolute).
    pub entry_file: PathBuf,
    /// The venv interpreter (absolute).
    pub interpreter: PathBuf,
}

/// Why a project could not be created.
#[derive(Debug, thiserror::Error)]
pub enum FlowError {
    /// The name is empty or only whitespace.
    #[error("enter a project name")]
    EmptyName,
    /// The name cannot be used as a folder name.
    #[error("project name {name:?} cannot contain {reason}")]
    InvalidName {
        /// The rejected name.
        name: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The location is not an absolute path.
    #[error("location must be an absolute folder path, got {0:?}")]
    RelativeLocation(PathBuf),
    /// The target path exists and is a file.
    #[error("{0} exists and is not a folder")]
    NotADirectory(PathBuf),
    /// The target folder already holds an Apex project.
    #[error("{0} already contains an Apex project; open it instead")]
    ProjectExists(PathBuf),
    /// The template creates its files with its packages (for example
    /// `django startproject`), so they cannot be skipped.
    #[error("the {0} template needs its packages; turn on \"Install packages\"")]
    PackagesRequired(&'static str),
    /// The user cancelled the creation.
    #[error("cancelled")]
    Cancelled,
    /// A post-install command is not in a form this pipeline can run.
    #[error("cannot run template command {0:?}")]
    UnsupportedCommand(Vec<String>),
    /// A post-install command exited unsuccessfully.
    #[error("{description} failed ({status})")]
    CommandFailed {
        /// What the command was doing.
        description: String,
        /// Exit code, or a note that the process was killed.
        status: String,
    },
    /// Creating or writing the project failed.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// Python tooling (scaffold, venv, pip/uv) failed.
    #[error(transparent)]
    Python(#[from] PythonError),
    /// A filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// Maps the Python crate's template to the id stored in the project file.
pub fn template_kind(template: ProjectTemplate) -> TemplateKind {
    match template {
        ProjectTemplate::Pure => TemplateKind::PurePython,
        ProjectTemplate::Django => TemplateKind::Django,
        ProjectTemplate::DataScience => TemplateKind::DataScience,
        ProjectTemplate::DataAnalytics => TemplateKind::DataAnalytics,
    }
}

/// Checks a request without touching the filesystem beyond `stat` calls.
/// Returns the project root on success.
pub fn validate(request: &NewProjectRequest) -> Result<PathBuf, FlowError> {
    let name = request.name.trim();
    if name.is_empty() {
        return Err(FlowError::EmptyName);
    }
    if name == "." || name == ".." {
        return Err(FlowError::InvalidName {
            name: name.to_owned(),
            reason: "only dots",
        });
    }
    if name.chars().any(|c| FORBIDDEN_NAME_CHARS.contains(&c)) {
        return Err(FlowError::InvalidName {
            name: name.to_owned(),
            reason: "any of < > : \" / \\ | ? *",
        });
    }
    if name.chars().any(char::is_control) {
        return Err(FlowError::InvalidName {
            name: name.to_owned(),
            reason: "control characters",
        });
    }
    if name.ends_with('.') {
        return Err(FlowError::InvalidName {
            name: name.to_owned(),
            reason: "a trailing dot",
        });
    }
    if !request.location.is_absolute() {
        return Err(FlowError::RelativeLocation(request.location.clone()));
    }
    let root = request.project_root();
    if root.exists() && !root.is_dir() {
        return Err(FlowError::NotADirectory(root));
    }
    if Project::is_project(&root) {
        return Err(FlowError::ProjectExists(root));
    }
    if !request.install_packages && !request.template.post_install_commands(&root).is_empty() {
        return Err(FlowError::PackagesRequired(request.template.display_name()));
    }
    Ok(root)
}

/// The run configurations a new project starts with.
pub fn default_run_configurations(template: ProjectTemplate) -> Vec<RunConfiguration> {
    match template {
        ProjectTemplate::Pure => vec![RunConfiguration::script("main", template.entry_file())],
        ProjectTemplate::Django => vec![
            RunConfiguration::script("Django server", template.entry_file())
                .with_args(["runserver"]),
            RunConfiguration::script("Django check", template.entry_file()).with_args(["check"]),
        ],
        ProjectTemplate::DataScience | ProjectTemplate::DataAnalytics => {
            vec![RunConfiguration::script("analysis", template.entry_file())]
        }
    }
}

/// Turns a template's `python_args` into a [`RunRequest`] for `python`.
///
/// Supports `-m <module> args...` and `<script.py> args...`.
pub fn post_install_request(
    python: &Path,
    root: &Path,
    command: &PostInstallCommand,
) -> Result<RunRequest, FlowError> {
    let args = &command.python_args;
    let (target, rest) = match args.as_slice() {
        [flag, module, rest @ ..] if flag == "-m" => (RunTarget::Module(module.clone()), rest),
        [script, rest @ ..] if script.ends_with(".py") => {
            (RunTarget::Script(root.join(script)), rest)
        }
        _ => return Err(FlowError::UnsupportedCommand(args.clone())),
    };
    let mut request = RunRequest::new(python, target);
    request.args = rest.to_vec();
    request.cwd = Some(root.to_path_buf());
    Ok(request)
}

/// Runs the whole pipeline. Blocks (possibly for minutes while uv downloads
/// Python or packages); call it from a worker thread.
///
/// Tool output is streamed to `progress` as [`Progress::Line`]s. Cancelling
/// `cancel` kills the running tool and fails with [`FlowError::Cancelled`];
/// the project file is not written, so the attempt can be retried.
pub fn create_project(
    request: &NewProjectRequest,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(Progress),
) -> Result<CreatedProject, FlowError> {
    let root = validate(request)?;
    let check_cancel = || {
        if cancel.is_cancelled() {
            Err(FlowError::Cancelled)
        } else {
            Ok(())
        }
    };
    let name = request.name.trim();
    let template = request.template;

    // 1. Scaffold.
    progress(Progress::Started(Step::Scaffold));
    std::fs::create_dir_all(&root).map_err(|source| FlowError::Io {
        context: format!("creating {}", root.display()),
        source,
    })?;
    progress(Progress::Line(format!(
        "Project folder: {}",
        root.display()
    )));
    let report = template.scaffold(&root, name)?;
    for path in &report.created {
        progress(Progress::Line(format!("created {}", relative(&root, path))));
    }
    for path in &report.skipped {
        progress(Progress::Line(format!(
            "kept existing {}",
            relative(&root, path)
        )));
    }
    progress(Progress::Finished(Step::Scaffold));

    // 2. Virtual environment.
    progress(Progress::Started(Step::Environment));
    let spec = match &request.interpreter {
        InterpreterChoice::Version(version) => VenvSpec {
            python_version: version.clone(),
            ..VenvSpec::default()
        },
        InterpreterChoice::Base(path) => VenvSpec {
            base_interpreter: Some(path.clone()),
            ..VenvSpec::default()
        },
    };
    match find_uv() {
        Some(uv) => progress(Progress::Line(format!("using uv at {}", uv.display()))),
        None => progress(Progress::Line(
            "uv not found; falling back to `python -m venv`".to_owned(),
        )),
    }
    check_cancel()?;
    let python = {
        let mut on_line = |line: &str| progress(Progress::Line(line.to_owned()));
        let mut options = ToolOptions {
            on_output: Some(&mut on_line),
            cancel: Some(cancel.clone()),
            ..ToolOptions::default()
        };
        create_venv_with(&root, &spec, &mut options).map_err(cancelled)?
    };
    progress(Progress::Line(format!("interpreter: {}", python.display())));
    progress(Progress::Finished(Step::Environment));

    // 3. Packages.
    let packages = template.packages();
    if !request.install_packages {
        progress(Progress::Skipped(Step::Packages, "turned off".to_owned()));
    } else if packages.is_empty() {
        progress(Progress::Skipped(
            Step::Packages,
            "template needs none".to_owned(),
        ));
    } else {
        progress(Progress::Started(Step::Packages));
        progress(Progress::Line(format!(
            "installing {}",
            packages.join(", ")
        )));
        let mut on_line = |line: &str| progress(Progress::Line(line.to_owned()));
        let mut options = ToolOptions {
            on_output: Some(&mut on_line),
            cancel: Some(cancel.clone()),
            ..ToolOptions::default()
        };
        install_packages_with(&python, packages, &mut options).map_err(cancelled)?;
        progress(Progress::Finished(Step::Packages));
    }

    // 4. Post-install commands (they need the packages).
    let commands = template.post_install_commands(&root);
    if commands.is_empty() {
        progress(Progress::Skipped(
            Step::PostInstall,
            "nothing to run".to_owned(),
        ));
    } else if !request.install_packages {
        progress(Progress::Skipped(
            Step::PostInstall,
            "packages were not installed".to_owned(),
        ));
    } else {
        progress(Progress::Started(Step::PostInstall));
        for command in &commands {
            check_cancel()?;
            run_post_install(&python, &root, command, cancel, progress)?;
        }
        progress(Progress::Finished(Step::PostInstall));
    }

    // 5. Project configuration, written last so failures above can be retried.
    check_cancel()?;
    progress(Progress::Started(Step::Configure));
    let config = ProjectConfig {
        interpreter: Some(venv_interpreter_relative(Path::new(VENV_DIR_NAME))),
        template: template_kind(template),
        run_configurations: default_run_configurations(template),
        ..ProjectConfig::default()
    };
    let project = Project::create(&root, name, config)?;
    progress(Progress::Line(format!(
        "wrote {}",
        relative(&root, &project.config_path())
    )));
    progress(Progress::Finished(Step::Configure));

    Ok(CreatedProject {
        entry_file: project.root.join(template.entry_file()),
        interpreter: project.interpreter_path().unwrap_or(python),
        root: project.root,
    })
}

/// Maps a tool cancellation to [`FlowError::Cancelled`].
fn cancelled(e: PythonError) -> FlowError {
    match e {
        PythonError::Cancelled { .. } => FlowError::Cancelled,
        e => FlowError::Python(e),
    }
}

/// How often a running post-install command checks for cancellation.
const CANCEL_POLL: Duration = Duration::from_millis(100);

fn run_post_install(
    python: &Path,
    root: &Path,
    command: &PostInstallCommand,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(Progress),
) -> Result<(), FlowError> {
    progress(Progress::Line(format!(
        "{}: python {}",
        command.description,
        command.python_args.join(" ")
    )));
    let handle = Runner::spawn(post_install_request(python, root, command)?)?;
    let mut status = None;
    loop {
        if cancel.is_cancelled() {
            if let Err(e) = handle.kill() {
                tracing::warn!(%e, "stopping template command");
            }
            return Err(FlowError::Cancelled);
        }
        match handle.events().recv_timeout(CANCEL_POLL) {
            Ok(RunEvent::Stdout(line) | RunEvent::Stderr(line)) => progress(Progress::Line(line)),
            Ok(RunEvent::Exited(code)) => {
                status = Some(code);
                break;
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    match status {
        Some(Some(0)) => Ok(()),
        Some(Some(code)) => Err(FlowError::CommandFailed {
            description: command.description.clone(),
            status: format!("exit code {code}"),
        }),
        Some(None) => Err(FlowError::CommandFailed {
            description: command.description.clone(),
            status: "terminated".to_owned(),
        }),
        None => Err(FlowError::CommandFailed {
            description: command.description.clone(),
            status: "output ended without an exit status".to_owned(),
        }),
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(location: &Path, name: &str) -> NewProjectRequest {
        NewProjectRequest {
            name: name.to_owned(),
            location: location.to_path_buf(),
            template: ProjectTemplate::Pure,
            interpreter: InterpreterChoice::Version(None),
            install_packages: true,
        }
    }

    #[test]
    fn validate_accepts_a_fresh_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = validate(&request(dir.path(), "  demo  ")).unwrap();
        assert_eq!(root, dir.path().join("demo"));
    }

    #[test]
    fn validate_rejects_bad_names() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            validate(&request(dir.path(), "   ")),
            Err(FlowError::EmptyName)
        ));
        for bad in ["a/b", "a\\b", "..", "x:y", "what?", "trailing."] {
            assert!(
                matches!(
                    validate(&request(dir.path(), bad)),
                    Err(FlowError::InvalidName { .. })
                ),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn validate_rejects_relative_location() {
        let err = validate(&request(Path::new("relative/dir"), "demo")).unwrap_err();
        assert!(matches!(err, FlowError::RelativeLocation(_)));
    }

    #[test]
    fn validate_rejects_file_and_existing_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("taken"), "x").unwrap();
        assert!(matches!(
            validate(&request(dir.path(), "taken")),
            Err(FlowError::NotADirectory(_))
        ));

        Project::create(
            dir.path().join("existing"),
            "existing",
            ProjectConfig::default(),
        )
        .unwrap();
        assert!(matches!(
            validate(&request(dir.path(), "existing")),
            Err(FlowError::ProjectExists(_))
        ));
    }

    #[test]
    fn template_kinds_round_trip_through_ids() {
        assert_eq!(
            template_kind(ProjectTemplate::Pure),
            TemplateKind::PurePython
        );
        assert_eq!(template_kind(ProjectTemplate::Django), TemplateKind::Django);
        assert_eq!(
            template_kind(ProjectTemplate::DataScience),
            TemplateKind::DataScience
        );
        assert_eq!(
            template_kind(ProjectTemplate::DataAnalytics),
            TemplateKind::DataAnalytics
        );
        // Every template maps to a dedicated kind that survives the project file.
        for template in ProjectTemplate::ALL {
            let kind = template_kind(template);
            assert!(!matches!(kind, TemplateKind::Other(_)), "{template:?}");
            assert_eq!(TemplateKind::from(kind.id().to_owned()), kind);
        }
    }

    #[test]
    fn default_run_configurations_point_at_entry_files() {
        for template in ProjectTemplate::ALL {
            let configs = default_run_configurations(template);
            assert!(!configs.is_empty());
            for config in configs {
                assert_eq!(
                    config.target,
                    apex_core::RunTarget::Script(PathBuf::from(template.entry_file()))
                );
            }
        }
    }

    #[test]
    fn post_install_request_parses_module_and_script_forms() {
        let root = Path::new("/proj");
        let python = Path::new("/proj/.venv/bin/python");
        let module = PostInstallCommand {
            description: "d".into(),
            python_args: ["-m", "django", "startproject", "config", "."]
                .map(String::from)
                .to_vec(),
        };
        let req = post_install_request(python, root, &module).unwrap();
        assert_eq!(req.target, RunTarget::Module("django".into()));
        assert_eq!(req.args, ["startproject", "config", "."]);
        assert_eq!(req.cwd.as_deref(), Some(root));

        let script = PostInstallCommand {
            description: "d".into(),
            python_args: vec!["setup.py".into(), "x".into()],
        };
        let req = post_install_request(python, root, &script).unwrap();
        assert_eq!(req.target, RunTarget::Script(root.join("setup.py")));
        assert_eq!(req.args, ["x"]);

        let bad = PostInstallCommand {
            description: "d".into(),
            python_args: vec!["-c".into(), "print(1)".into()],
        };
        assert!(matches!(
            post_install_request(python, root, &bad),
            Err(FlowError::UnsupportedCommand(_))
        ));
    }

    #[test]
    fn django_without_packages_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = request(dir.path(), "site");
        req.template = ProjectTemplate::Django;
        req.install_packages = false;
        assert!(matches!(
            validate(&req),
            Err(FlowError::PackagesRequired("Django"))
        ));
        assert!(matches!(
            create_project(&req, &CancelToken::new(), &mut |_| {}),
            Err(FlowError::PackagesRequired(_))
        ));
        assert!(!dir.path().join("site").exists(), "nothing was written");

        // Templates without post-install commands may skip packages.
        req.template = ProjectTemplate::DataScience;
        assert!(validate(&req).is_ok());
        // An existing Django project needs no `startproject`, so it is fine too.
        req.template = ProjectTemplate::Django;
        std::fs::create_dir_all(dir.path().join("site")).unwrap();
        std::fs::write(dir.path().join("site").join("manage.py"), "").unwrap();
        assert!(validate(&req).is_ok());
    }

    #[test]
    fn cancelled_creation_writes_no_project_file() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        let req = request(dir.path(), "stopped");
        assert!(matches!(
            create_project(&req, &cancel, &mut |_| {}),
            Err(FlowError::Cancelled)
        ));
        assert!(!Project::is_project(&req.project_root()));
    }

    #[test]
    fn steps_are_indexed_in_order() {
        for (i, step) in Step::ALL.iter().enumerate() {
            assert_eq!(step.index(), i);
            assert!(!step.label().is_empty());
        }
    }

    /// The real pipeline with uv or `python -m venv`. Skipped when neither
    /// tool is available on this machine.
    #[test]
    fn creates_a_pure_project_end_to_end() {
        if find_uv().is_none() && apex_python::discover_interpreters(None).is_empty() {
            eprintln!("skipping: no uv and no Python found");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut events = Vec::new();
        let created = create_project(&request(dir.path(), "e2e"), &CancelToken::new(), &mut |p| {
            events.push(p)
        })
        .expect("project created");
        assert!(created.entry_file.is_file());
        assert!(created.interpreter.is_file());
        assert!(events.contains(&Progress::Finished(Step::Configure)));

        let project = Project::open(&created.root).unwrap();
        assert_eq!(project.config.template, TemplateKind::PurePython);
        assert_eq!(project.config.run_configurations.len(), 1);
        assert_eq!(
            project.interpreter_path(),
            Some(created.interpreter.clone())
        );

        // Creating it again is refused rather than clobbering the project.
        assert!(matches!(
            create_project(
                &request(dir.path(), "e2e"),
                &CancelToken::new(),
                &mut |_| {}
            ),
            Err(FlowError::ProjectExists(_))
        ));
    }
}
