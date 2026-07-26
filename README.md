# court-alert

`court-alert` monitors configured ZHS Munich court products for newly bookable slots. It stores the last observed availability in SQLite and can post changes to a Discord webhook or send matching slot alerts as Discord DMs to subscribers.

## Configuration

1. A configuration file is required. Copy the example, then edit `config.toml` to choose courts to monitor:

   Replace the example court, then adjust:
   - `poll_interval_secs` — polling frequency.
   - `lookahead_days` — booking horizon to query.
   - `quiet_first_poll` — suppress alerts for slots already open when an empty database is initialized.
   - `operating_window_start_hour` / `operating_window_end_hour` — Berlin-local half-open polling window.

   `config.toml` is intentionally ignored by Git. Set `COURT_ALERT_CONFIG_PATH` to use a configuration file at another path.

2. Set the environment variables used by the application:

   | Variable                            | Required | Description                                                                                        |
   | ----------------------------------- | -------- | -------------------------------------------------------------------------------------------------- |
   | `COURT_ALERT_EMAIL`                 | Yes      | ZHS account email address.                                                                         |
   | `COURT_ALERT_PASSWORD`              | Yes      | ZHS account password.                                                                              |
   | `COURT_ALERT_CONFIG_PATH`           | No       | Path to the TOML configuration file. Defaults to `config.toml`.                                    |
   | `COURT_ALERT_DB_PATH`               | No       | Path to the SQLite database. Defaults to `data/court-alert.db`.                                    |
   | `COURT_ALERT_DISCORD_WEBHOOK`       | No       | Discord webhook URL that receives all availability changes.                                        |
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
