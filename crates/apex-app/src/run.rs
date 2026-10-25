//! Running Python programs: turning a run configuration (or the current
//! file) into a [`RunRequest`], and streaming a running program's output to
//! the UI thread.

use std::path::{Path, PathBuf};

use apex_core::{Project, RunConfiguration, RunTarget as CoreTarget};
use apex_python::{existing_venv_python, RunEvent, RunHandle, RunRequest, RunTarget, Runner};

use crate::events::{AppEvent, EventSink};

/// What the Run button runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunChoice {
    /// The file in the active editor tab.
    CurrentFile,
    /// A run configuration of the project, by index.
    Configuration(usize),
}

/// The run-configuration drop-down: the project's configurations followed
/// by "Current file".
pub fn choice_labels(project: &Project) -> Vec<String> {
    project
        .config
        .run_configurations
        .iter()
        .map(|c| c.name.clone())
        .chain(std::iter::once(CURRENT_FILE_LABEL.to_owned()))
        .collect()
}

/// Label of the [`RunChoice::CurrentFile`] entry.
pub const CURRENT_FILE_LABEL: &str = "Current file";

/// Maps a drop-down index back to a choice.
pub fn choice_at(project: &Project, index: usize) -> RunChoice {
    if index < project.config.run_configurations.len() {
        RunChoice::Configuration(index)
    } else {
        RunChoice::CurrentFile
    }
}

/// A ready-to-start run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPlan {
    /// Shown above the output, e.g. `main: main.py`.
    pub title: String,
    /// What to spawn.
    pub request: RunRequest,
}

/// Why nothing can be run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunPlanError {
    /// The project has no interpreter and no `.venv`.
    #[error("no Python interpreter configured for this project; create one with `uv venv` in {0}")]
    NoInterpreter(PathBuf),
    /// The project file names an interpreter that does not exist.
    #[error("the project's interpreter {0} does not exist")]
    MissingInterpreter(PathBuf),
    /// "Current file" was chosen but no Python file is open.
    #[error("open a Python file to run it")]
    NoPythonFile,
    /// The configuration index is stale.
    #[error("that run configuration no longer exists")]
    UnknownConfiguration,
}

/// The interpreter a project runs with: the configured one, else an
/// in-project `.venv`/`venv`.
pub fn project_interpreter(project: &Project) -> Result<PathBuf, RunPlanError> {
    match project.interpreter_path() {
        Some(path) if path.is_file() => Ok(path),
        Some(path) => Err(RunPlanError::MissingInterpreter(path)),
        None => existing_venv_python(&project.root)
            .ok_or_else(|| RunPlanError::NoInterpreter(project.root.clone())),
    }
}

/// Works out what to run.
pub fn plan_run(
    project: &Project,
    choice: RunChoice,
    current_file: Option<&Path>,
) -> Result<RunPlan, RunPlanError> {
    let python = project_interpreter(project)?;
    match choice {
        RunChoice::CurrentFile => {
            let file = current_file
                .filter(|f| {
                    f.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e.eq_ignore_ascii_case("py"))
                })
                .ok_or(RunPlanError::NoPythonFile)?;
            let mut request = RunRequest::new(python, RunTarget::Script(file.to_path_buf()));
            request.cwd = Some(project.root.clone());
            Ok(RunPlan {
                title: display_relative(&project.root, file),
                request,
            })
        }
        RunChoice::Configuration(index) => {
            let config = project
                .config
                .run_configurations
                .get(index)
                .ok_or(RunPlanError::UnknownConfiguration)?;
            Ok(plan_configuration(project, python, config))
        }
    }
}

