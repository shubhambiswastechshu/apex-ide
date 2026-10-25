//! Error type shared by every `apex-core` module.

use std::path::PathBuf;

/// Convenience alias for results produced by `apex-core`.
pub type Result<T> = std::result::Result<T, CoreError>;

/// Everything that can go wrong in `apex-core`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CoreError {
    /// A filesystem operation failed.
    #[error("I/O error at {}: {source}", path.display())]
    Io {
        /// The path the operation was acting on.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A TOML file exists but could not be parsed.
    #[error("could not parse {}: {source}", path.display())]
    Parse {
        /// The file that failed to parse.
        path: PathBuf,
        /// The underlying TOML error (boxed to keep `CoreError` small).
        #[source]
        source: Box<toml::de::Error>,
    },

    /// A value could not be serialized to TOML.
    #[error("could not serialize to TOML: {0}")]
    Serialize(#[from] Box<toml::ser::Error>),

    /// A path that must be a directory is not one (or does not exist).
    #[error("not a directory: {}", .0.display())]
    NotADirectory(PathBuf),

    /// `Project::create` was called on a folder that already holds a project.
    #[error("a project already exists at {}", .0.display())]
    ProjectExists(PathBuf),

    /// A project name was empty or contained characters that are not allowed.
    #[error("invalid project name {0:?}: {1}")]
    InvalidProjectName(String, &'static str),

    /// The operating system did not report a directory Apex IDE needs.
    #[error("the operating system did not provide a {0} directory")]
    MissingSystemDir(&'static str),
}

impl From<toml::ser::Error> for CoreError {
    fn from(e: toml::ser::Error) -> Self {
        Self::Serialize(Box::new(e))
    }
}

impl CoreError {
    /// Builds a [`CoreError::Io`] for `path`.
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Builds a [`CoreError::Parse`] for `path`.
    pub(crate) fn parse(path: impl Into<PathBuf>, source: toml::de::Error) -> Self {
        Self::Parse {
            path: path.into(),
            source: Box::new(source),
        }
    }
}
