-- Timestamps are stored as canonical UTC RFC 3339 with milliseconds
-- (YYYY-MM-DDTHH:MM:SS.SSSZ) so that text comparison equals chronological order.

CREATE TABLE IF NOT EXISTS subscriptions (
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
    -- Exactly one schedule kind: recurring weekday or single date.
    CHECK ((weekday IS NULL) <> (on_date IS NULL)),
    -- Half-open [start_minute, end_minute) within a single day.
    CHECK (start_minute < end_minute)
) STRICT;

CREATE INDEX IF NOT EXISTS subscriptions_user_idx ON subscriptions (provider, user_id);

-- Last observed snapshot of bookable slots; rewritten per venue on every poll.
CREATE TABLE IF NOT EXISTS bookable_slots (
    venue_id         TEXT    NOT NULL CHECK (venue_id <> ''),
    court_id         TEXT    NOT NULL CHECK (length(court_id) = 36),
    court_name       TEXT    NOT NULL CHECK (court_name <> ''),
    starts_at        TEXT    NOT NULL CHECK (starts_at IS strftime('%Y-%m-%dT%H:%M:%fZ', starts_at)),
    ends_at          TEXT    NOT NULL CHECK (ends_at IS strftime('%Y-%m-%dT%H:%M:%fZ', ends_at)),
    available_places INTEGER NOT NULL CHECK (available_places > 0),
    PRIMARY KEY (court_id, starts_at),
    CHECK (ends_at > starts_at)
) STRICT, WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS bookable_slots_venue_idx ON bookable_slots (venue_id);

-- Tracks completed polls even when a venue has no free slots.
CREATE TABLE IF NOT EXISTS venue_state (
    venue_id       TEXT NOT NULL PRIMARY KEY CHECK (venue_id <> ''),
    initialised_at TEXT NOT NULL
                   CHECK (initialised_at IS strftime('%Y-%m-%dT%H:%M:%fZ', initialised_at))
) STRICT, WITHOUT ROWID;

-- Posted alert lines retained after a slot becomes unbookable so its message can be edited.
CREATE TABLE IF NOT EXISTS alert_message_slots (
    surface    TEXT    NOT NULL CHECK (surface IN ('channel', 'dm')),
    channel_id TEXT    CHECK (channel_id IS NULL OR channel_id <> ''),
    message_id TEXT    NOT NULL CHECK (message_id <> ''),
    line_index INTEGER NOT NULL CHECK (line_index >= 0),
    club       TEXT    CHECK (club IS NULL OR club <> ''),
    court_id   TEXT    NOT NULL CHECK (length(court_id) = 36),
    court_name TEXT    NOT NULL CHECK (court_name <> ''),
    starts_at  TEXT    NOT NULL CHECK (starts_at IS strftime('%Y-%m-%dT%H:%M:%fZ', starts_at)),
    ends_at    TEXT    NOT NULL CHECK (ends_at IS strftime('%Y-%m-%dT%H:%M:%fZ', ends_at)),
    struck     INTEGER NOT NULL DEFAULT 0 CHECK (struck IN (0, 1)),
    -- Discord message ids are unique across channels, so they alone identify a message.
    PRIMARY KEY (message_id, line_index),
    CHECK ((surface = 'dm') = (channel_id IS NOT NULL)),
    CHECK (ends_at > starts_at)
) STRICT, WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS alert_message_slots_slot_idx
    ON alert_message_slots (surface, court_id, starts_at) WHERE struck = 0;