fn plan_configuration(project: &Project, python: PathBuf, config: &RunConfiguration) -> RunPlan {
    let (target, shown) = match &config.target {
        CoreTarget::Script(script) => {
            let path = if script.is_absolute() {
                script.clone()
            } else {
                project.root.join(script)
            };
            let shown = display_relative(&project.root, &path);
            (RunTarget::Script(path), shown)
        }
        CoreTarget::Module(module) => (RunTarget::Module(module.clone()), format!("-m {module}")),
    };
    let mut request = RunRequest::new(python, target);
    request.args = config.args.clone();
    request.cwd = Some(config.resolved_working_dir(&project.root));
    request.env = config
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let args = if config.args.is_empty() {
        String::new()
    } else {
        format!(" {}", config.args.join(" "))
    };
    RunPlan {
        title: format!("{}: {shown}{args}", config.name),
        request,
    }
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// A program started from the IDE.
pub struct RunSession {
    /// Identifies events from this run.
    pub id: u64,
    /// The process; used for Stop and for stdin.
    pub handle: RunHandle,
}

/// Starts `plan` and forwards its output to the UI thread as
/// [`AppEvent::Run`] events tagged with `id`.
pub fn start(plan: &RunPlan, id: u64, sink: &EventSink) -> apex_python::Result<RunSession> {
    let handle = Runner::spawn(plan.request.clone())?;
    let events = handle.events().clone();
    let sink = sink.clone();
    let forward = std::thread::Builder::new()
        .name(format!("run-{id}"))
        .spawn(move || {
            for event in events.iter() {
                let exited = matches!(event, RunEvent::Exited(_));
                sink.send(AppEvent::Run { run_id: id, event });
                if exited {
                    break;
                }
            }
        });
    if let Err(err) = forward {
        // Without a forwarder nobody would ever see the output; stop it.
        let _ = handle.kill();
        return Err(apex_python::PythonError::Spawn {
            program: "output forwarder thread".into(),
            source: err,
        });
    }
    Ok(RunSession { id, handle })
}

/// Text for the line printed when a program ends.
pub fn exit_message(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("Process finished with exit code {code}"),
        None => "Process terminated".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::ProjectConfig;

    fn project_with_venv() -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        let rel = apex_core::project::venv_interpreter_relative(Path::new(".venv"));
        let python = dir.path().join(&rel);
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, "").unwrap();
        let config = ProjectConfig {
            interpreter: Some(rel),
            run_configurations: vec![
                RunConfiguration::script("main", "main.py").with_args(["--fast"]),
                RunConfiguration::module("tests", "pytest").with_env("X", "1"),
            ],
            ..ProjectConfig::default()
        };
        let project = Project::create(dir.path(), "demo", config).unwrap();
        (dir, project)
    }

    #[test]
    fn labels_list_configurations_then_current_file() {
        let (_dir, project) = project_with_venv();
        assert_eq!(
            choice_labels(&project),
            ["main", "tests", CURRENT_FILE_LABEL]
        );
        assert_eq!(choice_at(&project, 1), RunChoice::Configuration(1));
        assert_eq!(choice_at(&project, 2), RunChoice::CurrentFile);
        assert_eq!(choice_at(&project, 99), RunChoice::CurrentFile);
    }

    #[test]
    fn plans_script_and_module_configurations() {
        let (_dir, project) = project_with_venv();
        let plan = plan_run(&project, RunChoice::Configuration(0), None).unwrap();
        assert_eq!(
            plan.request.target,
            RunTarget::Script(project.root.join("main.py"))
        );
        assert_eq!(plan.request.args, ["--fast"]);
        assert_eq!(plan.request.cwd.as_deref(), Some(project.root.as_path()));
        assert_eq!(plan.title, "main: main.py --fast");

        let plan = plan_run(&project, RunChoice::Configuration(1), None).unwrap();
        assert_eq!(plan.request.target, RunTarget::Module("pytest".into()));
        assert_eq!(plan.request.env, [("X".to_owned(), "1".to_owned())]);

        assert_eq!(
            plan_run(&project, RunChoice::Configuration(5), None),
            Err(RunPlanError::UnknownConfiguration)
        );
    }

    #[test]
    fn current_file_must_be_python() {
        let (_dir, project) = project_with_venv();
        let script = project.root.join("pkg").join("tool.py");
        let plan = plan_run(&project, RunChoice::CurrentFile, Some(&script)).unwrap();
        assert_eq!(plan.request.target, RunTarget::Script(script));
        assert!(plan.title.ends_with("tool.py"));
        assert_eq!(
            plan_run(
                &project,
                RunChoice::CurrentFile,
                Some(Path::new("notes.md"))
            ),
            Err(RunPlanError::NoPythonFile)
        );
        assert_eq!(
            plan_run(&project, RunChoice::CurrentFile, None),
            Err(RunPlanError::NoPythonFile)
        );
    }

    #[test]
    fn missing_interpreters_are_explained() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::open(dir.path()).unwrap();
        assert!(matches!(
            project_interpreter(&project),
            Err(RunPlanError::NoInterpreter(_))
        ));

        let config = ProjectConfig {
            interpreter: Some(PathBuf::from("gone/python")),
            ..ProjectConfig::default()
        };
        let project = Project::create(dir.path().join("p"), "p", config).unwrap();
        assert!(matches!(
            project_interpreter(&project),
            Err(RunPlanError::MissingInterpreter(_))
        ));
    }

    #[test]
    fn exit_messages() {
        assert_eq!(exit_message(Some(0)), "Process finished with exit code 0");
        assert_eq!(exit_message(None), "Process terminated");
    }
}
