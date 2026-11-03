use std::collections::HashSet;
use std::sync::Arc;

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

use super::alerts::WebhookAlertSender;
use super::{INTEGRATION, WebhookIntegration};
use crate::config::WebhookSettings;
use crate::integrations::{Integration, IntegrationServices, readiness};
use crate::model::{
    AvailabilityChange, BookableSlot, BookableSlotId, Court, CourtAttributes, CourtCatalog,
    CourtFilter, CourtSelection, Schedule, Sport, SubscriberRef, TimeRange, UncheckedDraft, Venue,
    VenueId, VenueIdentity, VenueRegistry,
};
use crate::ports::{BookableSlotSnapshotRepository, SubscriptionRepository};
use crate::store::SqliteStore;
use crate::subscriptions::contract::{AlertSender, AvailabilityAlert, SubscriptionCommand};
use crate::subscriptions::testing::registry;
use crate::subscriptions::{BerlinClock, ClubDirectory, SubscriptionService};
use uuid::Uuid;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const API_SECRET: &str = "api-secret";

struct Api {
    base: String,
    http: reqwest::Client,
}

async fn services() -> IntegrationServices {
    services_over(Arc::new(SqliteStore::open_in_memory().await.unwrap()))
}

fn services_over(store: Arc<SqliteStore>) -> IntegrationServices {
    services_with(store, registry())
}

fn services_with(
    store: Arc<SqliteStore>,
    registry: Arc<std::sync::RwLock<VenueRegistry>>,
) -> IntegrationServices {
    IntegrationServices {
        subscriptions: Arc::new(SubscriptionService::new(
            store.clone(),
            store,
            HashSet::new(),
            registry.clone(),
            CourtFilter::Any,
            Arc::new(BerlinClock),
        )),
        clubs: ClubDirectory::new(registry),
    }
}

fn integration() -> WebhookIntegration {
    WebhookIntegration::new(WebhookSettings {
        listen: "127.0.0.1:0".parse().unwrap(),
        api_secret: API_SECRET.into(),
        alert_url: "http://127.0.0.1:9/alerts".parse().unwrap(),
        alert_secret: "alert-secret".into(),
    })
}

async fn start() -> Api {
    serve(services().await).await
}

async fn serve(services: IntegrationServices) -> Api {
    let webhook = integration().bind().await.expect("bind webhook listener");
    let base = format!("http://{}", webhook.local_addr().unwrap());
    tokio::spawn(webhook.serve(services.subscriptions, services.clubs));
    Api {
        base,
        http: reqwest::Client::new(),
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn signature(secret: &str, timestamp: i64, method: &str, path: &str, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{timestamp}\n{method}\n{path}\n{body}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

impl Api {
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        headers: &[(&str, String)],
    ) -> reqwest::Response {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        request.send().await.expect("request reaches the listener")
    }

    async fn signed_get(&self, path: &str) -> reqwest::Response {
        self.send(
            reqwest::Method::GET,
            path,
            &signed_headers(API_SECRET, now(), "GET", path),
        )
        .await
    }

    async fn signed_delete(&self, path: &str) -> reqwest::Response {
        self.send(
            reqwest::Method::DELETE,
            path,
            &signed_headers(API_SECRET, now(), "DELETE", path),
        )
        .await
    }

    async fn signed_post(&self, path: &str, body: &str) -> reqwest::Response {
        let timestamp = now();
        self.http
            .post(format!("{}{path}", self.base))
            .header("Content-Type", "application/json")
            .header("X-Webhook-Timestamp", timestamp.to_string())
            .header(
                "X-Webhook-Signature-V2",
                signature(API_SECRET, timestamp, "POST", path, body),
            )
            .body(body.to_owned())
            .send()
            .await
            .expect("request reaches the listener")
    }

    async fn subscribe(&self, body: Value) -> reqwest::Response {
        self.signed_post("/subscriptions", &body.to_string()).await
    }

    async fn subscriptions_of(&self, subscriber: &str) -> Value {
        let response = self
            .signed_get(&format!("/subscriptions?subscriber_id={subscriber}"))
            .await;
        assert_eq!(response.status(), 200);
        response.json().await.unwrap()
    }
}

#[tokio::test]
async fn a_signed_request_lists_the_configured_clubs_of_a_sport_with_their_courts() {
    let api = start().await;

    let response = api.signed_get("/clubs?sport=padel").await;

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body,
        json!({
            "clubs": [{
                "id": "casa-padel",
                "name": "Casa Padel",
                "sport": "padel",
                "courts": [
                    {
                        "id": court(101),
                        "name": "Court 1 (Indoor)",
                        "surface": null,
                        "location": "indoor",
                    },
                    {
                        "id": court(106),
                        "name": "Court 6 (Outdoor)",
                        "surface": null,
                        "location": "outdoor",
                    },
                ],
            }]
        })
    );
}

fn court(id: u128) -> String {
    Uuid::from_u128(id).to_string()
}

fn signed_headers(
    secret: &str,
    timestamp: i64,
    method: &str,
    path: &str,
) -> Vec<(&'static str, String)> {
    vec![
        ("X-Webhook-Timestamp", timestamp.to_string()),
        (
            "X-Webhook-Signature-V2",
            signature(secret, timestamp, method, path, ""),
        ),
    ]
}

#[tokio::test]
async fn a_request_without_signature_headers_is_unauthorized() {
    let api = start().await;
    let path = "/clubs?sport=padel";
    let mut headers = signed_headers(API_SECRET, now(), "GET", path);

    for (name, _) in headers.clone() {
        let without: Vec<_> = headers
            .iter()
            .filter(|(n, _)| *n != name)
            .cloned()
            .collect();
        let response = api.send(reqwest::Method::GET, path, &without).await;
        assert_eq!(response.status(), 401, "accepted without {name}");
    }
    headers.clear();
    let response = api.send(reqwest::Method::GET, path, &headers).await;
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn an_unsigned_request_is_turned_away_before_its_body_is_read() {
    let api = start().await;
    let response = api
        .http
        .post(format!("{}/subscriptions", api.base))
        .body("x".repeat(65 * 1024))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn a_signed_request_with_a_body_over_64_kib_is_too_large() {
    let api = start().await;
    let response = api
        .signed_post("/subscriptions", &"x".repeat(65 * 1024))
        .await;

    assert_eq!(response.status(), 413);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"error": "body_too_large"})
    );
}

