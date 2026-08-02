--   sqlite3 -bail data/court-alert.db < sql/migrations/0004_venue_scoped_slots.sql

BEGIN;

-- Refuse to run against anything but a version 3 database: the CHECK fails,
-- the script aborts, and the transaction is rolled back.
CREATE TEMP TABLE migration_guard (expected_version INTEGER CHECK (expected_version = 3));
INSERT INTO migration_guard SELECT user_version FROM pragma_user_version;

-- Dropped rather than rebuilt with the new `venue_id` column: this table is a
-- cache the next poll rewrites in full, and there is no venue to attribute the
-- existing rows to. Startup recreates it, as it did after 0001 dropped
-- `slot_state`.
--
-- The next poll therefore starts from an empty snapshot. With the default
-- `quiet_first_poll = true` that first poll is silent; confirm it is enabled
-- before running this, or the restart alerts on everything currently free.
DROP TABLE IF EXISTS bookable_slots;

-- Left to startup as well, alongside the recreated `bookable_slots`:
--   CREATE TABLE venue_state (...)

PRAGMA user_version = 4;

DROP TABLE migration_guard;

COMMIT;
