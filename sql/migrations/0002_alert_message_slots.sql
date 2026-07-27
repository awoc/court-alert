--   sqlite3 -bail data/court-alert.db < sql/migrations/0002_alert_message_slots.sql

BEGIN;

-- Refuse to run against anything but a version 1 database: the CHECK fails,
-- the script aborts, and the transaction is rolled back.
CREATE TEMP TABLE migration_guard (expected_version INTEGER CHECK (expected_version = 1));
INSERT INTO migration_guard SELECT user_version FROM pragma_user_version;

-- Purely additive: nothing existing is read or rewritten.
CREATE TABLE alert_message_slots (
    message_id TEXT    NOT NULL CHECK (message_id <> ''),
    line_index INTEGER NOT NULL CHECK (line_index >= 0),
    court_id   TEXT    NOT NULL CHECK (length(court_id) = 36),
    court_name TEXT    NOT NULL CHECK (court_name <> ''),
    starts_at  TEXT    NOT NULL CHECK (starts_at IS strftime('%Y-%m-%dT%H:%M:%fZ', starts_at)),
    ends_at    TEXT    NOT NULL CHECK (ends_at IS strftime('%Y-%m-%dT%H:%M:%fZ', ends_at)),
    struck     INTEGER NOT NULL DEFAULT 0 CHECK (struck IN (0, 1)),
    PRIMARY KEY (message_id, line_index),
    CHECK (ends_at > starts_at)
) STRICT, WITHOUT ROWID;

CREATE INDEX alert_message_slots_slot_idx
    ON alert_message_slots (court_id, starts_at) WHERE struck = 0;

PRAGMA user_version = 2;

DROP TABLE migration_guard;

COMMIT;
