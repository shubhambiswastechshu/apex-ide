//! `apex-ide --selftest`: exercises the real non-UI flows end to end in a
//! temporary folder and reports each check. No window is opened, so it
//! also works on machines without a display.

use std::path::Path;
use std::time::{Duration, Instant};

use apex_core::{FileTree, Project, TemplateKind};
use apex_editor::Buffer;
use apex_python::{RunEvent, Runner};

use crate::db_panel::{DbCommand, DbReply, DbSession};
use crate::events;
use crate::project_flow::{self, InterpreterChoice, NewProjectRequest, Progress};
use crate::run::{self, RunChoice};

/// How long the generated program may take.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// Runs every check; returns `true` if all passed.
pub fn run() -> bool {
    let mut report = Report::default();
    let dir = match tempfile::Builder::new().prefix("apex-selftest-").tempdir() {
        Ok(d) => d,
        Err(e) => {
            report.fail("temporary folder", e);
            return report.finish();
        }
    };
    println!("Apex IDE self-test in {}", dir.path().display());

    if let Some(root) = check_new_project(&mut report, dir.path()) {
        check_open_and_tree(&mut report, &root);
        check_edit_and_save(&mut report, &root);
        check_run(&mut report, &root);
    }
    check_themes(&mut report);
    check_database(&mut report);
    check_plugins(&mut report, dir.path());
    report.finish()
}

#[derive(Default)]
struct Report {
    passed: usize,
    failed: usize,
}

impl Report {
    fn pass(&mut self, what: &str, detail: impl std::fmt::Display) {
        self.passed += 1;
        println!("  [ok]   {what}: {detail}");
    }

    fn fail(&mut self, what: &str, why: impl std::fmt::Display) {
        self.failed += 1;
        println!("  [FAIL] {what}: {why}");
    }

    fn check(&mut self, what: &str, ok: bool, detail: impl std::fmt::Display) {
        if ok {
            self.pass(what, detail);
        } else {
            self.fail(what, detail);
        }
    }

    fn finish(self) -> bool {
        println!("{} passed, {} failed", self.passed, self.failed);
        self.failed == 0
    }
}

fn check_new_project(report: &mut Report, location: &Path) -> Option<std::path::PathBuf> {
    let request = NewProjectRequest {
        name: "selftest_project".into(),
        location: location.to_path_buf(),
        template: apex_python::ProjectTemplate::Pure,
        interpreter: InterpreterChoice::Version(None),
        install_packages: true,
    };
    let mut steps = Vec::new();
    let started = Instant::now();
    match project_flow::create_project(&request, &apex_python::CancelToken::new(), &mut |p| {
        if let Progress::Finished(step) = p {
            steps.push(step.label());
        }
    }) {
        Ok(created) => {
            report.pass(
                "new project (Pure Python, uv/venv)",
                format!(
                    "{} steps in {:.1}s, interpreter {}",
                    steps.len(),
                    started.elapsed().as_secs_f32(),
                    created.interpreter.display()
                ),
            );
            report.check(
                "entry file",
                created.entry_file.is_file(),
                created.entry_file.display(),
            );
            Some(created.root)
        }
        Err(e) => {
            report.fail("new project (Pure Python, uv/venv)", e);
            None
        }
    }
}

fn check_open_and_tree(report: &mut Report, root: &Path) {
    match Project::open(root) {
        Ok(p) => report.check(
            "project file",
            p.config.template == TemplateKind::PurePython
                && p.config.run_configurations.len() == 1
                && p.interpreter_path().is_some_and(|i| i.is_file()),
            format!(
                "template {}, {} run configuration(s)",
                p.config.template.id(),
                p.config.run_configurations.len()
            ),
        ),
        Err(e) => report.fail("project file", e),
    }
    match FileTree::build(root) {
        Ok(tree) => {
            let rows = tree.visible_rows(&Default::default());
            let has_main = rows.iter().any(|r| r.name == "main.py");
            let hides_venv = !rows.iter().any(|r| r.name == ".venv");
            report.check(
                "project tree",
                has_main && hides_venv,
                format!(
                    "{} top-level rows, main.py listed, .venv hidden",
                    rows.len()
                ),
            );
        }
        Err(e) => report.fail("project tree", e),
    }
}

