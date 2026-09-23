-- Run with: sqlite3 -bail data/court-alert.db < sql/migrations/0006_provider_scoped_alert_messages.sql

BEGIN;

CREATE TEMP TABLE migration_guard (expected_version INTEGER CHECK (expected_version = 5));
INSERT INTO migration_guard SELECT user_version FROM pragma_user_version;

ALTER TABLE alert_message_slots RENAME TO alert_message_slots_legacy;

CREATE TABLE alert_message_slots (
    chat_provider TEXT    NOT NULL CHECK (chat_provider <> ''),
    surface       TEXT    NOT NULL CHECK (surface IN ('channel', 'dm')),
    destination   TEXT    NOT NULL, -- Empty means the adapter already knows the destination.
    message_id    TEXT    NOT NULL CHECK (message_id <> ''),
    line_index    INTEGER NOT NULL CHECK (line_index >= 0),
    club          TEXT    CHECK (club IS NULL OR club <> ''),
    court_id      TEXT    NOT NULL CHECK (length(court_id) = 36),
    court_name    TEXT    NOT NULL CHECK (court_name <> ''),
    starts_at     TEXT    NOT NULL CHECK (starts_at IS strftime('%Y-%m-%dT%H:%M:%fZ', starts_at)),
    ends_at       TEXT    NOT NULL CHECK (ends_at IS strftime('%Y-%m-%dT%H:%M:%fZ', ends_at)),
    struck        INTEGER NOT NULL DEFAULT 0 CHECK (struck IN (0, 1)),
    PRIMARY KEY (chat_provider, surface, destination, message_id, line_index),
    CHECK (ends_at > starts_at)
) STRICT, WITHOUT ROWID;

INSERT INTO alert_message_slots
    (chat_provider, surface, destination, message_id, line_index, club,
     court_id, court_name, starts_at, ends_at, struck)
SELECT 'discord', surface, coalesce(channel_id, ''), message_id, line_index, club,
       court_id, court_name, starts_at, ends_at, struck
FROM alert_message_slots_legacy;

DROP TABLE alert_message_slots_legacy;

CREATE INDEX alert_message_slots_slot_idx
    ON alert_message_slots (chat_provider, surface, court_id, starts_at) WHERE struck = 0;

PRAGMA user_version = 6;

DROP TABLE migration_guard;

COMMIT;
