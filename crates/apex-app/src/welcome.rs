//! The Welcome and New Project screens.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use apex_python::{
    discover_interpreters, find_uv, CancelToken, Interpreter, InterpreterKind, ProjectTemplate,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::app::{append_capped, to_i32, App};
use crate::events::{AppEvent, NoticeKind};
use crate::project_flow::{
    create_project, validate, CreatedProject, InterpreterChoice, NewProjectRequest, Progress, Step,
};
use crate::state::Discovery;
use crate::{AppApi, AppWindow, NewProjectApi, RecentProject, StepRow, TemplateCard, WelcomeApi};

/// Screen numbers of `AppApi.screen`.
pub const SCREEN_WELCOME: i32 = 0;
/// The New Project screen.
pub const SCREEN_NEW_PROJECT: i32 = 1;
/// The main IDE.
pub const SCREEN_WORKSPACE: i32 = 2;

/// Most lines kept in the New Project log.
const LOG_CAP: usize = 2_000;

/// Step-row states (see `StepRow.state` in api.slint).
const STEP_PENDING: i32 = 0;
const STEP_RUNNING: i32 = 1;
const STEP_DONE: i32 = 2;
const STEP_SKIPPED: i32 = 3;
const STEP_FAILED: i32 = 4;

/// A free folder name like `untitled`, `untitled1`, ... under `location`.
pub fn unused_project_name(location: &Path) -> String {
    let base = "untitled";
    if !location.join(base).exists() {
        return base.to_owned();
    }
    (1..1000)
        .map(|n| format!("{base}{n}"))
        .find(|name| !location.join(name).exists())
        .unwrap_or_else(|| base.to_owned())
}

/// Drop-down text for an interpreter.
pub fn interpreter_label(interpreter: &Interpreter) -> String {
    format!(
        "{}  ·  {}",
        interpreter.display_name,
        interpreter.path.display()
    )
}

/// The interpreter preselected as the base of a new venv: the first that
/// is not itself a venv (a venv of a venv works, but is fragile).
pub fn default_base_index(interpreters: &[Interpreter]) -> Option<usize> {
    interpreters
        .iter()
        .position(|i| i.kind != InterpreterKind::Venv)
        .or(if interpreters.is_empty() {
            None
        } else {
            Some(0)
        })
}

impl App {
    pub(crate) fn wire_welcome(self: &Rc<Self>, ui: &AppWindow) {
        let api = ui.global::<WelcomeApi>();

        let app = self.clone();
        api.on_new_project(move || app.show_new_project());

        let app = self.clone();
        api.on_open_folder(move |path| {
            let path = path.trim();
            if path.is_empty() {
                return;
            }
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                app.notify(
                    NoticeKind::Error,
                    format!("{} is not an absolute path", path.display()),
                );
                return;
            }
            app.open_project(path);
        });

        let app = self.clone();
        api.on_open_recent(move |i| {
            let path = app
                .state
                .borrow()
                .settings
                .recent_projects
                .get(usize::try_from(i).unwrap_or(usize::MAX))
                .cloned();
            if let Some(path) = path {
                if path.is_dir() {
                    app.open_project(path);
                } else {
                    app.notify(
                        NoticeKind::Error,
                        format!("{} no longer exists; remove it with ✕", path.display()),
                    );
                }
            }
        });

