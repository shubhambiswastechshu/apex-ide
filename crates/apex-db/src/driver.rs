//! Driver and connection traits, query results, and the driver registry.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::catalog::{find_driver, DriverInfo};
use crate::config::ConnectionConfig;
use crate::error::{DbError, Result};
use crate::value::Value;

/// Whether a relation is a table or a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TableKind {
    /// Base table.
    Table,
    /// View.
    View,
}

/// A table or view found by [`Connection::list_tables`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableInfo {
    /// Schema the table lives in.
    pub schema: String,
    /// Table name.
    pub name: String,
    /// Table or view.
    pub kind: TableKind,
}

/// A column found by [`Connection::list_columns`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnInfo {
    /// Zero-based position in the table.
    pub ordinal: usize,
    /// Column name.
    pub name: String,
    /// Declared type as the database reports it (may be empty, for example in SQLite).
    pub data_type: String,
    /// `false` when the column is declared `NOT NULL`.
    pub nullable: bool,
    /// `true` when the column is part of the primary key.
    pub primary_key: bool,
    /// Default value expression, if any.
    pub default_value: Option<String>,
}

/// Output of [`Connection::execute`].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct QueryResult {
    /// Column names of the last row-returning statement. Empty if none returned rows.
    pub columns: Vec<String>,
    /// Rows of the last row-returning statement, each `columns.len()` long.
    pub rows: Vec<Vec<Value>>,
    /// Rows changed by `INSERT` / `UPDATE` / `DELETE` statements, summed.
    pub rows_affected: u64,
    /// Wall time spent executing.
    pub elapsed: Duration,
    /// `true` when more rows existed than the requested limit.
    pub truncated: bool,
}

impl QueryResult {
    /// `true` when the statements returned a result set (possibly with zero rows).
    pub fn has_result_set(&self) -> bool {
        !self.columns.is_empty()
    }
}

/// Stops a statement running on a [`Connection`], from any thread.
///
/// Obtained with [`Connection::interrupt_handle`] before handing the
/// connection to a worker thread, so the UI can cancel a long query.
pub trait InterruptHandle: Send + Sync {
    /// Asks the database to abort the statement running now. The blocked
    /// call returns [`DbError::Interrupted`]. Has no effect when idle.
    fn interrupt(&self);
}

/// A database driver: describes itself and opens connections.
pub trait DatabaseDriver: Send + Sync {
    /// The catalog entry for this driver.
    fn info(&self) -> &'static DriverInfo;

    /// Opens a connection. The config has already passed
    /// [`ConnectionConfig::validate`] when called through [`DriverRegistry::connect`].
    fn connect(&self, config: &ConnectionConfig) -> Result<Box<dyn Connection>>;
}

/// An open database connection.
///
/// Calls block until the database answers, so run them off the UI thread.
/// Connections are `Send` (move one to a worker thread) but not `Sync`.
pub trait Connection: Send {
    /// Names of the schemas (or attached databases) visible on this connection.
    fn list_schemas(&mut self) -> Result<Vec<String>>;

    /// Tables and views in `schema`, sorted by name.
    fn list_tables(&mut self, schema: &str) -> Result<Vec<TableInfo>>;

    /// Columns of `schema.table`, in declaration order.
    fn list_columns(&mut self, schema: &str, table: &str) -> Result<Vec<ColumnInfo>>;

    /// Runs one or more `;`-separated statements.
    ///
    /// Rows come from the last statement that returns rows. `limit` caps how many
    /// rows are fetched; [`QueryResult::truncated`] says whether more existed.
    fn execute(&mut self, sql: &str, limit: Option<usize>) -> Result<QueryResult>;

    /// Closes the connection, reporting any error the database raises on close.
    fn close(self: Box<Self>) -> Result<()>;

    /// A handle that interrupts this connection's running statement from
    /// another thread, or `None` if the driver cannot interrupt.
    fn interrupt_handle(&self) -> Option<Arc<dyn InterruptHandle>> {
        None
    }
}