#[tokio::test(start_paused = true)]
async fn a_body_that_never_arrives_times_out() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let api = start().await;
    let address = api.base.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            format!(
                "POST /subscriptions HTTP/1.1\r\nHost: {address}\r\n\
                 X-Webhook-Timestamp: {}\r\nX-Webhook-Signature-V2: 00\r\n\
                 Content-Length: 10\r\n\r\n",
                now()
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    let mut response = vec![0; 1024];
    let read = stream.read(&mut response).await.unwrap();
    let response = String::from_utf8_lossy(&response[..read]);
    assert!(response.starts_with("HTTP/1.1 408"), "{response}");
    assert!(
        response.contains(r#"{"error":"request_timeout"}"#),
        "{response}"
    );
}

#[tokio::test]
async fn a_request_signed_with_the_wrong_secret_is_unauthorized() {
    let api = start().await;
    let path = "/clubs?sport=padel";

    let response = api
        .send(
            reqwest::Method::GET,
            path,
            &signed_headers("wrong-secret", now(), "GET", path),
        )
        .await;

    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn a_request_with_a_timestamp_outside_the_window_is_unauthorized() {
    let api = start().await;
    let path = "/clubs?sport=padel";

    for timestamp in [now() - 301, now() + 301] {
        let response = api
            .send(
                reqwest::Method::GET,
                path,
                &signed_headers(API_SECRET, timestamp, "GET", path),
            )
            .await;
        assert_eq!(response.status(), 401, "accepted timestamp {timestamp}");
    }

    let response = api
        .send(
            reqwest::Method::GET,
            path,
            &signed_headers(API_SECRET, now() - 290, "GET", path),
        )
        .await;
    assert_eq!(
        response.status(),
        200,
        "a timestamp inside the window is accepted"
    );
}

#[tokio::test]
async fn a_valid_signature_reused_for_another_path_or_method_is_unauthorized() {
    let api = start().await;
    let headers = signed_headers(API_SECRET, now(), "GET", "/clubs?sport=padel");

    let other_query = api
        .send(reqwest::Method::GET, "/clubs?sport=tennis", &headers)
        .await;
    assert_eq!(other_query.status(), 401);

    let other_path = api
        .send(
            reqwest::Method::GET,
            "/subscriptions?subscriber_id=a",
            &headers,
        )
        .await;
    assert_eq!(other_path.status(), 401);

    let other_method = api
        .send(reqwest::Method::DELETE, "/clubs?sport=padel", &headers)
        .await;
    assert_eq!(other_method.status(), 401);
}

#[tokio::test]
async fn a_signature_cannot_be_reused_by_moving_the_end_of_the_path_into_the_body() {
    let api = start().await;
    let timestamp = now();
    let signed = signature(
        API_SECRET,
        timestamp,
        "DELETE",
        "/subscriptions/1?subscriber_id=alice.smith",
        "",
    );

    let response = api
        .http
        .delete(format!("{}/subscriptions/1?subscriber_id=alice", api.base))
        .header("X-Webhook-Timestamp", timestamp.to_string())
        .header("X-Webhook-Signature-V2", signed)
        .body("smith.")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn an_unknown_or_missing_sport_is_a_bad_request() {
    let api = start().await;

    for path in ["/clubs?sport=squash", "/clubs"] {
        let response = api.signed_get(path).await;
        assert_eq!(response.status(), 400, "{path}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"], "invalid_request", "{path}: {body}");
    }
}

#[tokio::test]
async fn the_tennis_directory_lists_court_surfaces() {
    let api = start().await;

    let body: Value = api
        .signed_get("/clubs?sport=tennis")
        .await
        .json()
        .await
        .unwrap();

    let clubs = body["clubs"].as_array().unwrap();
    assert_eq!(clubs.len(), 1);
    assert_eq!(clubs[0]["id"], "zhs-munich");
    assert_eq!(
        clubs[0]["courts"][2],
        json!({
            "id": court(19),
            "name": "Court 19 - Synthetic",
            "surface": "synthetic",
            "location": null,
        })
    );
}

#[tokio::test]
async fn the_integration_signals_readiness_once_listening_and_contributes_no_admins() {
    let webhook = integration();
    assert!(webhook.admins().is_empty());
    let (mut signals, barrier) = readiness(1);

    tokio::spawn(Box::new(webhook).run(services().await, signals.remove(0)));

    tokio::time::timeout(std::time::Duration::from_secs(5), barrier.wait())
        .await
        .expect("the webhook integration never signalled readiness");
}

#[tokio::test]
async fn a_subscriber_can_subscribe_list_and_delete_their_subscriptions() {
    let api = start().await;

    let response = api
        .subscribe(json!({
            "subscriber_id": "alice",
            "sport": "padel",
            "club": "casa-padel",
            "weekday": "Thu",
            "start": "18:00",
            "end": "21:30",
            "filter": "indoor",
        }))
        .await;
    assert_eq!(response.status(), 201);
    let created: Value = response.json().await.unwrap();
    let id = created["subscription"]["id"].as_i64().expect("an id");
    assert_eq!(
        created,
        json!({
            "subscription": {
                "id": id,
                "sport": "padel",
                "club": "casa-padel",
                "club_name": "Casa Padel",
                "weekday": "Thu",
                "start": "18:00",
                "end": "21:30",
                "courts": null,
                "filter": "indoor",
            },
            "open_slots": [],
        })
    );

    assert_eq!(
        api.subscriptions_of("alice").await,
        json!({ "subscriptions": [created["subscription"]] })
    );

    let deleted = api
        .signed_delete(&format!("/subscriptions/{id}?subscriber_id=alice"))
        .await;
    assert_eq!(deleted.status(), 204);
    assert_eq!(
        api.subscriptions_of("alice").await,
        json!({ "subscriptions": [] })
    );
}

#[tokio::test]
async fn subscribing_returns_the_open_slots_that_already_match() {
    use chrono::{Days, TimeZone};
    use chrono_tz::Europe::Berlin;

    let tomorrow = crate::time::today_berlin()
        .checked_add_days(Days::new(1))
        .unwrap();
    let starts_at = Berlin
        .from_local_datetime(&tomorrow.and_hms_opt(10, 0, 0).unwrap())
        .single()
        .unwrap()
        .with_timezone(&chrono::Utc);
    let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
    store
        .replace_venue_snapshot(
            &VenueId::new("casa-padel"),
            vec![BookableSlot {
                venue_id: VenueId::new("casa-padel"),
                court_id: Uuid::from_u128(101),
                court_name: "Court 1 (Indoor)".into(),
                starts_at,
                ends_at: starts_at + chrono::Duration::minutes(90),
                available_places: 4,
            }],
        )
        .await
        .unwrap();
    let api = serve(services_over(store)).await;

    let response = api
        .subscribe(json!({
            "subscriber_id": "alice",
            "sport": "padel",
            "date": tomorrow.to_string(),
            "start": "09:00",
            "end": "12:00",
        }))
        .await;

    assert_eq!(response.status(), 201);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["subscription"]["date"], tomorrow.to_string());
    assert_eq!(
        body["open_slots"],
        json!([{
            "club": "Casa Padel",
            "court": "Court 1 (Indoor)",
            "court_id": "00000000-0000-0000-0000-000000000065",
            "starts_at": starts_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "ends_at": (starts_at + chrono::Duration::minutes(90))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        }])
    );
}

fn tennis_subscription(subscriber_id: &str) -> Value {
    json!({
        "subscriber_id": subscriber_id,
        "sport": "tennis",
        "weekday": "Tue",
        "start": "08:00",
        "end": "10:00",
    })
}

#[tokio::test]
async fn one_subscriber_cannot_see_or_delete_another_subscribers_subscriptions() {
    let api = start().await;
    let created: Value = api
        .subscribe(tennis_subscription("alice"))
        .await
        .json()
        .await
        .unwrap();
    let id = created["subscription"]["id"].as_i64().unwrap();

    assert_eq!(
        api.subscriptions_of("bob").await,
        json!({ "subscriptions": [] })
    );

    let foreign = api
        .signed_delete(&format!("/subscriptions/{id}?subscriber_id=bob"))
        .await;
    assert_eq!(foreign.status(), 404);
    let missing = api
        .signed_delete(&format!("/subscriptions/{}?subscriber_id=bob", id + 1))
        .await;
    assert_eq!(missing.status(), 404);
    assert_eq!(
        foreign.json::<Value>().await.unwrap(),
        missing.json::<Value>().await.unwrap(),
        "deleting someone else's subscription looks like deleting a missing one"
    );

    assert_eq!(
        api.subscriptions_of("alice").await["subscriptions"],
        json!([created["subscription"]])
    );
}

#[tokio::test]
async fn a_percent_encoded_subscriber_id_lists_and_deletes_what_its_raw_form_created() {
    let api = start().await;
    let raw = "signal:+49151 #&%";
    let encoded = "signal%3A%2B49151%20%23%26%25";
    let created: Value = api
        .subscribe(tennis_subscription(raw))
        .await
        .json()
        .await
        .unwrap();
    let id = created["subscription"]["id"].as_i64().unwrap();

    assert_eq!(
        api.subscriptions_of(encoded).await["subscriptions"],
        json!([created["subscription"]])
    );
    let deleted = api
        .signed_delete(&format!("/subscriptions/{id}?subscriber_id={encoded}"))
        .await;
    assert_eq!(deleted.status(), 204);
}

#[tokio::test]
async fn an_empty_or_overlong_subscriber_id_is_rejected() {
    let api = start().await;
    let overlong = "x".repeat(129);

    for subscriber_id in ["", overlong.as_str()] {
        let response = api.subscribe(tennis_subscription(subscriber_id)).await;
        assert_eq!(response.status(), 422, "subscribe as {subscriber_id:?}");
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({ "error": "invalid_subscriber_id" })
        );
        for response in [
            api.signed_get(&format!("/subscriptions?subscriber_id={subscriber_id}"))
                .await,
            api.signed_delete(&format!("/subscriptions/1?subscriber_id={subscriber_id}"))
                .await,
        ] {
            assert_eq!(
                response.status(),
                422,
                "list or delete as {subscriber_id:?}"
            );
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"],
                "invalid_subscriber_id"
            );
        }
    }

    let longest = "é".repeat(128);
    let response = api.subscribe(tennis_subscription(&longest)).await;
    assert_eq!(response.status(), 201, "128 characters are allowed");
}

