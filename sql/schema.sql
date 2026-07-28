-- Timestamps are stored as canonical UTC RFC 3339 with milliseconds
-- (YYYY-MM-DDTHH:MM:SS.SSSZ) so that text comparison equals chronological order.

CREATE TABLE IF NOT EXISTS subscriptions (
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
    -- Exactly one schedule kind: recurring weekday or single date.
    CHECK ((weekday IS NULL) <> (on_date IS NULL)),
    -- Half-open [start_minute, end_minute) within a single day.
    CHECK (start_minute < end_minute)
) STRICT;

CREATE INDEX IF NOT EXISTS subscriptions_user_idx ON subscriptions (provider, user_id);

-- Last observed snapshot of bookable slots; rewritten in full on every poll.
CREATE TABLE IF NOT EXISTS bookable_slots (
    court_id         TEXT    NOT NULL CHECK (length(court_id) = 36),
    court_name       TEXT    NOT NULL CHECK (court_name <> ''),
    starts_at        TEXT    NOT NULL CHECK (starts_at IS strftime('%Y-%m-%dT%H:%M:%fZ', starts_at)),
    ends_at          TEXT    NOT NULL CHECK (ends_at IS strftime('%Y-%m-%dT%H:%M:%fZ', ends_at)),
    available_places INTEGER NOT NULL CHECK (available_places > 0),
    PRIMARY KEY (court_id, starts_at),
    CHECK (ends_at > starts_at)
) STRICT, WITHOUT ROWID;

-- Posted alert lines retained after a slot becomes unbookable so its message can be edited.
CREATE TABLE IF NOT EXISTS alert_message_slots (
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

CREATE INDEX IF NOT EXISTS alert_message_slots_slot_idx
    ON alert_message_slots (court_id, starts_at) WHERE struck = 0;
