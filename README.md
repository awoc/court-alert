# court-alert

`court-alert` monitors configured clubs for newly bookable court slots. It stores the last observed availability in SQLite and can post changes to a Discord webhook, send matching slot alerts as Discord DMs to subscribers, or let an agent such as Hermes manage subscriptions and receive alerts over HTTP.

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
   | `COURT_ALERT_WEBHOOK_ALERT_URL`     | Webhook  | URL that receives alerts for webhook subscribers, e.g. a Hermes webhook route.                     |
   | `COURT_ALERT_WEBHOOK_ALERT_SECRET`  | Webhook  | Secret that alerts are signed with (Hermes Generic V2).                                            |

   `COURT_ALERT_DISCORD_GUILD_ID` and `COURT_ALERT_DISCORD_ADMIN_IDS` only apply when `COURT_ALERT_DISCORD_BOT_TOKEN` is set. The three `Webhook` variables are required once `COURT_ALERT_WEBHOOK_LISTEN` is set, and startup fails naming any that are missing.

## Run

Install the Rust toolchain specified in `rust-toolchain.toml`, then run locally:

```sh
cargo run --release
```

## Webhook integration

The webhook integration lets an agent, such as a [Hermes](https://github.com/NousResearch/hermes-agent) agent on the same LAN, manage subscriptions for the people it talks to and receive matching alerts, all over HTTP. It runs alongside Discord and is enabled by `COURT_ALERT_WEBHOOK_LISTEN`.

### API

The API is plain HTTP and JSON, meant for the LAN only. Every request carries a subscriber ID chosen by the caller, so one agent can act for several people; subscribers can't see or delete each other's subscriptions.

| Request                                        | Success                                    |
| ---------------------------------------------- | ------------------------------------------ |
| `GET /clubs?sport=<tennis\|padel>`             | `200` with clubs, their IDs and courts     |
| `POST /subscriptions`                          | `201` with the subscription and open slots |
| `GET /subscriptions?user=<subscriber>`         | `200` with the subscriber's subscriptions  |
| `DELETE /subscriptions/<id>?user=<subscriber>` | `204`                                      |

Errors are JSON with an `error` kind: `400` for malformed JSON or query parameters, `401` for a missing or invalid signature, `404` for a missing subscription, `413` for a body over 64 KiB, `422` for validation rejections (with the valid alternatives where there are any) and `500` for a server failure. [`hermes/skills/court-alert/SKILL.md`](hermes/skills/court-alert/SKILL.md) documents the request fields and every error kind.

### Signing

Each request sends `X-Webhook-Timestamp` (Unix seconds) and `X-Webhook-Signature-V2`, the hex HMAC-SHA256 of `"<timestamp>.<METHOD>.<path with query>.<body>"` keyed by `COURT_ALERT_WEBHOOK_API_SECRET`. The body is empty for `GET` and `DELETE`. Requests more than 300 seconds from the server's clock are rejected. [ADR 0001](docs/adr/0001-hmac-signed-webhook-api.md) explains why the signature covers the method and path. [`hermes/skills/court-alert/scripts/court-alert.sh`](hermes/skills/court-alert/scripts/court-alert.sh) signs and sends a request with `openssl` and `curl`:

```sh
export COURT_ALERT_URL=http://pi2:8645 COURT_ALERT_API_SECRET=...
hermes/skills/court-alert/scripts/court-alert.sh GET '/clubs?sport=padel'
```

### Alerts

When slots open that match a webhook subscriber's subscriptions, court-alert sends one `POST` per subscriber per poll to `COURT_ALERT_WEBHOOK_ALERT_URL`, with the body `{"user_id": "...", "slots": [{"club", "court", "court_id", "starts_at", "ends_at"}]}` and times in UTC. Alerts are signed with Hermes Generic V2 (`X-Webhook-Timestamp`, plus `X-Webhook-Signature-V2` over `"<timestamp>.<body>"` keyed by `COURT_ALERT_WEBHOOK_ALERT_SECRET`) and carry a unique `webhook-id` header. A failed delivery is logged and dropped without retries. Slots that become unbookable send nothing.

### Connecting Hermes

1. Generate two secrets, one for the API and one for alerts, e.g. with `openssl rand -hex 32`.
2. Set the four `COURT_ALERT_WEBHOOK_*` variables for court-alert, with the alert URL pointing at the Hermes route, e.g. `http://<hermes-host>:8644/webhooks/court-alert`, and restart it.
3. Copy `hermes/skills/court-alert` to `~/.hermes/skills/court-alert`, and set `COURT_ALERT_URL` (e.g. `http://pi2:8645`) and `COURT_ALERT_API_SECRET` in `~/.hermes/.env`.
4. Copy `hermes/scripts/court-alert-alert.py` to `~/.hermes/scripts/`, and merge [`hermes/court-alert-route.yaml`](hermes/court-alert-route.yaml) into `~/.hermes/config.yaml` with the alert secret. The sample route delivers to Telegram and ignores alerts for subscriber IDs with another prefix. For another platform, change `deliver` and the `user_id` filter; Hermes fixes `deliver` per route and court-alert sends to one URL, so a single deployment serves one platform.
5. Restart the Hermes gateway and ask the agent to list the padel clubs. It should answer from `GET /clubs`.
