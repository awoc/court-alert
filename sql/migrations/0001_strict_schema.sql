--   sqlite3 -bail data/court-alert.db < sql/migrations/0001_strict_schema.sql

BEGIN;

-- Refuse to run against anything but a version 0 database: the CHECK fails,
-- the script aborts, and the transaction is rolled back.
CREATE TEMP TABLE migration_guard (expected_version INTEGER CHECK (expected_version = 0));
INSERT INTO migration_guard SELECT user_version FROM pragma_user_version;

-- STRICT cannot be turned on in place, so the table is rebuilt: moved aside,
-- recreated under its own name, and refilled.
ALTER TABLE subscriptions RENAME TO subscriptions_legacy;

CREATE TABLE subscriptions (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    provider     TEXT    NOT NULL CHECK (provider <> ''),
    user_id      TEXT    NOT NULL CHECK (user_id <> ''),
    weekday      INTEGER CHECK (weekday BETWEEN 0 AND 6),
    on_date      TEXT    CHECK (on_date IS date(on_date)),
    start_minute INTEGER NOT NULL CHECK (start_minute BETWEEN 0 AND 1440),
    end_minute   INTEGER NOT NULL CHECK (end_minute BETWEEN 0 AND 1440),
    courts       TEXT    CHECK (courts IS NULL OR (json_valid(courts) AND json_type(courts) = 'array')),
    created_at   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                         CHECK (created_at IS strftime('%Y-%m-%dT%H:%M:%fZ', created_at)),
    -- Exactly one schedule kind: recurring weekday or single date.
    CHECK ((weekday IS NULL) <> (on_date IS NULL)),
    -- Half-open [start_minute, end_minute) within a single day.
    CHECK (start_minute < end_minute)
) STRICT;

-- `created_at` was written with CURRENT_TIMESTAMP ("YYYY-MM-DD HH:MM:SS") and
-- is normalized to the RFC 3339 form the schema now requires. Ids are copied
-- explicitly to keep the handles users see in /list valid.
INSERT INTO subscriptions
    (id, provider, user_id, weekday, on_date, start_minute, end_minute, courts, created_at)
SELECT id, provider, user_id, weekday, on_date, start_minute, end_minute, courts,
       strftime('%Y-%m-%dT%H:%M:%fZ', created_at)
FROM subscriptions_legacy;

-- Copying ids only advances the new AUTOINCREMENT counter to the highest
-- surviving id, so deleting the newest subscriptions before a migration would
-- make their ids available again and let a stale /list or /listall handle act
-- on whatever reused them. Carry the legacy counter over instead.
--
-- sqlite_sequence has no unique index, so INSERT OR REPLACE would append a
-- second row for `subscriptions` and change nothing; the row is replaced by
-- hand. The SELECT yields nothing if the legacy table never held a row, which
-- correctly leaves the counter unset.
DELETE FROM sqlite_sequence WHERE name = 'subscriptions';
INSERT INTO sqlite_sequence (name, seq)
    SELECT 'subscriptions', seq FROM sqlite_sequence WHERE name = 'subscriptions_legacy';

DROP TABLE subscriptions_legacy;

CREATE INDEX subscriptions_user_idx ON subscriptions (provider, user_id);

-- Dropped rather than migrated: its timestamps are in a format the new CHECKs
-- reject, and it is a cache the next poll rebuilds.
DROP TABLE IF EXISTS slot_state;

PRAGMA user_version = 1;

DROP TABLE migration_guard;

COMMIT;
