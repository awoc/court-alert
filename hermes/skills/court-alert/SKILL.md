---
name: court-alert
description: Subscribe people to tennis or padel court alerts from court-alert, list or delete their subscriptions, look up clubs and courts, and tell them about newly bookable slots when an alert arrives.
version: 1.0.0
platforms: [macos, linux]
metadata:
  hermes:
    tags: [tennis, padel, booking, alerts]
    requires_toolsets: [terminal]
required_environment_variables:
  - name: COURT_ALERT_URL
    prompt: court-alert webhook API base URL (e.g. http://pi2:8645)
  - name: COURT_ALERT_API_SECRET
    prompt: court-alert webhook API secret (COURT_ALERT_WEBHOOK_API_SECRET)
---

# court-alert

court-alert watches tennis and padel clubs in Munich and alerts subscribers when a matching court slot becomes bookable. You manage subscriptions for the people you talk to over a signed HTTP API, and court-alert POSTs alerts to your `court-alert` webhook route.

## Sending requests

Every request must be signed, so plain `curl` is rejected with `401`. Send every request through the helper:

```sh
~/.hermes/skills/court-alert/scripts/court-alert.sh METHOD PATH [JSON_BODY]
```

It prints the JSON response on stdout and `HTTP <status>` on stderr, and exits non-zero for any non-2xx status. Send `PATH` exactly as you want it signed, query string included.

## Subscriber IDs

Each request names a subscriber. Use `<platform>:<chat id>` for the person you are acting for, for example `telegram:123456789`. Use the same ID on every request for that person: court-alert scopes each subscription to its exact subscriber ID, and alerts carry it back as `user_id` so you know whom to tell. IDs are 1 to 128 characters.

## Endpoints

### List clubs: `GET /clubs?sport=<tennis|padel>`

```sh
court-alert.sh GET '/clubs?sport=padel'
```

`200` with `{"clubs": [{"id", "name", "sport", "courts": [{"name", "surface", "location"}]}]}`. `surface` is `clay`, `synthetic` or null; `location` is `indoor`, `outdoor` or null. Call this before subscribing so you use real club IDs and court names.

### Subscribe: `POST /subscriptions`

```sh
court-alert.sh POST /subscriptions '{"user_id":"telegram:123456789","sport":"padel","club":"casa-padel","weekday":"Thu","start":"18:00","end":"21:30","filter":"indoor"}'
```

| Field     | Required | Value                                                                            |
| --------- | -------- | -------------------------------------------------------------------------------- |
| `user_id` | yes      | The subscriber ID.                                                               |
| `sport`   | yes      | `tennis` or `padel`.                                                             |
| `club`    | no       | A club `id` from `GET /clubs`. Omit to watch every club of the sport.            |
| `weekday` | one of   | Recurring day: `Mon` … `Sun`.                                                    |
| `date`    | one of   | One day, `YYYY-MM-DD`, today or later. Send exactly one of `weekday` and `date`. |
| `start`   | yes      | `HH:MM`, Europe/Berlin local time. Slots starting at or after it match.          |
| `end`     | yes      | `HH:MM`, after `start`. Slots starting before it match. `24:00` means midnight.  |
| `courts`  | no       | Court names from `GET /clubs`. Omit for every court.                             |
| `filter`  | no       | `any`, `clay`, `synthetic`, `indoor` or `outdoor`. See below for the default.    |

Without `filter`, a tennis subscription without `courts` takes court-alert's configured surface (usually `clay`); every other subscription takes `any`. Tennis courts have no `location`, so `indoor` and `outdoor` are padel filters.

`201` with `{"subscription": {...}, "open_slots": [...]}`. `open_slots` are matching slots that are already bookable: tell the person straight away, because no alert will be sent for them. In the returned subscription, `club` is the club's display name.

### List subscriptions: `GET /subscriptions?user=<subscriber id>`

`200` with `{"subscriptions": [{"id", "sport", "club", "weekday" or "date", "start", "end", "courts", "filter"}]}`.

### Delete a subscription: `DELETE /subscriptions/<id>?user=<subscriber id>`

`204` with an empty body when deleted. `404` `{"error": "not_found"}` when the subscription does not exist or belongs to another subscriber. List the person's subscriptions first to find the `id`.

## Errors

Every error body is JSON with an `error` kind. Recover as follows:

| Status | `error`                  | Details              | Recovery                                                                                               |
| ------ | ------------------------ | -------------------- | ------------------------------------------------------------------------------------------------------ |
| 400    | `invalid_request`        | `message`            | Fix the JSON or query string as `message` describes (unknown field, bad time format, missing `sport`). |
| 401    | `unauthorized`           |                      | Check `COURT_ALERT_API_SECRET` and that this host's clock is within 5 minutes of court-alert's.        |
| 413    | `body_too_large`         |                      | Keep request bodies under 64 KiB.                                                                      |
| 404    | `not_found`              |                      | List the subscriber's subscriptions and use an `id` from there.                                        |
| 422    | `invalid_user_id`        |                      | Use a non-empty subscriber ID of at most 128 characters.                                               |
| 422    | `invalid_time_range`     |                      | Make `end` later than `start`.                                                                         |
| 422    | `invalid_schedule`       |                      | Use a `date` of today or later.                                                                        |
| 422    | `unknown_club`           | `unknown, available` | `available` lists `{"id", "name"}` of each club; retry with the right `id`.                            |
| 422    | `unknown_courts`         | `unknown, available` | Ask the person to pick from `available`, or drop `courts`.                                             |
| 422    | `no_clubs_configured`    | `sport`              | court-alert monitors no clubs for that sport; tell the person.                                         |
| 422    | `filter_excludes_courts` | `courts, filter`     | The chosen courts don't match `filter`; drop one of them or use `"filter": "any"`.                     |
| 500    | `internal`               |                      | court-alert failed; tell the person to try again later.                                                |

## Alerts

When matching slots open, court-alert POSTs one alert per subscriber to your `court-alert` route:

```json
{
  "user_id": "telegram:123456789",
  "slots": [
    {
      "club": "Casa Padel",
      "court": "Court 1 (Indoor)",
      "court_id": "…",
      "starts_at": "2026-11-05T17:00:00Z",
      "ends_at": "2026-11-05T18:30:00Z"
    }
  ]
}
```

Times are UTC. Convert them to Europe/Berlin before telling the person, and group slots by club and day. Alerts are sent once and never updated, so a slot may already be taken by the time the person looks.
