//! SQLite driver built on `rusqlite` (SQLite is compiled in, no system library needed).

use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::types::ValueRef;
use rusqlite::{Batch, ErrorCode, OpenFlags};

use crate::catalog::{sqlite_info, DriverInfo};
use crate::config::ConnectionConfig;
use crate::driver::{
    ColumnInfo, Connection, DatabaseDriver, InterruptHandle, QueryResult, TableInfo, TableKind,
};
use crate::error::{DbError, Result};
use crate::value::Value;

const DRIVER_NAME: &str = "SQLite";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

fn sqlite_err(e: rusqlite::Error) -> DbError {
    if e.sqlite_error_code() == Some(ErrorCode::OperationInterrupted) {
        return DbError::Interrupted;
    }
    DbError::driver(DRIVER_NAME, e)
}

/// Wraps rusqlite's thread-safe `sqlite3_interrupt` handle.
struct SqliteInterrupt(rusqlite::InterruptHandle);

impl InterruptHandle for SqliteInterrupt {
    fn interrupt(&self) {
        self.0.interrupt();
    }
}

/// The SQLite driver.
///
/// Uses [`ConnectionConfig::file_path`]; [`IN_MEMORY_PATH`](crate::IN_MEMORY_PATH)
/// opens a private in-memory database. Supported options:
///
/// * `read_only` (`true`/`false`, default `false`): open without write access.
/// * `create` (`true`/`false`, default `true`): create the file if it is missing.
///
/// Other options are ignored.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteDriver;

impl DatabaseDriver for SqliteDriver {
    fn info(&self) -> &'static DriverInfo {
        sqlite_info()
    }

    fn connect(&self, config: &ConnectionConfig) -> Result<Box<dyn Connection>> {
        let path = config
            .file_path
            .as_deref()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| DbError::InvalidConfig("SQLite needs a database file path".into()))?;
        let read_only = bool_option(config, "read_only", false)?;
        let create = bool_option(config, "create", true)?;

        let mut flags = OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if read_only {
            flags |= OpenFlags::SQLITE_OPEN_READ_ONLY;
        } else {
            flags |= OpenFlags::SQLITE_OPEN_READ_WRITE;
            if create {
                flags |= OpenFlags::SQLITE_OPEN_CREATE;
            }
        }
        let conn = rusqlite::Connection::open_with_flags(path, flags).map_err(sqlite_err)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(sqlite_err)?;
        Ok(Box::new(SqliteConnection { conn }))
    }
}

fn bool_option(config: &ConnectionConfig, key: &str, default: bool) -> Result<bool> {
    match config
        .options
        .get(key)
        .map(|v| v.trim().to_ascii_lowercase())
    {
        None => Ok(default),
        Some(v) => match v.as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => Err(DbError::InvalidConfig(format!(
                "option `{key}` must be true or false, got `{v}`"
            ))),
        },
    }
}

struct SqliteConnection {
    conn: rusqlite::Connection,
}

/// Quotes an identifier for use in SQL text.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn to_value(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Integer(i),
        ValueRef::Real(r) => Value::Real(r),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Blob(b.to_vec()),
    }
}

impl SqliteConnection {
    fn ensure_schema(&mut self, schema: &str) -> Result<()> {
        if self
            .list_schemas()?
            .iter()
            .any(|s| s.eq_ignore_ascii_case(schema))
        {
            Ok(())
        } else {
            Err(DbError::SchemaNotFound(schema.to_owned()))
        }
    }
}

impl Connection for SqliteConnection {
    fn list_schemas(&mut self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("PRAGMA database_list")
            .map_err(sqlite_err)?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(sqlite_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sqlite_err)?;
        Ok(names)
    }

    fn list_tables(&mut self, schema: &str) -> Result<Vec<TableInfo>> {
        self.ensure_schema(schema)?;
        let sql = format!(
            "SELECT name, type FROM {}.sqlite_master \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
             ORDER BY name",
            quote_ident(schema)
        );
        let mut stmt = self.conn.prepare(&sql).map_err(sqlite_err)?;
        let tables = stmt
            .query_map([], |row| {
                let kind = if row.get::<_, String>(1)? == "view" {
                    TableKind::View
                } else {
                    TableKind::Table
                };
                Ok(TableInfo {
                    schema: schema.to_owned(),
                    name: row.get(0)?,
                    kind,
                })
            })
            .map_err(sqlite_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sqlite_err)?;
        Ok(tables)
    }

