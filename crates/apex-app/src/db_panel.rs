//! The Databases tool window: the driver catalog with logos, and a SQLite
//! console whose connection lives on its own worker thread (database calls
//! block, and `Box<dyn Connection>` is `Send` but not `Sync`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use apex_db::{
    catalog, Connection, ConnectionConfig, DriverInfo, DriverRegistry, InterruptHandle,
    QueryResult, TableInfo, TableKind, IN_MEMORY_PATH,
};
use crossbeam_channel::{unbounded, Sender};

use crate::events::{AppEvent, EventSink};

/// Most rows a query shows.
pub const ROW_LIMIT: usize = 1_000;
/// Longest text shown in one grid cell.
const CELL_CHARS: usize = 120;

/// A request for the database worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbCommand {
    /// Open a SQLite file (or `:memory:`), replacing any open connection.
    Connect {
        /// The database file.
        path: PathBuf,
        /// Create the file if it does not exist (otherwise that is an error,
        /// so a mistyped path never silently makes an empty database).
        create: bool,
    },
    /// Run SQL on the open connection.
    Execute(String),
    /// Close the open connection.
    Disconnect,
}

/// The worker's answer to a [`DbCommand`].
#[derive(Debug)]
pub enum DbReply {
    /// Connected; `label` names the database.
    Connected {
        /// Shown in the console title.
        label: String,
        /// Tables and views of the main schema.
        tables: Vec<TableInfo>,
    },
    /// Query results (tables are refreshed because SQL may change them).
    Executed {
        /// The result.
        result: QueryResult,
        /// Fresh table list.
        tables: Vec<TableInfo>,
    },
    /// The connection was closed.
    Disconnected,
    /// The command failed.
    Failed(String),
}

/// The open connection's interrupt handle, shared between the worker
/// (which sets it) and the UI (which uses it to cancel a running query).
#[derive(Clone, Default)]
pub struct InterruptSlot(Arc<Mutex<Option<Arc<dyn InterruptHandle>>>>);

impl InterruptSlot {
    fn set(&self, handle: Option<Arc<dyn InterruptHandle>>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = handle;
    }

    /// Interrupts the statement running on the open connection, if any.
    /// Returns `false` when there is no connection that can be interrupted.
    pub fn interrupt(&self) -> bool {
        let handle = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match handle {
            Some(handle) => {
                handle.interrupt();
                true
            }
            None => false,
        }
    }
}

/// Connection state; owned by the worker thread, but usable directly in
/// tests.
#[derive(Default)]
pub struct DbSession {
    registry: DriverRegistry,
    connection: Option<Box<dyn Connection>>,
    interrupt: InterruptSlot,
}

impl DbSession {
    /// The slot through which another thread can cancel this session's
    /// running query.
    pub fn interrupt_slot(&self) -> InterruptSlot {
        self.interrupt.clone()
    }

    /// Runs one command to completion.
    pub fn handle(&mut self, command: DbCommand) -> DbReply {
        match command {
            DbCommand::Connect { path, create } => {
                self.close();
                let config = match sqlite_config(&path, create) {
                    Ok(c) => c,
                    Err(e) => return DbReply::Failed(e),
                };
                match self.registry.connect(&config) {
                    Ok(mut connection) => {
                        let tables = list_tables(connection.as_mut());
                        self.interrupt.set(connection.interrupt_handle());
                        self.connection = Some(connection);
                        DbReply::Connected {
                            label: config.name.clone(),
                            tables,
                        }
                    }
                    Err(e) => DbReply::Failed(e.to_string()),
                }
            }
            DbCommand::Execute(sql) => {
                let Some(connection) = self.connection.as_mut() else {
                    return DbReply::Failed("not connected".into());
                };
                if sql.trim().is_empty() {
                    return DbReply::Failed("enter a SQL statement".into());
                }
                match connection.execute(&sql, Some(ROW_LIMIT)) {
                    Ok(result) => DbReply::Executed {
                        tables: list_tables(connection.as_mut()),
                        result,
                    },
                    Err(e) => DbReply::Failed(e.to_string()),
                }
            }
            DbCommand::Disconnect => {
                self.close();
                DbReply::Disconnected
            }
        }
    }

    fn close(&mut self) {
        self.interrupt.set(None);
        if let Some(connection) = self.connection.take() {
            if let Err(e) = connection.close() {
                tracing::warn!(%e, "closing database connection");
            }
        }
    }
}

fn list_tables(connection: &mut dyn Connection) -> Vec<TableInfo> {
    connection.list_tables("main").unwrap_or_default()
}

