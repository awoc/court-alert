# court-alert

`court-alert` monitors configured clubs for newly bookable court slots. It stores the last observed availability in SQLite and can post changes to a Discord webhook or send matching slot alerts as Discord DMs to subscribers.

Each club is a venue with a sport: `/subscribe` covers tennis venues and `/padel` covers padel ones. Only tennis reaches the broadcast webhook channel; padel alerts go out solely as `/padel` direct messages. Direct messages name the club each court belongs to, since a reminder can span clubs and "Court 1" is not a unique name across them.

`/padel` takes an optional `club` (omit it to watch every configured club) and an optional `location` of `indoor`, `outdoor` or `any`. It has no `courts` option: padel court names are discovered and refreshed, so a name-based selector would silently stop matching if a club renamed a court.

> Playtomic exposes no public API — the availability route and the club page are both private surfaces used by their own frontend, and `robots.txt` disallows `/api`. Requests are sequential per club with a short delay, the poll interval stays conservative, and both payloads are pinned behind tests so a change fails loudly rather than silently.

## Configuration

1. A configuration file is required. Copy the example, then edit `config.toml` to choose courts to monitor:

   Replace the example venue, then adjust the global defaults:
   - `poll_interval_secs` — polling frequency.
   - `lookahead_days` — booking horizon to query.
   - `quiet_first_poll` — suppress alerts for slots already open when an empty database is initialized.
   - `operating_window_start_hour` / `operating_window_end_hour` — Berlin-local half-open polling window.
   - `surface_filter` — `clay` (default), `synthetic` or `all`. Restricts what the webhook channel posts, and is the default for new `/subscribe` reminders. It is a **tennis** setting: `indoor`/`outdoor` are rejected, since no tennis court carries a location and the filter would silently match nothing. Use `/padel`'s `location` option for those.

   Each monitored club is a `[[venues]]` entry:
   - `id` — a stable key of your choosing. It is the join key for stored slots, so **do not change it once set**: a rename orphans that venue's rows. The existing deployment keeps `id = "zhs-munich"`.
   - `display_name`, `sport` (`tennis` or `padel`) and `provider` (`zhs` or `playtomic`).
   - For `provider = "zhs"`: `base_url`, plus a `[[venues.courts]]` list — each court with an `id`, a `name`, and for a tennis venue a `surface` of `clay` (default) or `synthetic`.
   - For `provider = "playtomic"`: `tenant_id` and `slug` (the `/clubs/<slug>` path on playtomic.com). **No credentials and no court list**: the courts, their indoor/outdoor location and the club's opening hours are read from the club page at startup and refreshed daily. At most 25 padel venues can be configured, because that is how many clubs Discord lets `/padel` offer by name.
   - `poll_interval_secs`, `lookahead_days`, `operating_window_start_hour` / `operating_window_end_hour` — optional per-venue overrides of the globals above. Playtomic serves today through today+14, so `lookahead_days` above 15 is rejected for those venues.

   Every court is polled whatever the filter says, so subscribers can still ask for a surface the channel does not carry.

   > Upgrading from a flat top-level `base_url` and `[[products]]`? Move them into a single `[[venues]]` entry as above; the old layout is rejected at startup. Then apply the pending migrations in `sql/migrations` (see [Database](#database)).

   `config.toml` is intentionally ignored by Git. Set `COURT_ALERT_CONFIG_PATH` to use a configuration file at another path.

2. Set the environment variables used by the application:

   | Variable                            | Required | Description                                                                                        |
   | ----------------------------------- | -------- | -------------------------------------------------------------------------------------------------- |
   | `COURT_ALERT_EMAIL`                 | ZHS only | ZHS account email address. Required only if a `zhs` venue is configured.                            |
   | `COURT_ALERT_PASSWORD`              | ZHS only | ZHS account password. Required only if a `zhs` venue is configured.                                 |
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
