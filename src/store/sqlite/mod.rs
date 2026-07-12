mod bookable_slot_row;
mod bookable_slot_snapshot;
mod subscription_row;
mod subscriptions;

use bookable_slot_row::BookableSlotRow;
use subscription_row::SubscriptionRow;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::task::spawn_blocking;

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(include_str!("../../../sql/schema.sql"))
        .context("initializing schema")
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
            let conn = Connection::open(&path)
                .with_context(|| format!("opening DB {}", path.display()))?;
            init_schema(&conn)?;
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
            let conn = Connection::open_in_memory().context("opening in-memory DB")?;
            init_schema(&conn)?;
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