/// Builds a SQLite connection config from what the user typed. A missing
/// file is an error unless `create` is set.
pub fn sqlite_config(path: &Path, create: bool) -> Result<ConnectionConfig, String> {
    let text = path.to_string_lossy();
    let text = text.trim();
    if text.is_empty() {
        return Err("enter a database file path".into());
    }
    if text == IN_MEMORY_PATH {
        return Ok(ConnectionConfig::file("sqlite", IN_MEMORY_PATH));
    }
    let path = PathBuf::from(text);
    if !path.is_absolute() {
        return Err(format!("{text} is not an absolute path"));
    }
    if path.is_dir() {
        return Err(format!("{text} is a folder, not a database file"));
    }
    if !create && !path.exists() {
        return Err(format!(
            "{text} does not exist; tick \"Create if missing\" to make a new database"
        ));
    }
    let mut config = ConnectionConfig::file("sqlite", path);
    config.options.insert("create".into(), create.to_string());
    Ok(config)
}

/// The handle the UI keeps; commands run in order on one worker thread.
pub struct DbWorker {
    tx: Sender<DbCommand>,
    interrupt: InterruptSlot,
}

impl DbWorker {
    /// Starts the worker thread. Replies arrive as [`AppEvent::Db`].
    pub fn spawn(sink: EventSink) -> std::io::Result<Self> {
        let (tx, rx) = unbounded::<DbCommand>();
        let mut session = DbSession::default();
        let interrupt = session.interrupt_slot();
        std::thread::Builder::new()
            .name("db-worker".into())
            .spawn(move || {
                for command in rx.iter() {
                    sink.send(AppEvent::Db(session.handle(command)));
                }
                session.close();
            })?;
        Ok(Self { tx, interrupt })
    }

    /// Queues a command. Returns `false` if the worker is gone.
    ///
    /// `Connect` and `Disconnect` first cancel a running query, so they are
    /// never stuck behind it.
    pub fn send(&self, command: DbCommand) -> bool {
        if matches!(command, DbCommand::Connect { .. } | DbCommand::Disconnect) {
            self.interrupt.interrupt();
        }
        self.tx.send(command).is_ok()
    }

    /// Cancels the running query (its reply is a failure). Returns `false`
    /// if no connection is open.
    pub fn cancel(&self) -> bool {
        self.interrupt.interrupt()
    }
}

/// `SELECT * FROM "<table>" LIMIT n` with the name safely quoted.
pub fn select_all_sql(table: &str) -> String {
    format!(
        "SELECT * FROM \"{}\" LIMIT {};",
        table.replace('"', "\"\""),
        200
    )
}

/// Column names and printable cells of a result.
pub fn result_grid(result: &QueryResult) -> (Vec<String>, Vec<Vec<String>>) {
    let rows = result
        .rows
        .iter()
        .map(|row| row.iter().map(|v| v.display_cell(CELL_CHARS)).collect())
        .collect();
    (result.columns.clone(), rows)
}

/// One-line summary of a result.
pub fn result_status(result: &QueryResult) -> String {
    let time = format_duration(result.elapsed);
    if result.has_result_set() {
        let more = if result.truncated {
            format!(" (first {ROW_LIMIT} shown)")
        } else {
            String::new()
        };
        let n = result.rows.len();
        let s = if n == 1 { "" } else { "s" };
        format!("{n} row{s}{more} in {time}")
    } else {
        let n = result.rows_affected;
        let s = if n == 1 { "" } else { "s" };
        format!("{n} row{s} affected in {time}")
    }
}

fn format_duration(d: Duration) -> String {
    if d.as_millis() >= 1 {
        format!("{} ms", d.as_millis())
    } else {
        format!("{} µs", d.as_micros())
    }
}

/// Text for a table entry.
pub fn table_kind_label(kind: TableKind) -> &'static str {
    match kind {
        TableKind::View => "view",
        _ => "table",
    }
}