#[tokio::test]
async fn courts_are_chosen_by_id_within_the_club() {
    let api = start().await;
    let request = json!({
        "subscriber_id": "alice",
        "sport": "padel",
        "club": "casa-padel",
        "weekday": "Thu",
        "start": "18:00",
        "end": "21:30",
        "courts": [court(106), court(106)],
    });

    let response = api.subscribe(request).await;

    assert_eq!(response.status(), 201);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["subscription"]["courts"],
        json!([{"id": court(106), "name": "Court 6 (Outdoor)"}])
    );
}

fn club_with_two_centre_courts() -> Arc<std::sync::RwLock<VenueRegistry>> {
    let mut registry = VenueRegistry::new();
    registry.register(&padel_club());
    registry.set_catalog(
        &club_id(),
        CourtCatalog::new(vec![
            centre_court(1, "Centre Court"),
            centre_court(2, "Centre Court"),
        ]),
    );
    Arc::new(std::sync::RwLock::new(registry))
}

fn club_id() -> VenueId {
    VenueId::new("club")
}

fn padel_club() -> Venue {
    Venue {
        id: club_id(),
        display_name: "Club".into(),
        sport: Sport::Padel,
        identity: VenueIdentity::Playtomic {
            tenant_id: Uuid::nil(),
            slug: "club".into(),
        },
        poll_interval_secs: None,
        lookahead_days: None,
        operating_window: None,
    }
}

