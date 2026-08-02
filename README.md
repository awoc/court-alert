# court-alert

`court-alert` monitors configured clubs for newly bookable court slots. It stores the last observed availability in SQLite and can post changes to a Discord webhook or send matching slot alerts as Discord DMs to subscribers.

Each club is a venue with a sport: `/subscribe` covers tennis venues and `/padel` covers padel ones. Only tennis reaches the broadcast webhook channel; padel alerts go out solely as `/padel` direct messages.

## Configuration

1. A configuration file is required. Copy the example, then edit `config.toml` to choose courts to monitor:

   Replace the example venue, then adjust the global defaults:
   - `poll_interval_secs` — polling frequency.
   - `lookahead_days` — booking horizon to query.
   - `quiet_first_poll` — suppress alerts for slots already open when an empty database is initialized.
   - `operating_window_start_hour` / `operating_window_end_hour` — Berlin-local half-open polling window.
   - `surface_filter` — `clay` (default), `synthetic` or `all`. Restricts what the webhook channel posts, and is the default for new subscriptions.

   Each monitored club is a `[[venues]]` entry:
   - `id` — a stable key of your choosing. It is the join key for stored slots, so **do not change it once set**: a rename orphans that venue's rows. The existing deployment keeps `id = "zhs-munich"`.
   - `display_name`, `sport` (`tennis` or `padel`) and `provider` (`zhs`).
   - `base_url` for a `zhs` venue.
   - `poll_interval_secs`, `lookahead_days`, `operating_window_start_hour` / `operating_window_end_hour` — optional per-venue overrides of the globals above.
   - `[[venues.courts]]` — the courts to poll, each with an `id`, a `name`, and for a tennis venue a `surface` of `clay` (default) or `synthetic`.

   Every court is polled whatever the filter says, so subscribers can still ask for a surface the channel does not carry.

   > Upgrading from a flat top-level `base_url` and `[[products]]`? Move them into a single `[[venues]]` entry as above; the old layout is rejected at startup. Then apply the pending migrations in `sql/migrations` (see [Database](#database)).

   `config.toml` is intentionally ignored by Git. Set `COURT_ALERT_CONFIG_PATH` to use a configuration file at another path.

2. Set the environment variables used by the application:

   | Variable                            | Required | Description                                                                                        |
   | ----------------------------------- | -------- | -------------------------------------------------------------------------------------------------- |
   | `COURT_ALERT_EMAIL`                 | Yes      | ZHS account email address.                                                                         |
   | `COURT_ALERT_PASSWORD`              | Yes      | ZHS account password.                                                                              |
   | `COURT_ALERT_CONFIG_PATH`           | No       | Path to the TOML configuration file. Defaults to `config.toml`.                                    |
   | `COURT_ALERT_DB_PATH`               | No       | Path to the SQLite database. Defaults to `data/court-alert.db`.                                    |
   | `COURT_ALERT_DISCORD_WEBHOOK`       | No       | Discord webhook URL that receives availability changes on the configured surfaces.                 |
   | `COURT_ALERT_DISCORD_ERROR_WEBHOOK` | No       | Discord monitoring-channel webhook that receives errors from background operations.                |
   | `COURT_ALERT_DISCORD_BOT_TOKEN`     | No       | Enables the Discord bot, slash commands, and DM subscription alerts.                               |
   | `COURT_ALERT_DISCORD_GUILD_ID`      | No       | Numeric Discord guild ID. With the bot enabled, commands are registered immediately in this guild. |
   | `COURT_ALERT_DISCORD_ADMIN_IDS`     | No       | Comma-separated Discord user IDs allowed to use admin bot commands.                                |

   `COURT_ALERT_DISCORD_GUILD_ID` and `COURT_ALERT_DISCORD_ADMIN_IDS` only apply when `COURT_ALERT_DISCORD_BOT_TOKEN` is set.

## Run

Install the Rust toolchain specified in `rust-toolchain.toml`, then run locally:

```sh
cargo run --release
```

## Database

Startup creates the schema in an empty database, but never upgrades an existing one: it refuses to start against a database that is behind and names the files to apply. Run them by hand, in order, from the version the database is at:

```sh
sqlite3 -bail data/court-alert.db < sql/migrations/0004_venue_scoped_slots.sql
sqlite3 -bail data/court-alert.db < sql/migrations/0005_subscription_sport_and_venue.sql
```

`0004` drops `bookable_slots` rather than migrating it — it is a cache the next poll rewrites, and pre-existing rows have no venue to attribute them to. The next poll therefore starts from an empty snapshot, so **check `quiet_first_poll = true` before running it**, or the first poll after the restart alerts on every slot that is currently free.

`0005` gives subscriptions a `sport` and an optional `venue`, and renames `surface` to `court_filter` now that the column spans both sports' vocabularies. Existing rows become `sport = 'tennis'` with no venue, i.e. "all tennis venues", which preserves their behaviour exactly.