/// The catalog in display order: available drivers first, then by
/// category order and name.
pub fn sorted_catalog() -> Vec<&'static DriverInfo> {
    let mut drivers: Vec<&'static DriverInfo> = catalog().iter().collect();
    drivers.sort_by_key(|d| {
        let category = apex_db::DriverCategory::ALL
            .iter()
            .position(|c| *c == d.category)
            .unwrap_or(usize::MAX);
        (!d.is_available(), category, d.name)
    });
    drivers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_session_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("my data.db");
        let mut session = DbSession::default();

        match session.handle(DbCommand::Execute("select 1".into())) {
            DbReply::Failed(msg) => assert!(msg.contains("not connected")),
            other => panic!("unexpected {other:?}"),
        }

        match session.handle(DbCommand::Connect {
            path: db.clone(),
            create: false,
        }) {
            DbReply::Failed(msg) => assert!(msg.contains("does not exist"), "{msg}"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(!db.exists(), "a mistyped path must not create a file");

        match session.handle(DbCommand::Connect {
            path: db.clone(),
            create: true,
        }) {
            DbReply::Connected { label, tables } => {
                assert!(label.contains("my data"));
                assert!(tables.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }

        let sql = "CREATE TABLE \"we\"\"ird\" (id INTEGER PRIMARY KEY, name TEXT);\
                   INSERT INTO \"we\"\"ird\" (name) VALUES ('a'), (NULL);";
        match session.handle(DbCommand::Execute(sql.into())) {
            DbReply::Executed { result, tables } => {
                assert_eq!(result.rows_affected, 2);
                assert_eq!(tables.len(), 1);
                assert!(result_status(&result).starts_with("2 rows affected"));
            }
            other => panic!("unexpected {other:?}"),
        }

        match session.handle(DbCommand::Execute(select_all_sql("we\"ird"))) {
            DbReply::Executed { result, .. } => {
                let (columns, rows) = result_grid(&result);
                assert_eq!(columns, ["id", "name"]);
                assert_eq!(rows, [["1", "a"], ["2", "NULL"]]);
                assert!(result_status(&result).starts_with("2 rows in"));
            }
            other => panic!("unexpected {other:?}"),
        }

        match session.handle(DbCommand::Execute("SELEC nonsense".into())) {
            DbReply::Failed(msg) => assert!(!msg.is_empty()),
            other => panic!("unexpected {other:?}"),
        }

        assert!(matches!(
            session.handle(DbCommand::Disconnect),
            DbReply::Disconnected
        ));
        assert!(db.is_file());
    }

    #[test]
    fn config_validation() {
        assert!(sqlite_config(Path::new("  "), true).is_err());
        assert!(sqlite_config(Path::new("relative.db"), true).is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(sqlite_config(dir.path(), true).is_err());
        let memory = sqlite_config(Path::new(":memory:"), false).unwrap();
        assert_eq!(memory.driver_id, "sqlite");
        let missing = dir.path().join("x.db");
        assert!(sqlite_config(&missing, false).is_err());
        let created = sqlite_config(&missing, true).unwrap();
        assert_eq!(
            created.options.get("create").map(String::as_str),
            Some("true")
        );
        std::fs::write(&missing, b"").unwrap();
        let existing = sqlite_config(&missing, false).unwrap();
        assert_eq!(
            existing.options.get("create").map(String::as_str),
            Some("false")
        );
    }

    #[test]
    fn a_running_query_can_be_cancelled_from_another_thread() {
        let mut session = DbSession::default();
        let slot = session.interrupt_slot();
        assert!(!slot.interrupt(), "nothing to interrupt before connecting");
        assert!(matches!(
            session.handle(DbCommand::Connect {
                path: IN_MEMORY_PATH.into(),
                create: false,
            }),
            DbReply::Connected { .. }
        ));
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let reply = session.handle(DbCommand::Execute(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) \
                 SELECT count(*) FROM c;"
                    .into(),
            ));
            let _ = tx.send(());
            (reply, session)
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while rx.try_recv().is_err() {
            assert!(std::time::Instant::now() < deadline, "query never stopped");
            assert!(slot.interrupt());
            std::thread::sleep(Duration::from_millis(20));
        }
        let (reply, mut session) = worker.join().unwrap();
        match reply {
            DbReply::Failed(msg) => assert!(msg.contains("cancelled"), "{msg}"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            session.handle(DbCommand::Disconnect),
            DbReply::Disconnected
        ));
        assert!(!slot.interrupt(), "slot is cleared on disconnect");
    }

    #[test]
    fn catalog_puts_available_drivers_first() {
        let drivers = sorted_catalog();
        assert_eq!(drivers.len(), catalog().len());
        assert!(drivers[0].is_available());
        let first_planned = drivers.iter().position(|d| !d.is_available());
        if let Some(i) = first_planned {
            assert!(drivers[i..].iter().all(|d| !d.is_available()));
        }
    }

    #[test]
    fn select_all_quotes_names() {
        assert_eq!(
            select_all_sql("a\"b"),
            "SELECT * FROM \"a\"\"b\" LIMIT 200;"
        );
    }
}