fn centre_court(id: u128, name: &str) -> Court {
    Court::new(
        Uuid::from_u128(id),
        name.into(),
        CourtAttributes::padel(None),
    )
}

fn centre_court_slot(id: u128, name: &str) -> BookableSlot {
    BookableSlot {
        venue_id: club_id(),
        ..tuesday_evening_padel(id, name)
    }
}

fn subscribe_to_court_one(subscriber_id: &str) -> Value {
    json!({
        "subscriber_id": subscriber_id,
        "sport": "padel",
        "club": "club",
        "weekday": "Tue",
        "start": "00:00",
        "end": "24:00",
        "courts": [court(1)],
    })
}

#[tokio::test]
async fn a_court_chosen_through_the_api_alerts_by_identity() {
    let receiver = receiver_accepting_alerts().await;
    let registry = club_with_two_centre_courts();
    let services = services_with(
        Arc::new(SqliteStore::open_in_memory().await.unwrap()),
        registry.clone(),
    );
    services.subscriptions.register_sender(
        INTEGRATION,
        Arc::new(
            WebhookAlertSender::new(
                format!("{}{ALERT_PATH}", receiver.uri()).parse().unwrap(),
                ALERT_SECRET.into(),
            )
            .unwrap(),
        ),
    );
    let api = serve(services.clone()).await;

    let created = api.subscribe(subscribe_to_court_one("alice")).await;
    assert_eq!(created.status(), 201);
    let created: Value = created.json().await.unwrap();
    assert_eq!(
        created["subscription"]["courts"],
        json!([{"id": court(1), "name": "Centre Court"}])
    );

    opens(&services, centre_court_slot(2, "Centre Court")).await;
    assert!(
        alerts_received(&receiver).await.is_empty(),
        "the other court of that name alerted"
    );

    opens(&services, centre_court_slot(1, "Centre Court")).await;
    let received = alerts_received(&receiver).await;
    assert_eq!(received.len(), 1);
    let alert: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(alert["slots"][0]["court_id"], court(1));

    registry.write().unwrap().set_catalog(
        &club_id(),
        CourtCatalog::new(vec![
            centre_court(1, "Show Court"),
            centre_court(2, "Centre Court"),
        ]),
    );
    assert_eq!(
        api.subscriptions_of("alice").await["subscriptions"][0]["courts"],
        json!([{"id": court(1), "name": "Show Court"}]),
        "the listing follows the rename"
    );

    opens(
        &services,
        BookableSlot {
            starts_at: centre_court_slot(1, "Show Court").starts_at + chrono::Duration::days(7),
            ends_at: centre_court_slot(1, "Show Court").ends_at + chrono::Duration::days(7),
            ..centre_court_slot(1, "Show Court")
        },
    )
    .await;
    assert_eq!(
        alerts_received(&receiver).await.len(),
        2,
        "the renamed court stopped alerting"
    );
}