        let app = self.clone();
        api.on_remove_recent(move |i| {
            let removed = {
                let mut st = app.state.borrow_mut();
                match st
                    .settings
                    .recent_projects
                    .get(usize::try_from(i).unwrap_or(usize::MAX))
                    .cloned()
                {
                    Some(path) => st.settings.remove_recent_project(path),
                    None => false,
                }
            };
            if removed {
                app.save_settings();
                app.refresh_recent();
            }
        });
    }

    pub(crate) fn wire_new_project(self: &Rc<Self>, ui: &AppWindow) {
        let api = ui.global::<NewProjectApi>();

        let app = self.clone();
        api.on_changed(move || app.revalidate_new_project());

        let app = self.clone();
        api.on_cancel(move || {
            // While creating, Cancel stops the running tool; the worker then
            // reports the cancellation like any other failure.
            let running = app.state.borrow().new_project.cancel.clone();
            if let Some(cancel) = running {
                if !cancel.is_cancelled() {
                    cancel.cancel();
                    append_capped(
                        &app.create_log,
                        vec![SharedString::from("Cancelling...")],
                        LOG_CAP,
                    );
                }
                return;
            }
            app.with_ui(|ui| ui.global::<AppApi>().set_screen(SCREEN_WELCOME));
            app.refresh_recent();
        });

        let app = self.clone();
        api.on_create(move || app.start_create());
    }

    /// Rebuilds the recent-projects list from settings, then checks on a
    /// worker which folders still exist (never stat paths on the UI thread:
    /// an offline network share can block for tens of seconds).
    pub fn refresh_recent(&self) {
        self.push_recent();
        let paths = self.state.borrow().settings.recent_projects.clone();
        if paths.is_empty() {
            return;
        }
        let sink = self.sink.clone();
        let spawned = std::thread::Builder::new()
            .name("recent-check".into())
            .spawn(move || {
                let missing = paths
                    .into_iter()
                    .map(|p| {
                        let gone = !p.is_dir();
                        (p, gone)
                    })
                    .collect();
                sink.send(AppEvent::RecentChecked(missing));
            });
        if let Err(e) = spawned {
            tracing::warn!(%e, "could not check recent projects");
        }
    }

    pub(crate) fn on_recent_checked(&self, missing: Vec<(PathBuf, bool)>) {
        let changed = {
            let mut st = self.state.borrow_mut();
            let mut changed = false;
            for (path, gone) in missing {
                changed |= st.recent_missing.insert(path, gone) != Some(gone);
            }
            changed
        };
        if changed {
            self.push_recent();
        }
    }

    /// Shows the recent projects with the last known missing flags.
    fn push_recent(&self) {
        let items: Vec<RecentProject> = {
            let st = self.state.borrow();
            st.settings
                .recent_projects
                .iter()
                .map(|p| {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.display().to_string());
                    let initial = name
                        .chars()
                        .next()
                        .map(|c| c.to_uppercase().collect::<String>())
                        .unwrap_or_default();
                    RecentProject {
                        name: name.into(),
                        initial: initial.into(),
                        path: p.display().to_string().into(),
                        missing: st.recent_missing.get(p).copied().unwrap_or(false),
                    }
                })
                .collect()
        };
        self.with_ui(|ui| {
            ui.global::<WelcomeApi>()
                .set_recent(ModelRc::new(VecModel::from(items)));
        });
    }

    /// Fills the New Project form with defaults.
    pub(crate) fn prepare_new_project_defaults(&self) {
        let location = self.state.borrow().settings.default_projects_dir.clone();
        let cards: Vec<TemplateCard> = ProjectTemplate::ALL
            .iter()
            .map(|t| TemplateCard {
                id: t.id().into(),
                name: t.display_name().into(),
                description: t.description().into(),
                packages: t.packages().join(", ").into(),
            })
            .collect();
        self.with_ui(|ui| {
            let api = ui.global::<NewProjectApi>();
            api.set_templates(ModelRc::new(VecModel::from(cards)));
            api.set_location(location.display().to_string().into());
            api.set_name(unused_project_name(&location).into());
        });
        self.revalidate_new_project();
    }

    fn show_new_project(&self) {
        let creating = self.state.borrow().new_project.creating;
        if !creating {
            // Fresh form, but keep the template and environment choices.
            let location = self
                .ui
                .upgrade()
                .map(|ui| PathBuf::from(ui.global::<NewProjectApi>().get_location().as_str()));
            self.with_ui(|ui| {
                let api = ui.global::<NewProjectApi>();
                if let Some(location) = &location {
                    api.set_name(unused_project_name(location).into());
                }
                api.set_failed(false);
                api.set_steps(ModelRc::default());
            });
            self.create_log.clear();
        }
        self.with_ui(|ui| ui.global::<AppApi>().set_screen(SCREEN_NEW_PROJECT));
        self.revalidate_new_project();
        self.start_discovery();
    }

    fn start_discovery(&self) {
        {
            let mut st = self.state.borrow_mut();
            if st.new_project.discovery != Discovery::NotStarted {
                return;
            }
            st.new_project.discovery = Discovery::Running;
        }
        self.with_ui(|ui| {
            let api = ui.global::<NewProjectApi>();
            api.set_discovering(true);
            api.set_uv_status("Looking for uv and installed Python interpreters...".into());
        });
        let sink = self.sink.clone();
        let spawned = std::thread::Builder::new()
            .name("interpreter-discovery".into())
            .spawn(move || {
                let uv = find_uv();
                let found = discover_interpreters(None);
                sink.send(AppEvent::Interpreters { found, uv });
            });
        if let Err(e) = spawned {
            self.on_interpreters(Vec::new(), None);
            self.notify(
                NoticeKind::Error,
                format!("Interpreter discovery failed: {e}"),
            );
        }
    }

    pub(crate) fn on_interpreters(&self, found: Vec<Interpreter>, uv: Option<PathBuf>) {
        let labels: Vec<SharedString> = found.iter().map(|i| interpreter_label(i).into()).collect();
        let default = default_base_index(&found);
        let uv_text = match &uv {
            Some(path) => format!(
                "uv found at {}. It downloads the chosen Python if it is not installed.",
                path.display()
            ),
            None => "uv not found: environments use `python -m venv` and need an installed \
                     interpreter. Install uv from https://docs.astral.sh/uv/ for the fast path."
                .to_owned(),
        };
        let no_uv = uv.is_none();
        {
            let mut st = self.state.borrow_mut();
            st.new_project.interpreters = found;
            st.new_project.discovery = Discovery::Done;
        }
        self.with_ui(|ui| {
            let api = ui.global::<NewProjectApi>();
            api.set_discovering(false);
            api.set_interpreter_labels(ModelRc::new(VecModel::from(labels)));
            api.set_interpreter_index(default.map_or(-1, to_i32));
            api.set_uv_status(uv_text.into());
            // Without uv a version request cannot be honoured; prefer a base.
            if no_uv && default.is_some() {
                api.set_env_mode(1);
            }
        });
        self.revalidate_new_project();
    }

    /// Reads the form into a request (also used for validation).
    fn request_from_form(&self) -> Option<NewProjectRequest> {
        let ui = self.ui.upgrade()?;
        let api = ui.global::<NewProjectApi>();
        let template = usize::try_from(api.get_template_index())
            .ok()
            .and_then(|i| ProjectTemplate::ALL.get(i).copied())
            .unwrap_or_default();
        let interpreter = if api.get_env_mode() == 1 {
            let st = self.state.borrow();
            let chosen = usize::try_from(api.get_interpreter_index())
                .ok()
                .and_then(|i| st.new_project.interpreters.get(i));
            match chosen {
                Some(i) => InterpreterChoice::Base(i.path.clone()),
                None => InterpreterChoice::Version(None),
            }
        } else {
            let versions = api.get_python_versions();
            let version = usize::try_from(api.get_version_index())
                .ok()
                .and_then(|i| versions.row_data(i))
                .map(|v| v.to_string());
            InterpreterChoice::Version(version)
        };
        Some(NewProjectRequest {
            name: api.get_name().to_string(),
            location: PathBuf::from(api.get_location().trim()),
            template,
            interpreter,
            install_packages: api.get_install_packages(),
        })
    }

    fn revalidate_new_project(&self) {
        let Some(request) = self.request_from_form() else {
            return;
        };
        let mut problem = match validate(&request) {
            Ok(_) => String::new(),
            Err(e) => capitalize(&e.to_string()),
        };
        if problem.is_empty() {
            let ui = self.ui.upgrade();
            let wants_base = ui
                .as_ref()
                .is_some_and(|ui| ui.global::<NewProjectApi>().get_env_mode() == 1);
            if wants_base && matches!(request.interpreter, InterpreterChoice::Version(_)) {
                problem = "Pick a base interpreter, or use a new uv environment".to_owned();
            }
        }
        let target = request.project_root().display().to_string();
        self.with_ui(|ui| {
            let api = ui.global::<NewProjectApi>();
            api.set_target_path(target.into());
            api.set_validation(problem.into());
        });
    }

    fn start_create(&self) {
        let Some(request) = self.request_from_form() else {
            return;
        };
        if let Err(e) = validate(&request) {
            self.with_ui(|ui| {
                ui.global::<NewProjectApi>()
                    .set_validation(capitalize(&e.to_string()).into());
            });
            return;
        }
        let cancel = CancelToken::new();
        {
            let mut st = self.state.borrow_mut();
            if st.new_project.creating {
                return;
            }
            st.new_project.creating = true;
            st.new_project.cancel = Some(cancel.clone());
            st.new_project.steps = Step::ALL
                .iter()
                .map(|s| (*s, STEP_PENDING, String::new()))
                .collect();
        }
        self.create_log.clear();
        self.with_ui(|ui| {
            let api = ui.global::<NewProjectApi>();
            api.set_busy(true);
            api.set_failed(false);
        });
        self.push_steps();

        let sink = self.sink.clone();
        let spawned = std::thread::Builder::new()
            .name("new-project".into())
            .spawn(move || {
                let progress_sink = sink.clone();
                let result = create_project(&request, &cancel, &mut |p| {
                    progress_sink.send(AppEvent::CreateProgress(p));
                })
                .map_err(|e| e.to_string());
                sink.send(AppEvent::CreateFinished(result));
            });
        if let Err(e) = spawned {
            self.on_create_finished(Err(format!("could not start a worker thread: {e}")));
        }
    }

    fn push_steps(&self) {
        let rows: Vec<StepRow> = self
            .state
            .borrow()
            .new_project
            .steps
            .iter()
            .map(|(step, state, note)| StepRow {
                label: step.label().into(),
                state: *state,
                note: note.as_str().into(),
            })
            .collect();
        self.with_ui(|ui| {
            ui.global::<NewProjectApi>()
                .set_steps(ModelRc::new(VecModel::from(rows)));
        });
    }

    fn set_step(&self, step: Step, state: i32, note: String) {
        {
            let mut st = self.state.borrow_mut();
            if let Some(entry) = st.new_project.steps.get_mut(step.index()) {
                entry.1 = state;
                entry.2 = note;
            }
        }
        self.push_steps();
    }

    pub(crate) fn on_create_progress(&self, progress: Progress) {
        match progress {
            Progress::Started(step) => self.set_step(step, STEP_RUNNING, String::new()),
            Progress::Finished(step) => self.set_step(step, STEP_DONE, String::new()),
            Progress::Skipped(step, why) => self.set_step(step, STEP_SKIPPED, why),
            Progress::Line(line) => {
                append_capped(&self.create_log, vec![SharedString::from(line)], LOG_CAP);
            }
        }
    }

    pub(crate) fn on_create_finished(&self, result: Result<CreatedProject, String>) {
        {
            let mut st = self.state.borrow_mut();
            st.new_project.creating = false;
            st.new_project.cancel = None;
        }
        self.with_ui(|ui| ui.global::<NewProjectApi>().set_busy(false));
        match result {
            Ok(created) => {
                self.notify(
                    NoticeKind::Success,
                    format!("Created {}", created.root.display()),
                );
                self.with_ui(|ui| ui.global::<NewProjectApi>().set_steps(ModelRc::default()));
                self.open_project(created.root.clone());
                self.open_file(&created.entry_file);
            }
            Err(message) => {
                let running = self
                    .state
                    .borrow()
                    .new_project
                    .steps
                    .iter()
                    .find(|(_, state, _)| *state == STEP_RUNNING)
                    .map(|(step, _, _)| *step);
                if let Some(step) = running {
                    self.set_step(step, STEP_FAILED, String::new());
                }
                append_capped(
                    &self.create_log,
                    vec![SharedString::from(format!("error: {message}"))],
                    LOG_CAP,
                );
                self.with_ui(|ui| ui.global::<NewProjectApi>().set_failed(true));
                self.notify(NoticeKind::Error, format!("Project not created: {message}"));
                self.revalidate_new_project();
            }
        }
    }
}

/// Upper-cases the first letter (error messages start lower-case).
pub(crate) fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unused_names_skip_existing_folders() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unused_project_name(dir.path()), "untitled");
        std::fs::create_dir(dir.path().join("untitled")).unwrap();
        std::fs::create_dir(dir.path().join("untitled1")).unwrap();
        assert_eq!(unused_project_name(dir.path()), "untitled2");
    }

    #[test]
    fn capitalize_first_letter() {
        assert_eq!(capitalize("enter a name"), "Enter a name");
        assert_eq!(capitalize(""), "");
    }

    #[test]
    fn default_base_skips_venvs() {
        let make = |kind| Interpreter {
            path: PathBuf::from("python"),
            version: (3, 12, 0),
            kind,
            display_name: "Python 3.12.0".into(),
        };
        assert_eq!(default_base_index(&[]), None);
        assert_eq!(default_base_index(&[make(InterpreterKind::Venv)]), Some(0));
        assert_eq!(
            default_base_index(&[make(InterpreterKind::Venv), make(InterpreterKind::System)]),
            Some(1)
        );
    }
}
