-- Run with: sqlite3 -bail data/court-alert.db < sql/migrations/0004_venue_scoped_slots.sql

BEGIN;

CREATE TEMP TABLE migration_guard (expected_version INTEGER CHECK (expected_version = 3));
INSERT INTO migration_guard SELECT user_version FROM pragma_user_version;

-- Existing rows have no venue key, so startup must rebuild this cache.
-- Keep quiet_first_poll enabled to avoid alerting on the rebuilt snapshot.
DROP TABLE IF EXISTS bookable_slots;

-- Startup also creates venue_state.
PRAGMA user_version = 4;

DROP TABLE migration_guard;

COMMIT;