#[tokio::test]
async fn courts_sharing_a_name_can_both_be_chosen_and_stay_apart() {
    let api = serve(services_with(
        Arc::new(SqliteStore::open_in_memory().await.unwrap()),
        club_with_two_centre_courts(),
    ))
    .await;

    let response = api
        .subscribe(with(
            subscribe_to_court_one("alice"),
            json!({"courts": [court(2), court(1), court(2)]}),
        ))
        .await;

    assert_eq!(response.status(), 201);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["subscription"]["courts"],
        json!([
            {"id": court(2), "name": "Centre Court"},
            {"id": court(1), "name": "Centre Court"},
        ])
    );
}

#[tokio::test]
async fn listing_keeps_the_id_of_a_court_whose_name_is_not_known_right_now() {
    let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
    let registry = club_with_two_centre_courts();
    let api = serve(services_with(store.clone(), registry.clone())).await;
    let response = api.subscribe(subscribe_to_court_one("alice")).await;
    assert_eq!(response.status(), 201);

    registry.write().unwrap().set_catalog(
        &club_id(),
        CourtCatalog::new(vec![centre_court(2, "Centre Court")]),
    );
    assert_eq!(
        api.subscriptions_of("alice").await["subscriptions"][0]["courts"],
        json!([{"id": court(1), "name": null}]),
        "the court left the catalog"
    );

    let mut unresolved = VenueRegistry::new();
    unresolved.register(&padel_club());
    let restarted = serve(services_with(
        store,
        Arc::new(std::sync::RwLock::new(unresolved)),
    ))
    .await;
    assert_eq!(
        restarted.subscriptions_of("alice").await["subscriptions"][0]["courts"],
        json!([{"id": court(1), "name": null}]),
        "the catalog is not ready after a restart"
    );
}

#[tokio::test]
async fn listing_shows_a_selection_stored_by_name_without_an_id() {
    let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
    store
        .add(
            UncheckedDraft {
                subscriber: subscriber("alice"),
                sport: Sport::Tennis,
                venue: None,
                schedule: Schedule::Weekday(chrono::Weekday::Tue),
                time_range: TimeRange::new(0, 24 * 60).unwrap(),
                courts: CourtSelection::PendingNames(vec!["Court 2".into()]),
                filter: CourtFilter::Any,
            }
            .assume_valid(),
        )
        .await
        .unwrap();
    let api = serve(services_over(store)).await;

    assert_eq!(
        api.subscriptions_of("alice").await["subscriptions"][0]["courts"],
        json!([{"id": null, "name": "Court 2"}])
    );
}

