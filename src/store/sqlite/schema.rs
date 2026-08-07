//! Creates the schema and gates startup on its version.
//!
//! Upgrades to existing databases are not applied here: they live in
//! `sql/migrations` as numbered SQL files and are run by hand with
//! `sqlite3 -bail`. This module only creates a database that does not exist
//! yet, and refuses to start against one that is behind.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

const SCHEMA_VERSION: i64 = 5;

const SCHEMA: &str = include_str!("../../../sql/schema.sql");

pub(super) fn ensure_current(conn: &mut Connection) -> Result<()> {
    let version = schema_version(conn)?;
    if version > SCHEMA_VERSION {
        bail!("database schema version {version} is newer than the supported {SCHEMA_VERSION}");
    }
    if version < SCHEMA_VERSION && !is_empty(conn)? {
        bail!(
            "database is at schema version {version}, but this build needs {SCHEMA_VERSION}; \
             apply the files in sql/migrations above {version} first, in order, with \
             `sqlite3 -bail <database> < sql/migrations/<file>.sql`"
        );
    }

    ensure_schema(conn)
}

fn ensure_schema(conn: &mut Connection) -> Result<()> {
    let transaction = conn.transaction().context("starting schema creation")?;
    transaction
        .execute_batch(SCHEMA)
        .context("creating schema")?;
    transaction
        .pragma_update(None, "user_version", SCHEMA_VERSION)
        .context("recording schema version")?;
    transaction.commit().context("committing schema creation")
}

fn schema_version(conn: &Connection) -> Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("reading schema version")
}

