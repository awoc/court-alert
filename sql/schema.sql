CREATE TABLE IF NOT EXISTS subscriptions (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    provider     TEXT NOT NULL,
    user_id      TEXT NOT NULL,
    weekday      INTEGER,
    on_date      TEXT,
    start_minute INTEGER NOT NULL,
    end_minute   INTEGER NOT NULL,
    courts       TEXT,
    created_at   TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK ((weekday IS NULL) <> (on_date IS NULL))
);

CREATE INDEX IF NOT EXISTS subs_user_idx ON subscriptions(provider, user_id);

CREATE TABLE IF NOT EXISTS slot_state (
    product_id   TEXT NOT NULL,
    product_name TEXT NOT NULL,
    start_at     TEXT NOT NULL,
    end_at       TEXT NOT NULL,
    availability INTEGER NOT NULL,
    PRIMARY KEY (product_id, start_at)
);
