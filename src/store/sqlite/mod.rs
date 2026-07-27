mod bookable_slot_row;
mod bookable_slot_snapshot;
mod schema;
mod subscription_row;
mod subscriptions;

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
    inner: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    fn wrap(conn: Connection) -> Self {
        Self {
            inner: Arc::new(Mutex::new(conn)),
        }
    }

    async fn with_conn<T, F>(&self, op: &'static str, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.inner.clone();
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
        let conn = spawn_blocking(move || -> Result<Connection> {
            let mut conn = Connection::open(&path)
                .with_context(|| format!("opening DB {}", path.display()))?;
            prepare(&mut conn).with_context(|| format!("preparing DB {}", path.display()))?;
            Ok(conn)
        })
        .await
        .context("DB open task panicked")??;
        Ok(Self::wrap(conn))
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
        Ok(Self::wrap(conn))
    }
}

trait DbRepr: Sized {
    type Db;

    fn into_db(self) -> Result<Self::Db>;
    fn from_db(db: Self::Db) -> rusqlite::Result<Self>;
}
