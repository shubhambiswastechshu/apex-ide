//! Error type shared by every module of this crate.

use std::io;
use std::path::PathBuf;

/// Errors produced by `apex-python`.
#[derive(Debug, thiserror::Error)]
pub enum PythonError {
    /// A filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted, including the path involved.
        context: String,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// A program could not be started (missing executable, permissions, ...).
    #[error("could not start `{program}`: {source}")]
    Spawn {
        /// The program that failed to start.
        program: String,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// A program ran but exited unsuccessfully.
    #[error("`{command}` failed (exit code {code:?}): {stderr}")]
    CommandFailed {
        /// The command line, for display.
        command: String,
        /// The exit code, or `None` if the process was terminated by a signal.
        code: Option<i32>,
        /// The (possibly truncated) standard error output.
        stderr: String,
    },

    /// A tool command was stopped through its
    /// [`CancelToken`](crate::CancelToken).
    #[error("`{command}` was cancelled")]
    Cancelled {
        /// The command line, for display.
        command: String,
    },

    /// A tool command printed nothing for too long and was stopped.
    #[error("`{command}` printed nothing for {} s and was stopped", idle.as_secs())]
    TimedOut {
        /// The command line, for display.
        command: String,
        /// The idle limit that was exceeded.
        idle: std::time::Duration,
    },

    /// No usable Python interpreter was found.
    #[error("no Python interpreter found{}", requested.as_ref().map(|r| format!(" matching `{r}`")).unwrap_or_default())]
    NoInterpreter {
        /// The version that was requested, if any.
        requested: Option<String>,
    },

    /// The given path does not behave like a Python interpreter.
    #[error("`{0}` is not a working Python interpreter")]
    NotAnInterpreter(PathBuf),

    /// A virtual environment was created but its interpreter is missing.
    #[error("virtual environment was created but no interpreter exists at `{0}`")]
    VenvPythonMissing(PathBuf),

    /// A Python version request was malformed (empty or looks like a flag).
    #[error("invalid Python version request `{0}`")]
    InvalidVersionRequest(String),

    /// A project name cannot be turned into a valid package name.
    #[error("invalid project name `{0}`")]
    InvalidProjectName(String),

    /// Tool output (such as `pip list --format json`) could not be read.
    #[error("could not read tool output: {0}")]
    Parse(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T, E = PythonError> = std::result::Result<T, E>;

impl PythonError {
    /// Builds an [`PythonError::Io`] with a human-readable context.
    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}