fn is_empty(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT NOT EXISTS (
             SELECT 1 FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'
         )",
        [],
        |row| row.get(0),
    )
    .context("checking whether the database is empty")
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPGRADE_TO_V1: &str = include_str!("../../../sql/migrations/0001_strict_schema.sql");
    const UPGRADE_TO_V2: &str =
        include_str!("../../../sql/migrations/0002_alert_message_slots.sql");
    const UPGRADE_TO_V3: &str =
        include_str!("../../../sql/migrations/0003_subscription_surface.sql");
    const UPGRADE_TO_V4: &str = include_str!("../../../sql/migrations/0004_multi_venue.sql");
    const UPGRADE_TO_V5: &str = include_str!("../../../sql/migrations/0005_dm_alert_messages.sql");

    const LEGACY_SCHEMA: &str = "
        CREATE TABLE subscriptions (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            provider     TEXT NOT NULL,
            user_id      TEXT NOT NULL,
            weekday      INTEGER,
            on_date      TEXT,
            start_minute INTEGER NOT NULL,
            end_minute   INTEGER NOT NULL,
            courts       TEXT,
            created_at   TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            CHECK ((weekday IS NULL) <> (on_date IS NULL))
        );
        CREATE INDEX subs_user_idx ON subscriptions(provider, user_id);
        CREATE TABLE slot_state (
            product_id   TEXT NOT NULL,
            product_name TEXT NOT NULL,
            start_at     TEXT NOT NULL,
            end_at       TEXT NOT NULL,
            availability INTEGER NOT NULL,
            PRIMARY KEY (product_id, start_at)
        );
    ";

    fn legacy_database() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO subscriptions (id, provider, user_id, weekday, start_minute, end_minute, courts)
             VALUES (7, 'discord', '12345', 2, 1080, 1320, '[\"Court 2\"]');
             INSERT INTO subscriptions (provider, user_id, on_date, start_minute, end_minute)
             VALUES ('discord', '12345', '2026-06-23', 600, 660);
             INSERT INTO slot_state VALUES ('some-product', 'Court 1', '2026-07-13T08:00:00+00:00', '2026-07-13T09:00:00+00:00', 2);",
        )
        .unwrap();
        conn
    }

    /// The full manual upgrade: run the migration files by hand, in order, then
    /// start the app, which creates whatever the migrations deliberately left out.
    fn migrate_by_hand(conn: &mut Connection) {
        conn.execute_batch(UPGRADE_TO_V1).unwrap();
        conn.execute_batch(UPGRADE_TO_V2).unwrap();
        conn.execute_batch(UPGRADE_TO_V3).unwrap();
        conn.execute_batch(UPGRADE_TO_V4).unwrap();
        conn.execute_batch(UPGRADE_TO_V5).unwrap();
        ensure_current(conn).unwrap();
    }

    fn table_sql(conn: &Connection, table: &str) -> Option<String> {
        conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .ok()
    }

    fn is_strict(conn: &Connection, table: &str) -> bool {
        table_sql(conn, table).is_some_and(|sql| sql.contains("STRICT"))
    }

    /// Every object the database defines, with comments and layout stripped so
    /// that only the definitions themselves are compared.
    fn schema_objects(conn: &Connection) -> Vec<(String, String)> {
        let mut statement = conn
            .prepare(
                "SELECT name, coalesce(sql, '') FROM sqlite_master
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                let (name, sql): (String, String) = (row.get(0)?, row.get(1)?);
                Ok((name, normalize(&sql)))
            })
            .unwrap();
        rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }

    fn normalize(sql: &str) -> String {
        sql.lines()
            .map(|line| line.split("--").next().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn fresh_database_gets_the_current_schema() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_current(&mut conn).unwrap();

        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        assert!(is_strict(&conn, "subscriptions"));
        assert!(is_strict(&conn, "bookable_slots"));
        assert!(is_strict(&conn, "alert_message_slots"));
    }

    /// The migrations drop the snapshot cache without recreating it, and stamp
    /// the version, so a hand-run migration leaves the app nothing to upgrade.
    /// Startup has to create the missing table anyway.
    #[test]
    fn hand_migrated_database_still_gets_the_snapshot_table() {
        let mut conn = legacy_database();
        conn.execute_batch(UPGRADE_TO_V1).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 1);
        conn.execute_batch(UPGRADE_TO_V2).unwrap();
        conn.execute_batch(UPGRADE_TO_V3).unwrap();
        conn.execute_batch(UPGRADE_TO_V4).unwrap();
        conn.execute_batch(UPGRADE_TO_V5).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        assert!(table_sql(&conn, "bookable_slots").is_none());
        assert!(table_sql(&conn, "venue_state").is_none());

        ensure_current(&mut conn).unwrap();

        assert!(is_strict(&conn, "bookable_slots"));
        assert!(is_strict(&conn, "venue_state"));
    }

    #[test]
    fn fourth_migration_refuses_to_run_twice() {
        let mut conn = legacy_database();
        migrate_by_hand(&mut conn);

        assert!(conn.execute_batch(UPGRADE_TO_V4).is_err());
    }

    #[test]
    fn fourth_migration_drops_the_snapshot_cache() {
        let mut conn = legacy_database();
        conn.execute_batch(UPGRADE_TO_V1).unwrap();
        conn.execute_batch(UPGRADE_TO_V2).unwrap();
        conn.execute_batch(UPGRADE_TO_V3).unwrap();
        ensure_schema(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO bookable_slots
             (venue_id, court_id, court_name, starts_at, ends_at, available_places)
             VALUES ('zhs-munich', '92db7384-2dec-4888-a92a-4c2b6faac5f7', 'Court 1',
                     '2026-07-13T08:00:00.000Z', '2026-07-13T09:00:00.000Z', 2)",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();

        conn.execute_batch(UPGRADE_TO_V4).unwrap();

        assert!(table_sql(&conn, "bookable_slots").is_none());
    }

    /// The guard inside the migration file, which is what protects a maintainer
    /// running it against a database that no longer needs it.
    #[test]
    fn migration_file_refuses_to_run_on_a_migrated_database() {
        let mut conn = legacy_database();
        migrate_by_hand(&mut conn);

        assert!(conn.execute_batch(UPGRADE_TO_V1).is_err());
        let count: i64 = conn
            .query_row("SELECT count(*) FROM subscriptions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2, "subscriptions were touched by the aborted run");
    }

    #[test]
    fn second_migration_refuses_to_run_twice() {
        let mut conn = legacy_database();
        migrate_by_hand(&mut conn);

        assert!(conn.execute_batch(UPGRADE_TO_V2).is_err());
    }

    #[test]
    fn third_migration_refuses_to_run_twice() {
        let mut conn = legacy_database();
        migrate_by_hand(&mut conn);

        assert!(conn.execute_batch(UPGRADE_TO_V3).is_err());
    }

    #[test]
    fn fifth_migration_refuses_to_run_twice() {
        let mut conn = legacy_database();
        migrate_by_hand(&mut conn);

        assert!(conn.execute_batch(UPGRADE_TO_V5).is_err());
    }

    #[test]
    fn fifth_migration_keeps_recorded_lines_as_channel_messages() {
        let conn = legacy_database();
        conn.execute_batch(UPGRADE_TO_V1).unwrap();
        conn.execute_batch(UPGRADE_TO_V2).unwrap();
        conn.execute_batch(UPGRADE_TO_V3).unwrap();
        conn.execute_batch(UPGRADE_TO_V4).unwrap();
        conn.execute(
            "INSERT INTO alert_message_slots
             (message_id, line_index, court_id, court_name, starts_at, ends_at, struck)
             VALUES ('1408', 0, '92db7384-2dec-4888-a92a-4c2b6faac5f7', 'Court 1',
                     '2026-07-13T08:00:00.000Z', '2026-07-13T09:00:00.000Z', 0)",
            [],
        )
        .unwrap();

        conn.execute_batch(UPGRADE_TO_V5).unwrap();

        let (surface, channel_id, club, court_name): (
            String,
            Option<String>,
            Option<String>,
            String,
        ) = conn
            .query_row(
                "SELECT surface, channel_id, club, court_name FROM alert_message_slots",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(surface, "channel");
        assert_eq!(channel_id, None, "the webhook addresses messages by id");
        assert_eq!(club, None, "channel lines name no club");
        assert_eq!(court_name, "Court 1");
    }

    #[test]
    fn migration_gives_existing_subscriptions_the_filter_subscribe_would_pick() {
        let mut conn = legacy_database();
        migrate_by_hand(&mut conn);

        let mut statement = conn
            .prepare("SELECT courts IS NULL, court_filter FROM subscriptions ORDER BY id")
            .unwrap();
        let rows: Vec<(bool, String)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert_eq!(
            rows,
            vec![(false, "any".to_string()), (true, "clay".to_string())]
        );
    }

    #[test]
    fn migration_back_fills_existing_subscriptions_as_tennis_at_every_venue() {
        let mut conn = legacy_database();
        migrate_by_hand(&mut conn);

        let mut statement = conn
            .prepare("SELECT sport, venue IS NULL FROM subscriptions ORDER BY id")
            .unwrap();
        let rows: Vec<(String, bool)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert_eq!(
            rows,
            vec![("tennis".to_string(), true), ("tennis".to_string(), true)]
        );
    }

    /// The migration file repeats the subscriptions definition from
    /// `schema.sql` so it can also be run standalone; both paths must end up at
    /// the same schema.
    #[test]
    fn migrated_database_matches_a_freshly_created_one() {
        let mut fresh = Connection::open_in_memory().unwrap();
        ensure_current(&mut fresh).unwrap();
        let mut migrated = legacy_database();
        migrate_by_hand(&mut migrated);

        assert_eq!(schema_objects(&migrated), schema_objects(&fresh));
    }

    #[test]
    fn strict_typing_rejects_values_of_the_wrong_type() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_current(&mut conn).unwrap();

        let error = conn
            .execute(
                "INSERT INTO subscriptions
                 (provider, user_id, sport, weekday, start_minute, end_minute, court_filter)
                 VALUES ('discord', '12345', 'tennis', 2, 'half past six', 1320, 'clay')",
                [],
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot store TEXT value in INTEGER column"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn constraints_reject_invalid_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_current(&mut conn).unwrap();

        let rejected = [
            // weekday outside Mon..=Sun
            "INSERT INTO subscriptions
             (provider, user_id, sport, weekday, start_minute, end_minute, court_filter)
             VALUES ('discord', '12345', 'tennis', 9, 1080, 1320, 'clay')",
            // inverted time range
            "INSERT INTO subscriptions
             (provider, user_id, sport, weekday, start_minute, end_minute, court_filter)
             VALUES ('discord', '12345', 'tennis', 2, 1320, 1080, 'clay')",
            // calendar date that does not exist
            "INSERT INTO subscriptions
             (provider, user_id, sport, on_date, start_minute, end_minute, court_filter)
             VALUES ('discord', '12345', 'tennis', '2026-02-30', 1080, 1320, 'clay')",
            // courts is not a JSON array
            "INSERT INTO subscriptions
             (provider, user_id, sport, weekday, start_minute, end_minute, courts, court_filter)
             VALUES ('discord', '12345', 'tennis', 2, 1080, 1320, 'Court 2', 'clay')",
            "INSERT INTO subscriptions
             (provider, user_id, sport, weekday, start_minute, end_minute, court_filter)
             VALUES ('discord', '12345', 'tennis', 2, 1080, 1320, 'grass')",
            "INSERT INTO subscriptions
             (provider, user_id, sport, weekday, start_minute, end_minute, court_filter)
             VALUES ('discord', '12345', 'squash', 2, 1080, 1320, 'any')",
            // timestamp that is not canonical UTC RFC 3339
            "INSERT INTO bookable_slots
             VALUES ('zhs-munich', '123e4567-e89b-12d3-a456-426614174000', 'Court 1',
                     '2026-07-13T08:00:00+00:00', '2026-07-13T09:00:00.000Z', 2)",
            // slot that ends before it starts
            "INSERT INTO bookable_slots
             VALUES ('zhs-munich', '123e4567-e89b-12d3-a456-426614174000', 'Court 1',
                     '2026-07-13T09:00:00.000Z', '2026-07-13T08:00:00.000Z', 2)",
            "INSERT INTO bookable_slots
             VALUES ('', '123e4567-e89b-12d3-a456-426614174000', 'Court 1',
                     '2026-07-13T08:00:00.000Z', '2026-07-13T09:00:00.000Z', 2)",
            // struck outside the 0/1 domain
            "INSERT INTO alert_message_slots
             VALUES ('channel', NULL, '1408', 0, NULL, '123e4567-e89b-12d3-a456-426614174000',
                     'Court 1', '2026-07-13T08:00:00.000Z', '2026-07-13T09:00:00.000Z', 2)",
            // timestamp that is not canonical UTC RFC 3339
            "INSERT INTO alert_message_slots
             VALUES ('channel', NULL, '1408', 0, NULL, '123e4567-e89b-12d3-a456-426614174000',
                     'Court 1', '2026-07-13T08:00:00+00:00', '2026-07-13T09:00:00.000Z', 0)",
            "INSERT INTO alert_message_slots
             VALUES ('email', NULL, '1408', 0, NULL, '123e4567-e89b-12d3-a456-426614174000',
                     'Court 1', '2026-07-13T08:00:00.000Z', '2026-07-13T09:00:00.000Z', 0)",
            "INSERT INTO alert_message_slots
             VALUES ('dm', NULL, '1408', 0, 'ZHS München', '123e4567-e89b-12d3-a456-426614174000',
                     'Court 1', '2026-07-13T08:00:00.000Z', '2026-07-13T09:00:00.000Z', 0)",
            "INSERT INTO alert_message_slots
             VALUES ('channel', '99', '1408', 0, NULL, '123e4567-e89b-12d3-a456-426614174000',
                     'Court 1', '2026-07-13T08:00:00.000Z', '2026-07-13T09:00:00.000Z', 0)",
        ];
        for statement in rejected {
            assert!(
                conn.execute(statement, []).is_err(),
                "accepted invalid row: {statement}"
            );
        }
    }

    #[test]
    fn applying_twice_is_a_no_op() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_current(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO subscriptions
             (provider, user_id, sport, weekday, start_minute, end_minute, court_filter)
             VALUES ('discord', '12345', 'tennis', 2, 1080, 1320, 'clay')",
            [],
        )
        .unwrap();

        ensure_current(&mut conn).unwrap();

        let count: i64 = conn
            .query_row("SELECT count(*) FROM subscriptions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn legacy_database_keeps_subscriptions_and_gains_strict_tables() {
        let mut conn = legacy_database();
        migrate_by_hand(&mut conn);

        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        assert!(is_strict(&conn, "subscriptions"));
        assert!(table_sql(&conn, "subscriptions_legacy").is_none());
        assert!(table_sql(&conn, "slot_state").is_none());

        let (id, courts, created_at): (i64, String, String) = conn
            .query_row(
                "SELECT id, courts, created_at FROM subscriptions WHERE weekday IS NOT NULL",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(id, 7);
        assert_eq!(courts, "[\"Court 2\"]");
        assert!(created_at.ends_with('Z'), "created_at was {created_at}");

        let count: i64 = conn
            .query_row("SELECT count(*) FROM subscriptions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    /// Ids are handles users hold in /list output, so a migration must never
    /// make a retired one available again — including ids whose rows were
    /// deleted before the migration ran, which the copied rows do not account
    /// for on their own.
    #[test]
    fn migration_never_reissues_an_id() {
        let mut conn = legacy_database();
        // The fixture already holds ids 7 and 8; retire two above them.
        conn.execute_batch(
            "INSERT INTO subscriptions (id, provider, user_id, weekday, start_minute, end_minute)
             VALUES (9, 'discord', '12345', 3, 600, 660), (10, 'discord', '12345', 4, 600, 660);
             DELETE FROM subscriptions WHERE id IN (9, 10);",
        )
        .unwrap();

        migrate_by_hand(&mut conn);

        conn.execute(
            "INSERT INTO subscriptions
             (provider, user_id, sport, weekday, start_minute, end_minute, court_filter)
             VALUES ('discord', '12345', 'tennis', 3, 600, 660, 'clay')",
            [],
        )
        .unwrap();
        assert_eq!(
            conn.last_insert_rowid(),
            11,
            "an id belonging to a deleted subscription was handed out again"
        );
    }

    /// The whole point of migrating by hand: starting against a database nobody
    /// migrated has to fail here, not later on a missing table, and has to say
    /// what to run.
    #[test]
    fn unmigrated_database_is_rejected_with_instructions() {
        let mut conn = legacy_database();

        let error = ensure_current(&mut conn).unwrap_err().to_string();

        assert!(error.contains("schema version 0"), "unexpected: {error}");
        assert!(error.contains("sql/migrations"), "unexpected: {error}");
        assert_eq!(
            schema_version(&conn).unwrap(),
            0,
            "a rejected database must be left untouched"
        );
        assert!(table_sql(&conn, "bookable_slots").is_none());
    }

    #[test]
    fn newer_database_is_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert!(ensure_current(&mut conn).is_err());
    }
}