fn with(mut base: Value, changes: Value) -> Value {
    for (key, value) in changes.as_object().unwrap() {
        if value.is_null() {
            base.as_object_mut().unwrap().remove(key);
        } else {
            base[key] = value.clone();
        }
    }
    base
}

#[tokio::test]
async fn each_validation_rejection_is_unprocessable_with_its_details() {
    let api = start().await;
    let tennis = tennis_subscription("alice");
    let cases = [
        (
            with(tennis.clone(), json!({"start": "10:00", "end": "08:00"})),
            json!({"error": "invalid_time_range"}),
        ),
        (
            with(
                tennis.clone(),
                json!({"weekday": null, "date": "2020-01-01"}),
            ),
            json!({"error": "invalid_schedule"}),
        ),
        (
            with(tennis.clone(), json!({"weekday": null})),
            json!({"error": "invalid_schedule"}),
        ),
        (
            with(tennis.clone(), json!({"date": "2030-01-01"})),
            json!({"error": "invalid_schedule"}),
        ),
        (
            with(
                tennis.clone(),
                json!({"club": "zhs-munich", "courts": [court(2), court(101)]}),
            ),
            json!({
                "error": "unknown_courts",
                "unknown": [court(101)],
                "available": [
                    {"id": court(2), "name": "Court 2"},
                    {"id": court(5), "name": "Court 5"},
                    {"id": court(19), "name": "Court 19 - Synthetic"},
                ],
            }),
        ),
        (
            with(tennis.clone(), json!({"courts": [court(2)]})),
            json!({"error": "courts_need_club"}),
        ),
        (
            with(tennis.clone(), json!({"club": "nowhere"})),
            json!({
                "error": "unknown_club",
                "unknown": "nowhere",
                "available": [{"id": "zhs-munich", "name": "ZHS München"}],
            }),
        ),
        (
            with(
                tennis.clone(),
                json!({"club": "zhs-munich", "courts": [court(19)], "filter": "clay"}),
            ),
            json!({
                "error": "filter_excludes_courts",
                "courts": ["Court 19 - Synthetic"],
                "filter": "clay",
            }),
        ),
        (
            with(
                tennis.clone(),
                json!({"club": "zhs-munich", "courts": vec![court(2); 21]}),
            ),
            json!({"error": "too_many_courts", "max": 20}),
        ),
        (
            with(tennis.clone(), json!({"filter": "indoor"})),
            json!({"error": "filter_not_for_sport", "sport": "tennis", "filter": "indoor"}),
        ),
    ];

    for (request, expected) in cases {
        let response = api.subscribe(request.clone()).await;
        assert_eq!(response.status(), 422, "{request}");
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            expected,
            "{request}"
        );
    }
}

#[tokio::test]
async fn subscribing_to_a_sport_without_clubs_is_unprocessable() {
    let api = serve(services_with(
        Arc::new(SqliteStore::open_in_memory().await.unwrap()),
        Arc::new(std::sync::RwLock::new(VenueRegistry::new())),
    ))
    .await;

    let response = api.subscribe(tennis_subscription("alice")).await;

    assert_eq!(response.status(), 422);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"error": "no_clubs_configured", "sport": "tennis"})
    );
}

#[tokio::test]
async fn malformed_requests_are_bad_requests() {
    let api = start().await;
    let tennis = tennis_subscription("alice");

    for body in [
        "{not json".to_owned(),
        with(tennis.clone(), json!({"subscriber_id": null})).to_string(),
        with(tennis.clone(), json!({"sport": "squash"})).to_string(),
        with(tennis.clone(), json!({"weekday": "Funday"})).to_string(),
        with(tennis.clone(), json!({"start": "8"})).to_string(),
        with(tennis.clone(), json!({"end": "24:30"})).to_string(),
        with(tennis.clone(), json!({"filter": "grass"})).to_string(),
        with(
            tennis.clone(),
            json!({"club": "zhs-munich", "courts": ["Court 2"]}),
        )
        .to_string(),
        with(tennis.clone(), json!({"surprise": true})).to_string(),
    ] {
        let response = api.signed_post("/subscriptions", &body).await;
        assert_eq!(response.status(), 400, "{body}");
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"],
            "invalid_request",
            "{body}"
        );
    }

    for path in [
        "/subscriptions",
        "/subscriptions/abc?subscriber_id=alice",
        "/subscriptions/1",
    ] {
        let response = if path == "/subscriptions" {
            api.signed_get(path).await
        } else {
            api.signed_delete(path).await
        };
        assert_eq!(response.status(), 400, "{path}");
    }
}

