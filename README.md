# court-alert

`court-alert` monitors configured clubs for newly bookable court slots. It stores the last observed availability in SQLite and can post changes to a Discord webhook, send matching slot alerts as Discord DMs to subscribers, or let an agent manage subscriptions and receive alerts over HTTP.

## Configuration

1. A configuration file is required. Copy the example, then edit `config.toml` to choose courts to monitor:

   Replace the example venue, then adjust the global defaults:
   - `poll_interval_secs` — polling frequency.
   - `lookahead_days` — booking horizon to query.
   - `quiet_first_poll` — suppress alerts for slots already open when an empty database is initialized.
   - `operating_window_start_hour` / `operating_window_end_hour` — Berlin-local half-open polling window.
   - `surface_filter` — `clay` (default), `synthetic` or `all`. Restricts what the webhook channel posts, and is the default for new `/tennis` reminders. It is a **tennis** setting: `indoor`/`outdoor` are rejected, since no tennis court carries a location and the filter would silently match nothing. Use `/padel`'s `location` option for those.

   Each monitored club is a `[[venues]]` entry:
   - `id` — a stable key of your choosing. It is the join key for stored slots, so **do not change it once set**: a rename orphans that venue's rows. The existing deployment keeps `id = "zhs-munich"`.
   - `display_name`, `sport` (`tennis` or `padel`) and `provider` (`zhs` or `playtomic`).
   - For `provider = "zhs"`: `base_url`, plus a `[[venues.courts]]` list — each court with an `id`, a `name`, and for a tennis venue a `surface` of `clay` (default) or `synthetic`.
   - For `provider = "playtomic"`: `tenant_id` and `slug` (the `/clubs/<slug>` path on playtomic.com). The courts, their indoor/outdoor location and the club's opening hours are read from the club page at startup and refreshed daily. While the Discord bot is enabled, at most 25 padel venues can be configured, because that is how many clubs Discord lets `/padel` offer by name.
   - `poll_interval_secs`, `lookahead_days`, `operating_window_start_hour` / `operating_window_end_hour` — optional per-venue overrides of the globals above. Playtomic serves today through today+14, so `lookahead_days` above 15 is rejected for those venues.

   Every court is polled whatever the filter says, so subscribers can still ask for a surface the channel does not carry.

   `config.toml` is intentionally ignored by Git. Set `COURT_ALERT_CONFIG_PATH` to use a configuration file at another path.

2. Set the environment variables used by the application:

   | Variable                            | Required | Description                                                                                        |
   | ----------------------------------- | -------- | -------------------------------------------------------------------------------------------------- |
   | `COURT_ALERT_EMAIL`                 | ZHS only | ZHS account email address. Required only if a `zhs` venue is configured.                           |
   | `COURT_ALERT_PASSWORD`              | ZHS only | ZHS account password. Required only if a `zhs` venue is configured.                                |
   | `COURT_ALERT_CONFIG_PATH`           | No       | Path to the TOML configuration file. Defaults to `config.toml`.                                    |
   | `COURT_ALERT_DB_PATH`               | No       | Path to the SQLite database. Defaults to `data/court-alert.db`.                                    |
   | `COURT_ALERT_DISCORD_WEBHOOK`       | No       | Discord webhook URL that receives availability changes on the configured surfaces.                 |
   | `COURT_ALERT_DISCORD_ERROR_WEBHOOK` | No       | Discord monitoring-channel webhook that receives errors from background operations.                |
   | `COURT_ALERT_DISCORD_BOT_TOKEN`     | No       | Enables the Discord bot, slash commands, and DM subscription alerts.                               |
   | `COURT_ALERT_DISCORD_GUILD_ID`      | No       | Numeric Discord guild ID. With the bot enabled, commands are registered immediately in this guild. |
   | `COURT_ALERT_DISCORD_ADMIN_IDS`     | No       | Comma-separated Discord user IDs allowed to use admin bot commands.                                |
   | `COURT_ALERT_WEBHOOK_LISTEN`        | No       | Address for the webhook API, e.g. `0.0.0.0:8645`. Enables the webhook integration.                 |
   | `COURT_ALERT_WEBHOOK_API_SECRET`    | Webhook  | Secret that callers of the webhook API sign their requests with.                                   |
   | `COURT_ALERT_WEBHOOK_ALERT_URL`     | Webhook  | URL that receives alerts for webhook subscribers.                                                  |
   | `COURT_ALERT_WEBHOOK_ALERT_SECRET`  | Webhook  | Secret that alerts are signed with.                                                                |

   `COURT_ALERT_DISCORD_GUILD_ID` and `COURT_ALERT_DISCORD_ADMIN_IDS` only apply when `COURT_ALERT_DISCORD_BOT_TOKEN` is set. The three `Webhook` variables are required once `COURT_ALERT_WEBHOOK_LISTEN` is set, and startup fails naming any that are missing.

## Run

Install the Rust toolchain specified in `rust-toolchain.toml`, then run locally:

```sh
cargo run --release
```

## Upgrading

