--   sqlite3 -bail data/court-alert.db < sql/migrations/0003_subscription_surface.sql

BEGIN;

-- Refuse to run against anything but a version 2 database: the CHECK fails,
-- the script aborts, and the transaction is rolled back.
CREATE TEMP TABLE migration_guard (expected_version INTEGER CHECK (expected_version = 2));
INSERT INTO migration_guard SELECT user_version FROM pragma_user_version;

-- ALTER TABLE ADD COLUMN would append `surface` after the table constraints and
-- leave a definition a freshly created database never produces, so the table is
-- rebuilt the way 0001 rebuilt it.
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
    surface      TEXT    NOT NULL DEFAULT 'clay' CHECK (surface IN ('all', 'clay', 'synthetic')),
    created_at   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                         CHECK (created_at IS strftime('%Y-%m-%dT%H:%M:%fZ', created_at)),
    CHECK ((weekday IS NULL) <> (on_date IS NULL)),
    CHECK (start_minute < end_minute)
) STRICT;

-- Existing rows are given the surface `/tennis` would resolve for them
-- today: a subscription that names courts means those courts whatever they are
-- made of, and one that names none follows the clay default.
INSERT INTO subscriptions
    (id, provider, user_id, weekday, on_date, start_minute, end_minute, courts, surface, created_at)
SELECT id, provider, user_id, weekday, on_date, start_minute, end_minute, courts,
       CASE WHEN courts IS NULL THEN 'clay' ELSE 'all' END,
       created_at
FROM subscriptions_legacy;

-- Copying ids only advances the new AUTOINCREMENT counter to the highest
-- surviving id, so the legacy counter is carried over to keep ids retired.
DELETE FROM sqlite_sequence WHERE name = 'subscriptions';
INSERT INTO sqlite_sequence (name, seq)
    SELECT 'subscriptions', seq FROM sqlite_sequence WHERE name = 'subscriptions_legacy';

DROP TABLE subscriptions_legacy;

CREATE INDEX subscriptions_user_idx ON subscriptions (provider, user_id);

PRAGMA user_version = 3;

DROP TABLE migration_guard;

COMMIT;
