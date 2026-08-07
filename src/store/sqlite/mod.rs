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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The two-connection path needs a database that outlives any one
    /// connection, which an in-memory one is not — it belongs to the connection
    /// that opened it, so `open_in_memory` cannot exercise this.
    struct TempDb(PathBuf);

    impl TempDb {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("court-alert-test-{}.db", uuid::Uuid::new_v4())))
        }

        fn path(&self) -> PathBuf {
            self.0.clone()
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }

    /// The reason the connections were split: a venue tick holds the writer for
    /// as long as its transaction runs, and a slash command must not wait for
    /// it. Sharing one connection deadlocks this test rather than failing it,
    /// so the read is given a deadline.
    #[tokio::test]
    async fn a_read_goes_through_while_a_write_is_still_open() {
        let db = TempDb::new();
        let store = Arc::new(SqliteStore::open(db.path()).await.unwrap());
        let (writing, started) = tokio::sync::oneshot::channel();
        let (release, hold) = std::sync::mpsc::channel::<()>();

        let write = tokio::spawn({
            let store = store.clone();
            async move {
                store
                    .with_writer("test_write", move |connection| {
                        let transaction = connection.transaction()?;
                        transaction.execute(
                            "INSERT INTO venue_state (venue_id, initialised_at)
                             VALUES ('zhs-munich', '2026-06-01T00:00:00.000Z')",
                            [],
                        )?;
                        writing.send(()).expect("the test is waiting for this");
                        hold.recv().expect("the test holds the other end");
                        transaction.commit()?;
                        Ok(())
                    })
                    .await
            }
        });

        started.await.expect("the write never started");
        let read = tokio::time::timeout(
            Duration::from_secs(10),
            store.with_reader("test_read", |connection| {
                connection
                    .query_row("SELECT count(*) FROM venue_state", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .context("counting venue state")
            }),
        )
        .await;

        let counted = read
            .expect("the read waited for the open write instead of working through it")
            .unwrap();
        assert_eq!(
            counted, 0,
            "the read saw the snapshot the write started from"
        );

        release.send(()).unwrap();
        write.await.unwrap().unwrap();

        let after = store
            .with_reader("test_read_again", |connection| {
                connection
                    .query_row("SELECT count(*) FROM venue_state", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .context("counting venue state")
            })
            .await
            .unwrap();
        assert_eq!(after, 1, "and the committed row afterwards");
    }
}