fn check_edit_and_save(report: &mut Report, root: &Path) {
    let path = root.join("main.py");
    let result = (|| -> Result<bool, apex_editor::EditorError> {
        let mut buffer = Buffer::open(&path)?;
        let original = buffer.text();
        let edited = format!("{original}\nprint(\"edited by selftest\")\n");
        buffer.apply_full_text(&edited);
        let dirty = buffer.is_dirty();
        buffer.save()?;
        let reopened = Buffer::open(&path)?;
        Ok(dirty && !buffer.is_dirty() && reopened.text() == edited)
    })();
    match result {
        Ok(ok) => report.check("edit and save main.py", ok, "round-trip through Buffer"),
        Err(e) => report.fail("edit and save main.py", e),
    }
}

fn check_run(report: &mut Report, root: &Path) {
    let project = match Project::open(root) {
        Ok(p) => p,
        Err(e) => return report.fail("run main.py", e),
    };
    let plan = match run::plan_run(&project, RunChoice::Configuration(0), None) {
        Ok(p) => p,
        Err(e) => return report.fail("run main.py", e),
    };
    let handle = match Runner::spawn(plan.request) {
        Ok(h) => h,
        Err(e) => return report.fail("run main.py", e),
    };
    let deadline = Instant::now() + RUN_TIMEOUT;
    let mut output = Vec::new();
    let mut exit = None;
    while exit.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        match handle.events().recv_timeout(left) {
            Ok(RunEvent::Stdout(l) | RunEvent::Stderr(l)) => output.push(l),
            Ok(RunEvent::Exited(code)) => exit = Some(code),
            Err(_) => {
                let _ = handle.kill();
                return report.fail("run main.py", "timed out");
            }
        }
    }
    let printed = output.iter().any(|l| l.contains("edited by selftest"));
    report.check(
        "run main.py",
        exit == Some(Some(0)) && printed,
        format!(
            "{} ({} lines of output)",
            run::exit_message(exit.flatten()),
            output.len()
        ),
    );
}

fn check_themes(report: &mut Report) {
    let (registry, errors) = crate::theming::load_registry(None);
    let mut highlighter = match apex_editor::Highlighter::python() {
        Ok(h) => h,
        Err(e) => return report.fail("themes and highlighting", e),
    };
    let runs = highlighter.highlight_line_runs("def f(x):\n    return x  # done\n");
    let mut colored = 0;
    for theme in registry.list() {
        let lines = crate::theming::highlight_lines(theme, &runs);
        if lines.len() == runs.len() {
            colored += 1;
        }
    }
    report.check(
        "themes and highlighting",
        errors.is_empty() && colored == registry.len(),
        format!("{colored} themes color Python source"),
    );
}

fn check_database(report: &mut Report) {
    let mut session = DbSession::default();
    let connected = matches!(
        session.handle(DbCommand::Connect {
            path: apex_db::IN_MEMORY_PATH.into(),
            create: false,
        }),
        DbReply::Connected { .. }
    );
    let reply = session.handle(DbCommand::Execute(
        "CREATE TABLE t(x); INSERT INTO t VALUES (1), (2); SELECT count(*) FROM t;".into(),
    ));
    match reply {
        DbReply::Executed { result, tables } => {
            let count = result
                .rows
                .first()
                .and_then(|r| r.first())
                .map(ToString::to_string);
            report.check(
                "SQLite console",
                connected && count.as_deref() == Some("2") && tables.len() == 1,
                format!("count(*) = {}", count.unwrap_or_default()),
            );
        }
        DbReply::Failed(e) => report.fail("SQLite console", e),
        other => report.fail("SQLite console", format!("unexpected reply {other:?}")),
    }
    let logos = crate::db_panel::sorted_catalog()
        .iter()
        .filter(|d| d.logo_svg().is_some())
        .count();
    report.check(
        "database catalog",
        logos == apex_db::catalog().len(),
        format!("{logos} drivers with logos"),
    );
}

fn check_plugins(report: &mut Report, dir: &Path) {
    let (sink, _queue) = events::channel();
    let plugins_dir = dir.join("plugins");
    let (manager, problems) = crate::plugins::start(sink, Some(&plugins_dir));
    match manager {
        Some(manager) => {
            let commands = crate::plugins::commands(&manager);
            let ran = manager
                .execute_command(apex_plugins::HelloPlugin::SAY_HELLO, &[])
                .is_ok();
            report.check(
                "plugins",
                problems.is_empty() && ran,
                format!(
                    "{} plugin(s), {} command(s)",
                    manager.plugins().count(),
                    commands.len()
                ),
            );
        }
        None => report.fail("plugins", problems.join("; ")),
    }
}
