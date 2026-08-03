-- Run with: sqlite3 -bail data/court-alert.db < sql/migrations/0004_multi_venue.sql

BEGIN;

CREATE TEMP TABLE migration_guard (expected_version INTEGER CHECK (expected_version = 3));
INSERT INTO migration_guard SELECT user_version FROM pragma_user_version;

-- Existing rows have no venue key, so startup must rebuild this cache.
-- Keep quiet_first_poll enabled to avoid alerting on the rebuilt snapshot.
DROP TABLE IF EXISTS bookable_slots;

ALTER TABLE subscriptions RENAME TO subscriptions_legacy;

CREATE TABLE subscriptions (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    provider     TEXT    NOT NULL CHECK (provider <> ''),
    user_id      TEXT    NOT NULL CHECK (user_id <> ''),
    sport        TEXT    NOT NULL CHECK (sport IN ('tennis', 'padel')),
    venue        TEXT    CHECK (venue IS NULL OR venue <> ''),
    weekday      INTEGER CHECK (weekday BETWEEN 0 AND 6),
    on_date      TEXT    CHECK (on_date IS date(on_date)),
    start_minute INTEGER NOT NULL CHECK (start_minute BETWEEN 0 AND 1440),
    end_minute   INTEGER NOT NULL CHECK (end_minute BETWEEN 0 AND 1440),
    courts       TEXT    CHECK (courts IS NULL OR (json_valid(courts) AND json_type(courts) = 'array')),
    court_filter TEXT    NOT NULL
                         CHECK (court_filter IN ('any', 'clay', 'synthetic', 'indoor', 'outdoor')),
    created_at   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                         CHECK (created_at IS strftime('%Y-%m-%dT%H:%M:%fZ', created_at)),
    CHECK ((weekday IS NULL) <> (on_date IS NULL)),
    CHECK (start_minute < end_minute)
) STRICT;

-- Existing rows came from the tennis reminder command and covered every tennis venue.
INSERT INTO subscriptions
    (id, provider, user_id, sport, venue, weekday, on_date, start_minute, end_minute,
     courts, court_filter, created_at)
SELECT id, provider, user_id, 'tennis', NULL, weekday, on_date, start_minute, end_minute,
       courts,
       CASE surface WHEN 'all' THEN 'any' ELSE surface END,
       created_at
FROM subscriptions_legacy;

-- Preserve retired AUTOINCREMENT ids.
DELETE FROM sqlite_sequence WHERE name = 'subscriptions';
INSERT INTO sqlite_sequence (name, seq)
    SELECT 'subscriptions', seq FROM sqlite_sequence WHERE name = 'subscriptions_legacy';

DROP TABLE subscriptions_legacy;

CREATE INDEX subscriptions_user_idx ON subscriptions (provider, user_id);

-- Startup also creates bookable_slots and venue_state.
PRAGMA user_version = 4;

DROP TABLE migration_guard;

COMMIT;