The app never changes the schema of an existing database. It refuses to start against one that is behind and names the migration files to run. Stop the service, back up the database, then apply every file in `sql/migrations` above the database's `PRAGMA user_version`, in order:

```sh
sqlite3 -bail data/court-alert.db < sql/migrations/0007_subscription_court_ids.sql
```

`0007` adds the `selected_courts` column but cannot fill it, because Playtomic catalogs are only discovered at runtime. The app does the rest once it runs with an integration enabled: it tries at startup, retries every minute, and stops when no row is left.

- A subscription with a club converts once that club's catalog has loaded. One with no club waits for every club of its sport, which for a Playtomic club can take until its first poll inside the operating window.
- Names compare case-insensitively, and a name that matches several courts keeps all of them.
- Until a subscription converts it keeps matching by name, so no alerts are lost along the way.
- A subscription with a name that matches no court stays unconverted as a whole, logs a warning once per run, and is tried again as catalogs refresh.
- Once converted, a subscription is never derived from names again.

`0007` comes with a change to the webhook API: a subscription's `courts` used to be a list of names and is now a list of `{"id", "name"}` objects, so callers need updating when you deploy it.

## Court selections

A subscription stores each court it selects as a club and a court ID. A court keeps alerting after a rename, and a selection never alerts for a different court with the same name.

A Discord court name, or a number such as `2`, picks every court with that name or number when the reminder is created. With no club set it looks across all clubs of the sport, and with a club set it looks only at that club. A court added later under the same name is not included. Naming courts is rejected while a club in scope has not loaded its courts yet.

## Alert delivery

Each integration delivers alerts on its own worker, with a queue of up to 8 batches and up to 16 alerts in flight. A batch is what one poll of a club changed for that integration. A stalled receiver delays only its own integration and never holds back polling.

Within an integration, batches go out in order. A chat provider updates its alert messages for a batch only after that batch's alerts have finished. There is no ordering between integrations.

When an integration's queue is full, court-alert drops that integration's new batch and logs a warning with the integration and the number of alerts and slots dropped. It never retries or stores a dropped batch, so those alerts are lost. A dropped batch can also leave a stale line on a Discord alert message, because the update that would have marked the slot was in that batch.

## Webhook integration

The webhook integration lets an agent on the same LAN manage subscriptions for the people it talks to and receive matching alerts, all over HTTP. It runs alongside Discord and is enabled by `COURT_ALERT_WEBHOOK_LISTEN`.

### API

The API is plain HTTP and JSON, meant for the LAN only. Every request carries a subscriber ID chosen by the caller, so one agent can act for several people; subscribers can't see or delete each other's subscriptions.

| Request                                                 | Success                                    |
| ------------------------------------------------------- | ------------------------------------------ |
| `GET /clubs?sport=<tennis\|padel>`                      | `200` with clubs, their IDs and courts     |
| `POST /subscriptions`                                   | `201` with the subscription and open slots |
| `GET /subscriptions?subscriber_id=<subscriber>`         | `200` with the subscriber's subscriptions  |
| `DELETE /subscriptions/<id>?subscriber_id=<subscriber>` | `204`                                      |

In a subscription, `courts` is `null` when it watches every court, otherwise one `{"id", "name"}` object per selected court. `id` is the court ID from `GET /clubs`, and it is what the subscription matches on. `name` is the court's current name, so it follows renames. `name` is `null` while the club's catalog has not loaded, for example just after a restart, and once the court has left the catalog; the court stays selected either way. `id` is `null` only for an older subscription still stored by name.

Errors are JSON with an `error` kind: `400` for a malformed request, such as bad JSON, query parameters, a subscription ID in the path or times not written as `HH:MM`, `401` for a missing or invalid signature, `404` for a missing subscription, `408` for a body that takes over 10 seconds to arrive, `413` for a body over 64 KiB, `422` for validation rejections (with the valid alternatives where there are any) and `500` for a server failure.

### Signing

Each request sends `X-Webhook-Timestamp` (Unix seconds) and `X-Webhook-Signature-V2`, the hex HMAC-SHA256 of `"<timestamp>\n<METHOD>\n<path with query>\n<body>"` (newline-separated) keyed by `COURT_ALERT_WEBHOOK_API_SECRET`. The body is empty for `GET` and `DELETE`. Requests more than 300 seconds from the server's clock are rejected.

### Alerts

When slots open that match a webhook subscriber's subscriptions, court-alert sends one `POST` per subscriber per poll to `COURT_ALERT_WEBHOOK_ALERT_URL`, with the body `{"subscriber_id": "...", "slots": [{"club", "court", "court_id", "starts_at", "ends_at"}]}` and times in UTC. Alerts carry `X-Webhook-Timestamp` and `X-Webhook-Signature-V2`, the HMAC-SHA256 of `"<timestamp>.<body>"` keyed by `COURT_ALERT_WEBHOOK_ALERT_SECRET`, and a unique `webhook-id` header. A failed delivery is logged and dropped without retries, and so is a batch that finds the [delivery queue](#alert-delivery) full. Slots that become unbookable send nothing.
