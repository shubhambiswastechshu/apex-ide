//! Error type shared by every part of `apex-db`.

use std::error::Error as StdError;

/// Errors returned by `apex-db`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DbError {
    /// No driver with this id exists in the catalog.
    #[error("unknown database driver `{0}`")]
    UnknownDriver(String),

    /// The driver is in the catalog but has no implementation yet.
    #[error("{name} support is not yet implemented")]
    NotImplemented {
        /// Driver id, for example `postgresql`.
        id: String,
        /// Display name, for example `PostgreSQL`.
        name: &'static str,
    },

    /// A connection URL could not be parsed or built.
    #[error("invalid connection URL: {0}")]
    InvalidUrl(String),

    /// A [`ConnectionConfig`](crate::ConnectionConfig) is missing a field or has a bad value.
    #[error("invalid connection settings: {0}")]
    InvalidConfig(String),

    /// The requested schema does not exist.
    #[error("schema `{0}` not found")]
    SchemaNotFound(String),

    /// The requested table or view does not exist.
    #[error("table `{schema}.{table}` not found")]
    TableNotFound {
        /// Schema the lookup ran in.
        schema: String,
        /// Table or view name.
        table: String,
    },

    /// The running statement was stopped through an
    /// [`InterruptHandle`](crate::InterruptHandle).
    #[error("query cancelled")]
    Interrupted,

    /// The database driver reported an error (connect, query or close).
    #[error("{driver}: {source}")]
    Driver {
        /// Display name of the driver that failed.
        driver: &'static str,
        /// Underlying driver error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
}

impl DbError {
    /// Wraps a driver-specific error.
    pub fn driver(
        driver: &'static str,
        source: impl Into<Box<dyn StdError + Send + Sync>>,
    ) -> Self {
        Self::Driver {
            driver,
            source: source.into(),
        }
    }
}

/// Result alias used across `apex-db`.
pub type Result<T, E = DbError> = std::result::Result<T, E>;
