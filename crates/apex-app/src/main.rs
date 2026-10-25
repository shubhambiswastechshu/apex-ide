//! Apex IDE: a Python-first IDE with a native Slint UI.
//!
//! ```text
//! apex-ide [PROJECT_FOLDER]   open the IDE (optionally with a project)
//! apex-ide --selftest         run the non-UI flows end to end, exit 0/1
//! ```
//!
//! Module map:
//! - `app`: controller, worker-event routing, themes, databases, plugins
//! - `welcome`, `workspace`: the screens
//! - `project_flow`, `run`, `lsp`, `db_panel`, `plugins`, `theming`,
//!   `editor`: the code editor; UI-independent logic, unit tested
//! - `events`: worker → UI thread messaging
//! - `state`: UI-thread state

slint::include_modules!();

mod app;
mod commands;
mod db_panel;
mod editor;
mod events;
mod lsp;
mod plugins;
mod project_flow;
mod run;
mod screenshot;
mod selftest;
mod state;
mod switcher;
mod theming;
mod tree_view;
mod welcome;
mod workspace;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: apex-ide [PROJECT_FOLDER]
       apex-ide --selftest

Options:
  --screenshot <out.png> <welcome|new-project|workspace> [project] [file...]
               render a screen off-screen to a PNG (no window)
  --selftest   create a project in a temporary folder with the real
               new-project flow, run it, and check the editor, theme,
               database and plugin layers; exits 0 when all pass
  --help       show this help
  --version    show the version

Set RUST_LOG (for example RUST_LOG=debug) to change log output.";

fn main() -> ExitCode {
    init_logging();

    let all: Vec<String> = std::env::args().skip(1).collect();
    if all.first().map(String::as_str) == Some("--screenshot") {
        return match screenshot::Request::parse(&all[1..]).map(screenshot::run) {
            Ok(Ok(())) => ExitCode::SUCCESS,
            Ok(Err(e)) => {
                eprintln!("screenshot failed: {e:#}");
                ExitCode::FAILURE
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(2)
            }
        };
    }

    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    if args.next().is_some() {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    match first.as_ref().and_then(|a| a.to_str()) {
        Some("--selftest") => {
            return if selftest::run() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some("--version" | "-V") => {
            println!("apex-ide {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some(flag) if flag.starts_with('-') => {
            eprintln!("unknown option {flag}\n\n{USAGE}");
            return ExitCode::from(2);
        }
        _ => {}
    }

    let initial = first.map(PathBuf::from).map(|p| {
        if p.is_absolute() {
            p
        } else {
            std::path::absolute(&p).unwrap_or(p)
        }
    });
    match app::run(initial) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("apex-ide: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