#[tokio::test]
async fn a_subscription_may_end_at_midnight() {
    let api = start().await;

    let response = api
        .subscribe(with(
            tennis_subscription("alice"),
            json!({"start": "22:00", "end": "24:00"}),
        ))
        .await;

    assert_eq!(response.status(), 201);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["subscription"]["end"], "24:00");
}

const ALERT_SECRET: &str = "alert-secret";
const ALERT_PATH: &str = "/webhooks/court-alert";

async fn run_against(receiver: &MockServer) -> IntegrationServices {
    let services = services().await;
    let webhook = WebhookIntegration::new(WebhookSettings {
        listen: "127.0.0.1:0".parse().unwrap(),
        api_secret: API_SECRET.into(),
        alert_url: format!("{}{ALERT_PATH}", receiver.uri()).parse().unwrap(),
        alert_secret: ALERT_SECRET.into(),
    });
    let (mut signals, barrier) = readiness(1);
    tokio::spawn(Box::new(webhook).run(services.clone(), signals.remove(0)));
    tokio::time::timeout(std::time::Duration::from_secs(5), barrier.wait())
        .await
        .expect("the webhook integration never signalled readiness");
    services
}

fn subscriber(subscriber_id: &str) -> SubscriberRef {
    SubscriberRef {
        integration: INTEGRATION,
        subscriber_id: subscriber_id.into(),
    }
}

async fn subscribe_to_padel_on_tuesday_evenings(
    services: &IntegrationServices,
    subscriber: &SubscriberRef,
) {
    services
        .subscriptions
        .handle(
            subscriber,
            SubscriptionCommand::Subscribe {
                sport: Sport::Padel,
                venue: None,
                schedule: Schedule::Weekday(chrono::Weekday::Tue),
                start_minute: 18 * 60,
                end_minute: 22 * 60,
                courts: None,
                filter: None,
            },
        )
        .await
        .unwrap();
}

fn tuesday_evening_padel(court_id: u128, court_name: &str) -> BookableSlot {
    use chrono::TimeZone;
    let starts_at = chrono::Utc.with_ymd_and_hms(2026, 6, 2, 18, 0, 0).unwrap();
    BookableSlot {
        venue_id: VenueId::new("casa-padel"),
        court_id: Uuid::from_u128(court_id),
        court_name: court_name.into(),
        starts_at,
        ends_at: starts_at + chrono::Duration::minutes(90),
        available_places: 4,
    }
}

fn indoor_slot() -> BookableSlot {
    tuesday_evening_padel(101, "Court 1 (Indoor)")
}

fn outdoor_slot() -> BookableSlot {
    tuesday_evening_padel(106, "Court 6 (Outdoor)")
}

async fn opens(services: &IntegrationServices, slot: BookableSlot) {
    services
        .subscriptions
        .dispatch(&[AvailabilityChange::BecameBookable(slot)])
        .await;
}

async fn receiver_accepting_alerts() -> MockServer {
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ALERT_PATH))
        .respond_with(ResponseTemplate::new(202))
        .mount(&receiver)
        .await;
    receiver
}

async fn alerts_received(receiver: &MockServer) -> Vec<Request> {
    receiver.received_requests().await.unwrap_or_default()
}

fn alerted_subscriber(alert: &Request) -> String {
    let body: Value = serde_json::from_slice(&alert.body).unwrap();
    body["subscriber_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn matching_slots_reach_the_alert_url_in_one_alert_per_subscriber() {
    let receiver = receiver_accepting_alerts().await;
    let services = run_against(&receiver).await;
    subscribe_to_padel_on_tuesday_evenings(&services, &subscriber("alice")).await;

    services
        .subscriptions
        .dispatch(&[
            AvailabilityChange::BecameBookable(indoor_slot()),
            AvailabilityChange::BecameBookable(outdoor_slot()),
        ])
        .await;

    let received = alerts_received(&receiver).await;
    assert_eq!(received.len(), 1);
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        body,
        json!({
            "subscriber_id": "alice",
            "slots": [
                {
                    "club": "Casa Padel",
                    "court": "Court 1 (Indoor)",
                    "court_id": "00000000-0000-0000-0000-000000000065",
                    "starts_at": "2026-06-02T18:00:00Z",
                    "ends_at": "2026-06-02T19:30:00Z",
                },
                {
                    "club": "Casa Padel",
                    "court": "Court 6 (Outdoor)",
                    "court_id": "00000000-0000-0000-0000-00000000006a",
                    "starts_at": "2026-06-02T18:00:00Z",
                    "ends_at": "2026-06-02T19:30:00Z",
                },
            ],
        })
    );
}

