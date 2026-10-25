//! Python tooling for Apex IDE.
//!
//! - [`interpreter`]: find installed Python interpreters.
//! - [`venv`]: create virtual environments and install packages (uv first,
//!   `venv`/`pip` as fallback).
//! - [`templates`]: new-project templates (Pure, Django, Data Science,
//!   Data Analytics).
//! - [`runner`]: run scripts/modules with streamed output.
//! - [`packages`]: imports and requirements that are not installed.
//!
//! All process spawning hides the console window on Windows. Functions that
//! spawn processes block; call them from a background thread in UI code.
//!
//! A typical "New Project" flow:
//!
//! ```no_run
//! use apex_python::{create_venv, install_packages, ProjectTemplate, VenvSpec};
//! # fn main() -> apex_python::Result<()> {
//! let root = std::path::Path::new("/projects/shop");
//! let template = ProjectTemplate::Django;
//! template.scaffold(root, "shop")?;
//! let python = create_venv(root, &VenvSpec::default())?;
//! install_packages(&python, template.packages())?;
//! for cmd in template.post_install_commands(root) {
//!     // run `python cmd.python_args...` in `root`, e.g. through `Runner`.
//!     let _ = cmd;
//! }
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

mod error;
pub mod interpreter;
pub mod packages;
mod process;
pub mod runner;
pub mod templates;
pub mod venv;

pub use error::{PythonError, Result};
pub use interpreter::{
    discover_interpreters, find_matching, inspect_interpreter, query_version, Interpreter,
    InterpreterKind,
};
pub use process::{CancelToken, ToolOptions, DEFAULT_TOOL_IDLE_TIMEOUT};
pub use runner::{RunEvent, RunHandle, RunRequest, RunTarget, Runner};
pub use templates::{PostInstallCommand, ProjectTemplate, ScaffoldReport};
pub use venv::{
    create_venv, create_venv_with, existing_venv_python, find_uv, install_packages,
    install_packages_with, python_in_venv, venv_dir, venv_python, VenvBackend, VenvSpec,
    VENV_DIR_NAME,
};
