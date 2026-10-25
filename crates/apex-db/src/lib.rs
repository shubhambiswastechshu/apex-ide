//! Database support for Apex IDE.
//!
//! * [`catalog`] lists every database the IDE knows about, with brand colour,
//!   default port and [status](DriverStatus); [`logo_svg`] returns its bundled logo.
//! * [`ConnectionConfig`] is a saved connection. Its password lives in memory
//!   only; settings files store a [`SecretRef`] instead.
//! * [`DriverRegistry`] opens a [`Connection`] for a config. SQLite is implemented;
//!   other catalog entries return [`DbError::NotImplemented`].
//!
//! ```
//! use apex_db::{ConnectionConfig, DriverRegistry, IN_MEMORY_PATH};
//!
//! let registry = DriverRegistry::with_builtin();
//! let mut conn = registry.connect(&ConnectionConfig::file("sqlite", IN_MEMORY_PATH))?;
//! let result = conn.execute("SELECT 1 + 1 AS two", None)?;
//! assert_eq!(result.rows[0][0].to_string(), "2");
//! conn.close()?;
//! # Ok::<(), apex_db::DbError>(())
//! ```

mod catalog;
mod config;
mod driver;
mod error;
mod logos;
mod sqlite;
mod value;

pub use catalog::{
    catalog, drivers_in, find_driver, find_driver_by_scheme, DriverCategory, DriverInfo,
    DriverStatus,
};
pub use config::{
    is_sensitive_option, new_connection_id, ConnectionConfig, Secret, SecretRef, IN_MEMORY_PATH,
};
pub use driver::{
    ColumnInfo, Connection, DatabaseDriver, DriverRegistry, InterruptHandle, QueryResult,
    TableInfo, TableKind,
};
pub use error::{DbError, Result};
pub use logos::logo_svg;
pub use sqlite::SqliteDriver;
pub use value::Value;