#[tokio::test]
async fn an_alert_is_signed_with_the_alert_secret() {
    let receiver = receiver_accepting_alerts().await;
    let services = run_against(&receiver).await;
    subscribe_to_padel_on_tuesday_evenings(&services, &subscriber("alice")).await;

    opens(&services, indoor_slot()).await;

    let received = alerts_received(&receiver).await;
    let alert = &received[0];
    let header = |name: &str| {
        alert
            .headers
            .get(name)
            .unwrap_or_else(|| panic!("no {name} header"))
            .to_str()
            .unwrap()
            .to_owned()
    };
    let timestamp = header("X-Webhook-Timestamp");
    assert!(
        now().abs_diff(timestamp.parse::<i64>().unwrap()) <= 5,
        "timestamp {timestamp} is not now"
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(ALERT_SECRET.as_bytes()).unwrap();
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(&alert.body);
    mac.verify_slice(&hex::decode(header("X-Webhook-Signature-V2")).unwrap())
        .expect("the signature verifies with the alert secret");
    assert_eq!(header("Content-Type"), "application/json");
}

#[tokio::test]
async fn each_delivery_carries_its_own_webhook_id() {
    let receiver = receiver_accepting_alerts().await;
    let services = run_against(&receiver).await;
    subscribe_to_padel_on_tuesday_evenings(&services, &subscriber("alice")).await;
    subscribe_to_padel_on_tuesday_evenings(&services, &subscriber("bob")).await;

    opens(&services, indoor_slot()).await;
    opens(&services, outdoor_slot()).await;

    let ids: HashSet<Uuid> = alerts_received(&receiver)
        .await
        .iter()
        .map(|alert| {
            let id = alert
                .headers
                .get("webhook-id")
                .expect("a webhook-id header");
            Uuid::parse_str(id.to_str().unwrap()).expect("a UUID")
        })
        .collect();
    assert_eq!(ids.len(), 4, "webhook-ids repeat: {ids:?}");
}

#[tokio::test]
async fn the_receiver_rejecting_one_subscribers_alert_does_not_stop_the_others() {
    let receiver = MockServer::start().await;
    Mock::given(body_partial_json(json!({"subscriber_id": "alice"})))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&receiver)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&receiver)
        .await;
    let services = run_against(&receiver).await;
    for id in ["alice", "bob", "carol"] {
        subscribe_to_padel_on_tuesday_evenings(&services, &subscriber(id)).await;
    }

    opens(&services, indoor_slot()).await;

    let mut users: Vec<String> = alerts_received(&receiver)
        .await
        .iter()
        .map(alerted_subscriber)
        .collect();
    users.sort();
    assert_eq!(users, ["alice", "bob", "carol"]);
}

#[tokio::test]
async fn a_slot_that_is_taken_sends_no_alert() {
    let receiver = receiver_accepting_alerts().await;
    let services = run_against(&receiver).await;
    subscribe_to_padel_on_tuesday_evenings(&services, &subscriber("alice")).await;

    services
        .subscriptions
        .dispatch(&[AvailabilityChange::BecameUnbookable(indoor_slot())])
        .await;

    assert!(alerts_received(&receiver).await.is_empty());
}

#[derive(Default)]
struct DiscordDms {
    sent: std::sync::Mutex<Vec<String>>,
    struck: std::sync::Mutex<Vec<BookableSlotId>>,
}

#[async_trait::async_trait]
impl AlertSender for DiscordDms {
    async fn send_alert(
        &self,
        subscriber_id: &str,
        _alert: &AvailabilityAlert,
    ) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(subscriber_id.to_owned());
        Ok(())
    }

    async fn strike_taken(&self, slots: &[BookableSlotId]) -> anyhow::Result<()> {
        self.struck.lock().unwrap().extend_from_slice(slots);
        Ok(())
    }
}

#[tokio::test]
async fn discord_dms_keep_flowing_alongside_webhook_alerts() {
    let receiver = receiver_accepting_alerts().await;
    let services = run_against(&receiver).await;
    let dms = Arc::new(DiscordDms::default());
    services
        .subscriptions
        .register_sender("discord".into(), dms.clone());
    let discord_subscriber = SubscriberRef {
        integration: "discord".into(),
        subscriber_id: "42".into(),
    };
    subscribe_to_padel_on_tuesday_evenings(&services, &discord_subscriber).await;
    subscribe_to_padel_on_tuesday_evenings(&services, &subscriber("alice")).await;

    opens(&services, indoor_slot()).await;
    services
        .subscriptions
        .dispatch(&[AvailabilityChange::BecameUnbookable(indoor_slot())])
        .await;

    let received = alerts_received(&receiver).await;
    assert_eq!(
        received.iter().map(alerted_subscriber).collect::<Vec<_>>(),
        ["alice"]
    );
    assert_eq!(*dms.sent.lock().unwrap(), ["42"]);
    assert_eq!(
        *dms.struck.lock().unwrap(),
        [BookableSlotId::from(&indoor_slot())]
    );
}
