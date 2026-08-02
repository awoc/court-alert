--   sqlite3 -bail data/court-alert.db < sql/migrations/0005_subscription_sport_and_venue.sql

BEGIN;

-- Refuse to run against anything but a version 4 database: the CHECK fails,
-- the script aborts, and the transaction is rolled back.
CREATE TEMP TABLE migration_guard (expected_version INTEGER CHECK (expected_version = 4));
INSERT INTO migration_guard SELECT user_version FROM pragma_user_version;

-- `surface` is renamed to `court_filter` and its CHECK widened to span both
-- sports, so the table is rebuilt the way 0001 and 0003 rebuilt it.
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

-- Every existing subscription came from `/subscribe`, so it is tennis. It is
-- back-filled with `venue = NULL` — "all tennis venues" — rather than an
-- invented venue id: with exactly one ZHS venue that preserves today's
-- behaviour precisely, and it keeps working if a second tennis club appears.
--
-- `'all'` becomes `'any'`: the same meaning under the wider vocabulary, where
-- "all surfaces" would have read as a tennis-only answer.
INSERT INTO subscriptions
    (id, provider, user_id, sport, venue, weekday, on_date, start_minute, end_minute,
     courts, court_filter, created_at)
SELECT id, provider, user_id, 'tennis', NULL, weekday, on_date, start_minute, end_minute,
       courts,
       CASE surface WHEN 'all' THEN 'any' ELSE surface END,
       created_at
FROM subscriptions_legacy;

-- Copying ids only advances the new AUTOINCREMENT counter to the highest
-- surviving id, so the legacy counter is carried over to keep ids retired.
DELETE FROM sqlite_sequence WHERE name = 'subscriptions';
INSERT INTO sqlite_sequence (name, seq)
    SELECT 'subscriptions', seq FROM sqlite_sequence WHERE name = 'subscriptions_legacy';

DROP TABLE subscriptions_legacy;

CREATE INDEX subscriptions_user_idx ON subscriptions (provider, user_id);

PRAGMA user_version = 5;

DROP TABLE migration_guard;

COMMIT;