/// The set of drivers the app can use, keyed by driver id.
///
/// Asking for a driver that is in the [catalog](crate::catalog) but not
/// registered gives [`DbError::NotImplemented`]; an id outside the catalog gives
/// [`DbError::UnknownDriver`].
pub struct DriverRegistry {
    drivers: HashMap<&'static str, Box<dyn DatabaseDriver>>,
}

impl DriverRegistry {
    /// A registry with no drivers.
    pub fn empty() -> Self {
        Self {
            drivers: HashMap::new(),
        }
    }

    /// A registry with every driver this crate implements (currently SQLite).
    pub fn with_builtin() -> Self {
        let mut registry = Self::empty();
        registry.register(Box::new(crate::sqlite::SqliteDriver));
        registry
    }

    /// Adds or replaces a driver, returning the one it replaced.
    pub fn register(&mut self, driver: Box<dyn DatabaseDriver>) -> Option<Box<dyn DatabaseDriver>> {
        self.drivers.insert(driver.info().id, driver)
    }

    /// The driver for `id`.
    pub fn get(&self, id: &str) -> Result<&dyn DatabaseDriver> {
        if let Some(driver) = self.drivers.get(id) {
            return Ok(driver.as_ref());
        }
        match find_driver(id) {
            Some(info) => Err(DbError::NotImplemented {
                id: info.id.to_owned(),
                name: info.name,
            }),
            None => Err(DbError::UnknownDriver(id.to_owned())),
        }
    }

    /// `true` if a driver for `id` is registered.
    pub fn is_registered(&self, id: &str) -> bool {
        self.drivers.contains_key(id)
    }

    /// Validates `config` and opens a connection with its driver.
    pub fn connect(&self, config: &ConnectionConfig) -> Result<Box<dyn Connection>> {
        config.validate()?;
        self.get(&config.driver_id)?.connect(config)
    }
}

impl Default for DriverRegistry {
    /// Same as [`DriverRegistry::with_builtin`].
    fn default() -> Self {
        Self::with_builtin()
    }
}

impl std::fmt::Debug for DriverRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut ids: Vec<_> = self.drivers.keys().collect();
        ids.sort();
        f.debug_struct("DriverRegistry")
            .field("drivers", &ids)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::catalog;

    #[test]
    fn builtin_registry_matches_catalog_status() {
        let registry = DriverRegistry::with_builtin();
        for d in catalog() {
            assert_eq!(registry.is_registered(d.id), d.is_available(), "{}", d.id);
        }
    }

    #[test]
    fn planned_driver_reports_not_implemented() {
        let registry = DriverRegistry::default();
        let mut c = ConnectionConfig::new("pg", "postgresql");
        c.host = Some("localhost".into());
        let err = registry.connect(&c).err().expect("should fail");
        assert!(matches!(&err, DbError::NotImplemented { id, .. } if id == "postgresql"));
        assert_eq!(err.to_string(), "PostgreSQL support is not yet implemented");
    }

    #[test]
    fn unknown_driver_is_reported() {
        let registry = DriverRegistry::default();
        assert!(matches!(registry.get("nope"), Err(DbError::UnknownDriver(id)) if id == "nope"));
    }

    #[test]
    fn invalid_config_fails_before_driver_lookup() {
        let registry = DriverRegistry::default();
        let c = ConnectionConfig::new("pg", "postgresql");
        assert!(matches!(
            registry.connect(&c),
            Err(DbError::InvalidConfig(_))
        ));
    }

    #[test]
    fn registry_and_connections_are_thread_safe() {
        fn send_sync<T: Send + Sync>() {}
        fn send<T: Send + ?Sized>() {}
        send_sync::<DriverRegistry>();
        send::<dyn Connection>();
        send_sync::<Arc<dyn InterruptHandle>>();
    }
}