    fn list_columns(&mut self, schema: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        self.ensure_schema(schema)?;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT cid, name, type, \"notnull\", dflt_value, pk \
                 FROM pragma_table_info(?1, ?2) ORDER BY cid",
            )
            .map_err(sqlite_err)?;
        let columns = stmt
            .query_map([table, schema], |row| {
                Ok(ColumnInfo {
                    ordinal: usize::try_from(row.get::<_, i64>(0)?).unwrap_or_default(),
                    name: row.get(1)?,
                    data_type: row.get(2)?,
                    nullable: row.get::<_, i64>(3)? == 0,
                    default_value: row.get(4)?,
                    primary_key: row.get::<_, i64>(5)? > 0,
                })
            })
            .map_err(sqlite_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sqlite_err)?;
        if columns.is_empty() {
            return Err(DbError::TableNotFound {
                schema: schema.to_owned(),
                table: table.to_owned(),
            });
        }
        Ok(columns)
    }

    fn execute(&mut self, sql: &str, limit: Option<usize>) -> Result<QueryResult> {
        let started = Instant::now();
        let mut result = QueryResult::default();
        // `sqlite3_changes()` keeps the last DML count across DDL, COMMIT and
        // other statements, and ignores `... RETURNING`; the running total
        // only moves when rows really change, so diff it per statement.
        let mut changes_before = self.conn.total_changes();
        let mut batch = Batch::new(&self.conn, sql);
        while let Some(mut stmt) = batch.next().map_err(sqlite_err)? {
            if stmt.column_count() == 0 {
                stmt.execute([]).map_err(sqlite_err)?;
                let now = self.conn.total_changes();
                result.rows_affected += now.saturating_sub(changes_before);
                changes_before = now;
                continue;
            }
            let columns: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
            let width = columns.len();
            let mut rows = Vec::new();
            let mut truncated = false;
            let mut cursor = stmt.query([]).map_err(sqlite_err)?;
            while let Some(row) = cursor.next().map_err(sqlite_err)? {
                if limit.is_some_and(|max| rows.len() >= max) {
                    truncated = true;
                    break;
                }
                let mut cells = Vec::with_capacity(width);
                for i in 0..width {
                    cells.push(to_value(row.get_ref(i).map_err(sqlite_err)?));
                }
                rows.push(cells);
            }
            drop(cursor);
            // A truncated `... RETURNING` stops early, but SQLite still
            // applies every change when the statement is reset.
            drop(stmt);
            let now = self.conn.total_changes();
            result.rows_affected += now.saturating_sub(changes_before);
            changes_before = now;
            result.columns = columns;
            result.rows = rows;
            result.truncated = truncated;
        }
        result.elapsed = started.elapsed();
        Ok(result)
    }

    fn close(self: Box<Self>) -> Result<()> {
        self.conn.close().map_err(|(_, e)| sqlite_err(e))
    }

    fn interrupt_handle(&self) -> Option<Arc<dyn InterruptHandle>> {
        Some(Arc::new(SqliteInterrupt(self.conn.get_interrupt_handle())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DriverRegistry, IN_MEMORY_PATH};

    fn open(path: &std::path::Path) -> Box<dyn Connection> {
        DriverRegistry::with_builtin()
            .connect(&ConnectionConfig::file("sqlite", path))
            .expect("open sqlite")
    }

    #[test]
    fn driver_info_is_sqlite() {
        assert_eq!(SqliteDriver.info().id, "sqlite");
        assert!(SqliteDriver.info().is_available());
    }

    #[test]
    fn end_to_end_in_tempdir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        let mut conn = open(&path);

        let created = conn
            .execute(
                "CREATE TABLE users (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    score REAL DEFAULT 0.5,
                    avatar BLOB
                 );
                 CREATE VIEW top_users AS SELECT name FROM users WHERE score > 1;",
                None,
            )
            .expect("create");
        assert!(!created.has_result_set());

        let inserted = conn
            .execute(
                "INSERT INTO users (name, score, avatar) VALUES ('ada', 2.0, x'DEADBEEF');
                 INSERT INTO users (name, score) VALUES ('bob', NULL), ('cy', 1.5);",
                None,
            )
            .expect("insert");
        assert_eq!(inserted.rows_affected, 3);

        let q = conn
            .execute(
                "SELECT id, name, score, avatar FROM users ORDER BY id",
                None,
            )
            .expect("select");
        assert_eq!(q.columns, ["id", "name", "score", "avatar"]);
        assert_eq!(q.rows.len(), 3);
        assert!(!q.truncated);
        assert_eq!(
            q.rows[0],
            vec![
                Value::Integer(1),
                Value::from("ada"),
                Value::Real(2.0),
                Value::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            ]
        );
        let shown: Vec<String> = q.rows[1].iter().map(ToString::to_string).collect();
        assert_eq!(shown, ["2", "bob", "NULL", "NULL"]);
        assert_eq!(q.rows[0][3].to_string(), "<4 bytes>");

        let limited = conn
            .execute("SELECT name FROM users ORDER BY id", Some(2))
            .expect("limit");
        assert_eq!(limited.rows.len(), 2);
        assert!(limited.truncated);
        let exact = conn
            .execute("SELECT name FROM users", Some(3))
            .expect("limit exact");
        assert!(!exact.truncated);

        let updated = conn
            .execute("UPDATE users SET score = 9 WHERE score IS NULL", None)
            .expect("update");
        assert_eq!(updated.rows_affected, 1);

        // Introspection.
        let schemas = conn.list_schemas().expect("schemas");
        assert!(schemas.iter().any(|s| s == "main"), "{schemas:?}");

        let tables = conn.list_tables("main").expect("tables");
        let names: Vec<(&str, TableKind)> =
            tables.iter().map(|t| (t.name.as_str(), t.kind)).collect();
        // sqlite_sequence (created by AUTOINCREMENT) is hidden.
        assert_eq!(
            names,
            [("top_users", TableKind::View), ("users", TableKind::Table)]
        );

        let cols = conn.list_columns("main", "users").expect("columns");
        assert_eq!(cols.len(), 4);
        assert_eq!(cols[0].name, "id");
        assert!(cols[0].primary_key);
        assert_eq!(cols[0].data_type, "INTEGER");
        assert_eq!(cols[1].name, "name");
        assert!(!cols[1].nullable);
        assert!(!cols[1].primary_key);
        assert_eq!(cols[2].default_value.as_deref(), Some("0.5"));
        assert!(cols[3].nullable);
        assert_eq!(
            cols.iter().map(|c| c.ordinal).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );

        let view_cols = conn
            .list_columns("main", "top_users")
            .expect("view columns");
        assert_eq!(view_cols[0].name, "name");

        assert!(matches!(
            conn.list_columns("main", "missing"),
            Err(DbError::TableNotFound { table, .. }) if table == "missing"
        ));
        assert!(matches!(conn.list_tables("nope"), Err(DbError::SchemaNotFound(s)) if s == "nope"));

        conn.close().expect("close");

        // Data persisted to the file.
        let mut again = open(&path);
        let count = again
            .execute("SELECT count(*) FROM users", None)
            .expect("count");
        assert_eq!(count.rows, [[Value::Integer(3)]]);
        again.close().expect("close again");
    }

    #[test]
    fn attached_databases_are_schemas() {
        let mut conn = open(std::path::Path::new(IN_MEMORY_PATH));
        conn.execute(
            "ATTACH DATABASE ':memory:' AS aux; CREATE TABLE aux.\"odd \"\"name\" (x);",
            None,
        )
        .expect("attach");
        let schemas = conn.list_schemas().expect("schemas");
        assert!(schemas.iter().any(|s| s == "aux"), "{schemas:?}");
        let tables = conn.list_tables("aux").expect("tables");
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "odd \"name");
        assert_eq!(tables[0].schema, "aux");
        let cols = conn.list_columns("aux", "odd \"name").expect("columns");
        assert_eq!(cols[0].name, "x");
        assert_eq!(cols[0].data_type, "");
        assert!(conn.list_tables("main").expect("main").is_empty());
    }

    #[test]
    fn last_result_set_wins_and_empty_sql_is_ok() {
        let mut conn = open(std::path::Path::new(IN_MEMORY_PATH));
        let r = conn
            .execute("SELECT 1 AS a; SELECT 'two' AS b, NULL AS c", None)
            .expect("multi");
        assert_eq!(r.columns, ["b", "c"]);
        assert_eq!(r.rows, [[Value::from("two"), Value::Null]]);
        let empty = conn.execute("  -- just a comment\n", None).expect("empty");
        assert!(!empty.has_result_set());
        assert_eq!(empty.rows_affected, 0);
    }

    #[test]
    fn sql_errors_are_driver_errors() {
        let mut conn = open(std::path::Path::new(IN_MEMORY_PATH));
        let err = conn.execute("SELEC 1", None).expect_err("syntax error");
        assert!(matches!(
            err,
            DbError::Driver {
                driver: "SQLite",
                ..
            }
        ));
        assert!(err.to_string().starts_with("SQLite: "), "{err}");
    }

    #[test]
    fn read_only_and_create_options() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ro.db");
        let registry = DriverRegistry::with_builtin();

        let mut no_create = ConnectionConfig::file("sqlite", &path);
        no_create.options.insert("create".into(), "false".into());
        assert!(registry.connect(&no_create).is_err());
        assert!(!path.exists());

        let mut rw = open(&path);
        rw.execute("CREATE TABLE t (x); INSERT INTO t VALUES (1)", None)
            .expect("setup");
        rw.close().expect("close");

        let mut ro_config = ConnectionConfig::file("sqlite", &path);
        ro_config.options.insert("read_only".into(), "true".into());
        let mut ro = registry.connect(&ro_config).expect("open read-only");
        assert_eq!(
            ro.execute("SELECT x FROM t", None).expect("read").rows,
            [[Value::Integer(1)]]
        );
        assert!(ro.execute("INSERT INTO t VALUES (2)", None).is_err());

        let mut bad = ConnectionConfig::file("sqlite", &path);
        bad.options.insert("read_only".into(), "maybe".into());
        assert!(matches!(
            registry.connect(&bad),
            Err(DbError::InvalidConfig(_))
        ));
    }

    #[test]
    fn rows_affected_counts_only_real_changes() {
        let mut conn = open(std::path::Path::new(IN_MEMORY_PATH));
        conn.execute("CREATE TABLE t (x INTEGER)", None)
            .expect("create");
        let tx = conn
            .execute("BEGIN; INSERT INTO t VALUES (1), (2); COMMIT;", None)
            .expect("transaction");
        assert_eq!(tx.rows_affected, 2, "COMMIT must not repeat the count");
        let ddl = conn
            .execute("CREATE INDEX i ON t (x);", None)
            .expect("index");
        assert_eq!(ddl.rows_affected, 0, "DDL changes no rows");
        let returning = conn
            .execute("UPDATE t SET x = x + 1 RETURNING x", None)
            .expect("returning");
        assert_eq!(returning.rows.len(), 2);
        assert_eq!(returning.rows_affected, 2);
        let limited = conn
            .execute("DELETE FROM t RETURNING x", Some(1))
            .expect("limited returning");
        assert!(limited.truncated);
        assert_eq!(limited.rows_affected, 2);
    }

    #[test]
    fn interrupt_stops_a_running_query() {
        let conn = open(std::path::Path::new(IN_MEMORY_PATH));
        let handle = conn.interrupt_handle().expect("sqlite can interrupt");
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut conn = conn;
            let result = conn.execute(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) \
                 SELECT count(*) FROM c",
                None,
            );
            let _ = tx.send(());
            (result, conn)
        });
        // Interrupt until the endless query gives up (the first call may land
        // before it started).
        let deadline = Instant::now() + Duration::from_secs(20);
        while rx.try_recv().is_err() {
            assert!(Instant::now() < deadline, "query was not interrupted");
            handle.interrupt();
            std::thread::sleep(Duration::from_millis(20));
        }
        let (result, mut conn) = worker.join().expect("worker");
        assert!(matches!(result, Err(DbError::Interrupted)), "{result:?}");
        // The connection stays usable.
        let ok = conn.execute("SELECT 1", None).expect("after interrupt");
        assert_eq!(ok.rows, [[Value::Integer(1)]]);
    }

    #[test]
    fn missing_path_is_invalid_config() {
        let config = ConnectionConfig::new("x", "sqlite");
        assert!(matches!(
            SqliteDriver.connect(&config),
            Err(DbError::InvalidConfig(_))
        ));
    }
}
