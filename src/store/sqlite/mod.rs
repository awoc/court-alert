mod alert_message_row;
mod alert_messages;
mod bookable_slot_row;
mod bookable_slot_snapshot;
mod schema;
mod subscription_row;
mod subscriptions;

use alert_message_row::AlertMessageRow;
use bookable_slot_row::BookableSlotRow;
use subscription_row::SubscriptionRow;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::task::spawn_blocking;

/// `journal_mode` is persisted in the database file, the rest apply per connection
const PRAGMAS: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA foreign_keys = ON;
    PRAGMA busy_timeout = 5000;
";

fn prepare(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(PRAGMAS)
        .context("applying connection pragmas")?;
    schema::ensure_current(conn)
}

pub struct SqliteStore {
    writer: Arc<Mutex<Connection>>,
    reader: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    fn wrap(writer: Connection, reader: Connection) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
            reader: Arc::new(Mutex::new(reader)),
        }
    }

    async fn with_writer<T, F>(&self, op: &'static str, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        Self::run(self.writer.clone(), op, f).await
    }

    async fn with_reader<T, F>(&self, op: &'static str, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        Self::run(self.reader.clone(), op, f).await
    }

    async fn run<T, F>(conn: Arc<Mutex<Connection>>, op: &'static str, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        spawn_blocking(move || {
            let mut guard = conn.lock().expect("connection mutex poisoned");
            f(&mut guard)
        })
        .await
        .with_context(|| format!("DB {op} task panicked"))?
    }

    pub async fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating DB directory {}", parent.display()))?;
        }
        let (writer, reader) = spawn_blocking(move || -> Result<(Connection, Connection)> {
            let mut writer = Connection::open(&path)
                .with_context(|| format!("opening DB {}", path.display()))?;
            prepare(&mut writer).with_context(|| format!("preparing DB {}", path.display()))?;
            // Opened second, so the schema it reads is already there.
            let reader = Connection::open(&path)
                .with_context(|| format!("opening DB {} for reads", path.display()))?;
            reader
                .execute_batch(PRAGMAS)
                .with_context(|| format!("applying read pragmas to {}", path.display()))?;
            Ok((writer, reader))
        })
        .await
        .context("DB open task panicked")??;
        Ok(Self::wrap(writer, reader))
    }
}

#[cfg(test)]
impl SqliteStore {
    pub async fn open_in_memory() -> Result<Self> {
        let conn = spawn_blocking(|| -> Result<Connection> {
            let mut conn = Connection::open_in_memory().context("opening in-memory DB")?;
            prepare(&mut conn).context("preparing in-memory DB")?;
            Ok(conn)
        })
        .await
        .context("DB open task panicked")??;

        let shared = Arc::new(Mutex::new(conn));
        Ok(Self {
            writer: shared.clone(),
            reader: shared,
        })
    }
}

trait DbRepr: Sized {
    type Db;

    fn into_db(self) -> Result<Self::Db>;
    fn from_db(db: Self::Db) -> rusqlite::Result<Self>;
}
