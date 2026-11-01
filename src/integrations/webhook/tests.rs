use std::collections::HashSet;
use std::sync::Arc;

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

use super::{INTEGRATION, WebhookIntegration};
use crate::config::WebhookSettings;
use crate::integrations::{Integration, readiness};
use crate::model::{
    AvailabilityChange, BookableSlot, BookableSlotId, CourtFilter, Schedule, Sport, SubscriberRef,
    VenueId,
};
use crate::ports::BookableSlotSnapshotRepository;
use crate::store::SqliteStore;
use crate::subscriptions::contract::{AlertSender, AvailabilityAlert, SubscriptionCommand};
use crate::subscriptions::testing::registry;
use crate::subscriptions::{BerlinClock, SubscriptionService};
use uuid::Uuid;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const API_SECRET: &str = "api-secret";

struct Api {
    base: String,
    http: reqwest::Client,
}

async fn service() -> Arc<SubscriptionService> {
    service_over(Arc::new(SqliteStore::open_in_memory().await.unwrap()))
}

fn service_over(store: Arc<SqliteStore>) -> Arc<SubscriptionService> {
    service_with(store, registry())
}

fn service_with(
    store: Arc<SqliteStore>,
    registry: Arc<std::sync::RwLock<crate::model::VenueRegistry>>,
) -> Arc<SubscriptionService> {
    Arc::new(SubscriptionService::new(
        store.clone(),
        store,
        HashSet::new(),
        registry,
        CourtFilter::Any,
        Arc::new(BerlinClock),
    ))
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
    serve(service().await).await
}

async fn serve(service: Arc<SubscriptionService>) -> Api {
    let webhook = integration().bind().await.expect("bind webhook listener");
    let base = format!("http://{}", webhook.local_addr().unwrap());
    tokio::spawn(webhook.serve(service));
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
    mac.update(format!("{timestamp}.{method}.{path}.{body}").as_bytes());
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

    async fn subscriptions_of(&self, user: &str) -> Value {
        let response = self
            .signed_get(&format!("/subscriptions?user={user}"))
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
                    {"name": "Court 1 (Indoor)", "surface": null, "location": "indoor"},
                    {"name": "Court 6 (Outdoor)", "surface": null, "location": "outdoor"},
                ],
            }]
        })
    );
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
        .send(reqwest::Method::GET, "/subscriptions?user=a", &headers)
        .await;
    assert_eq!(other_path.status(), 401);

    let other_method = api
        .send(reqwest::Method::DELETE, "/clubs?sport=padel", &headers)
        .await;
    assert_eq!(other_method.status(), 401);
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
        json!({"name": "Court 19 - Synthetic", "surface": "synthetic", "location": null})
    );
}

#[tokio::test]
async fn the_integration_signals_readiness_once_listening_and_contributes_no_admins() {
    let webhook = integration();
    assert!(webhook.admins().is_empty());
    let (mut signals, barrier) = readiness(1);

    tokio::spawn(Box::new(webhook).run(service().await, signals.remove(0)));

    tokio::time::timeout(std::time::Duration::from_secs(5), barrier.wait())
        .await
        .expect("the webhook integration never signalled readiness");
}

#[tokio::test]
async fn a_subscriber_can_subscribe_list_and_delete_their_subscriptions() {
    let api = start().await;

    let response = api
        .subscribe(json!({
            "user_id": "alice",
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
                "club": "Casa Padel",
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
        .signed_delete(&format!("/subscriptions/{id}?user=alice"))
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
    let api = serve(service_over(store)).await;

    let response = api
        .subscribe(json!({
            "user_id": "alice",
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

fn tennis_subscription(user_id: &str) -> Value {
    json!({
        "user_id": user_id,
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
        .signed_delete(&format!("/subscriptions/{id}?user=bob"))
        .await;
    assert_eq!(foreign.status(), 404);
    let missing = api
        .signed_delete(&format!("/subscriptions/{}?user=bob", id + 1))
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
async fn an_empty_or_overlong_user_id_is_rejected() {
    let api = start().await;
    let overlong = "x".repeat(129);

    for user_id in ["", overlong.as_str()] {
        let response = api.subscribe(tennis_subscription(user_id)).await;
        assert_eq!(response.status(), 422, "subscribe as {user_id:?}");
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({ "error": "invalid_user_id" })
        );
        for response in [
            api.signed_get(&format!("/subscriptions?user={user_id}"))
                .await,
            api.signed_delete(&format!("/subscriptions/1?user={user_id}"))
                .await,
        ] {
            assert_eq!(response.status(), 422, "list or delete as {user_id:?}");
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"],
                "invalid_user_id"
            );
        }
    }

    let longest = "é".repeat(128);
    let response = api.subscribe(tennis_subscription(&longest)).await;
    assert_eq!(response.status(), 201, "128 characters are allowed");
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
            with(tennis.clone(), json!({"courts": ["Court 2", "Court 99"]})),
            json!({
                "error": "unknown_courts",
                "unknown": ["Court 99"],
                "available": ["Court 2", "Court 5", "Court 19 - Synthetic"],
            }),
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
                json!({"courts": ["Court 19 - Synthetic"], "filter": "clay"}),
            ),
            json!({
                "error": "filter_excludes_courts",
                "courts": ["Court 19 - Synthetic"],
                "filter": "clay",
            }),
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
    let api = serve(service_with(
        Arc::new(SqliteStore::open_in_memory().await.unwrap()),
        Arc::new(std::sync::RwLock::new(crate::model::VenueRegistry::new())),
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
        with(tennis.clone(), json!({"user_id": null})).to_string(),
        with(tennis.clone(), json!({"sport": "squash"})).to_string(),
        with(tennis.clone(), json!({"weekday": "Funday"})).to_string(),
        with(tennis.clone(), json!({"start": "8"})).to_string(),
        with(tennis.clone(), json!({"end": "24:30"})).to_string(),
        with(tennis.clone(), json!({"filter": "grass"})).to_string(),
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
        "/subscriptions/abc?user=alice",
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

async fn run_against(hermes: &MockServer) -> Arc<SubscriptionService> {
    let service = service().await;
    let webhook = WebhookIntegration::new(WebhookSettings {
        listen: "127.0.0.1:0".parse().unwrap(),
        api_secret: API_SECRET.into(),
        alert_url: format!("{}{ALERT_PATH}", hermes.uri()).parse().unwrap(),
        alert_secret: ALERT_SECRET.into(),
    });
    let (mut signals, barrier) = readiness(1);
    tokio::spawn(Box::new(webhook).run(service.clone(), signals.remove(0)));
    tokio::time::timeout(std::time::Duration::from_secs(5), barrier.wait())
        .await
        .expect("the webhook integration never signalled readiness");
    service
}

fn subscriber(user_id: &str) -> SubscriberRef {
    SubscriberRef {
        integration: INTEGRATION,
        user_id: user_id.into(),
    }
}

async fn subscribe_to_padel_on_tuesday_evenings(
    service: &SubscriptionService,
    user: &SubscriberRef,
) {
    service
        .handle(
            user,
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

async fn opens(service: &SubscriptionService, slot: BookableSlot) {
    service
        .dispatch(&[AvailabilityChange::BecameBookable(slot)])
        .await;
}

async fn hermes_accepting_alerts() -> MockServer {
    let hermes = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ALERT_PATH))
        .respond_with(ResponseTemplate::new(202))
        .mount(&hermes)
        .await;
    hermes
}

async fn alerts_received(hermes: &MockServer) -> Vec<Request> {
    hermes.received_requests().await.unwrap_or_default()
}

fn alerted_user(alert: &Request) -> String {
    let body: Value = serde_json::from_slice(&alert.body).unwrap();
    body["user_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn matching_slots_reach_hermes_in_one_alert_per_subscriber() {
    let hermes = hermes_accepting_alerts().await;
    let service = run_against(&hermes).await;
    subscribe_to_padel_on_tuesday_evenings(&service, &subscriber("alice")).await;

    service
        .dispatch(&[
            AvailabilityChange::BecameBookable(indoor_slot()),
            AvailabilityChange::BecameBookable(outdoor_slot()),
        ])
        .await;

    let received = alerts_received(&hermes).await;
    assert_eq!(received.len(), 1);
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        body,
        json!({
            "user_id": "alice",
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
async fn an_alert_is_signed_for_hermes_with_the_alert_secret() {
    let hermes = hermes_accepting_alerts().await;
    let service = run_against(&hermes).await;
    subscribe_to_padel_on_tuesday_evenings(&service, &subscriber("alice")).await;

    opens(&service, indoor_slot()).await;

    let received = alerts_received(&hermes).await;
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
    let hermes = hermes_accepting_alerts().await;
    let service = run_against(&hermes).await;
    subscribe_to_padel_on_tuesday_evenings(&service, &subscriber("alice")).await;
    subscribe_to_padel_on_tuesday_evenings(&service, &subscriber("bob")).await;

    opens(&service, indoor_slot()).await;
    opens(&service, outdoor_slot()).await;

    let ids: HashSet<Uuid> = alerts_received(&hermes)
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
async fn hermes_rejecting_one_subscribers_alert_does_not_stop_the_others() {
    let hermes = MockServer::start().await;
    Mock::given(body_partial_json(json!({"user_id": "alice"})))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&hermes)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&hermes)
        .await;
    let service = run_against(&hermes).await;
    for user in ["alice", "bob", "carol"] {
        subscribe_to_padel_on_tuesday_evenings(&service, &subscriber(user)).await;
    }

    opens(&service, indoor_slot()).await;

    let mut users: Vec<String> = alerts_received(&hermes)
        .await
        .iter()
        .map(alerted_user)
        .collect();
    users.sort();
    assert_eq!(users, ["alice", "bob", "carol"]);
}

#[tokio::test]
async fn a_slot_that_is_taken_sends_hermes_nothing() {
    let hermes = hermes_accepting_alerts().await;
    let service = run_against(&hermes).await;
    subscribe_to_padel_on_tuesday_evenings(&service, &subscriber("alice")).await;

    service
        .dispatch(&[AvailabilityChange::BecameUnbookable(indoor_slot())])
        .await;

    assert!(alerts_received(&hermes).await.is_empty());
}

#[derive(Default)]
struct DiscordDms {
    sent: std::sync::Mutex<Vec<String>>,
    struck: std::sync::Mutex<Vec<BookableSlotId>>,
}

#[async_trait::async_trait]
impl AlertSender for DiscordDms {
    async fn send_alert(&self, user_id: &str, _alert: &AvailabilityAlert) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(user_id.to_owned());
        Ok(())
    }

    async fn strike_taken(&self, slots: &[BookableSlotId]) -> anyhow::Result<()> {
        self.struck.lock().unwrap().extend_from_slice(slots);
        Ok(())
    }
}

#[tokio::test]
async fn discord_dms_keep_flowing_alongside_webhook_alerts() {
    let hermes = hermes_accepting_alerts().await;
    let service = run_against(&hermes).await;
    let dms = Arc::new(DiscordDms::default());
    service.register_sender("discord".into(), dms.clone());
    let discord_user = SubscriberRef {
        integration: "discord".into(),
        user_id: "42".into(),
    };
    subscribe_to_padel_on_tuesday_evenings(&service, &discord_user).await;
    subscribe_to_padel_on_tuesday_evenings(&service, &subscriber("alice")).await;

    opens(&service, indoor_slot()).await;
    service
        .dispatch(&[AvailabilityChange::BecameUnbookable(indoor_slot())])
        .await;

    let received = alerts_received(&hermes).await;
    assert_eq!(
        received.iter().map(alerted_user).collect::<Vec<_>>(),
        ["alice"]
    );
    assert_eq!(*dms.sent.lock().unwrap(), ["42"]);
    assert_eq!(
        *dms.struck.lock().unwrap(),
        [BookableSlotId::from(&indoor_slot())]
    );
}

const HELPER_SUBSCRIBER: &str = "telegram:42";

struct HelperResponse {
    status: std::process::ExitStatus,
    body: Value,
}

async fn run_helper(api: &Api, args: &[&str]) -> HelperResponse {
    let output = tokio::process::Command::new("bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/hermes/skills/court-alert/scripts/court-alert.sh"
        ))
        .args(args)
        .env("COURT_ALERT_URL", &api.base)
        .env("COURT_ALERT_API_SECRET", API_SECRET)
        .output()
        .await
        .expect("run the Hermes helper");
    let stdout = String::from_utf8(output.stdout).unwrap();
    HelperResponse {
        status: output.status,
        body: if stdout.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout:?}"))
        },
    }
}

#[tokio::test]
async fn the_hermes_helper_signs_requests_the_api_accepts() {
    let api = start().await;

    let clubs = run_helper(&api, &["GET", "/clubs?sport=padel"]).await;
    assert!(clubs.status.success());
    assert_eq!(clubs.body["clubs"][0]["id"], "casa-padel");

    let subscription = json!({
        "user_id": HELPER_SUBSCRIBER,
        "sport": "padel",
        "club": "casa-padel",
        "weekday": "Thu",
        "start": "18:00",
        "end": "21:30",
    });
    let created = run_helper(&api, &["POST", "/subscriptions", &subscription.to_string()]).await;
    assert!(created.status.success());
    let id = created.body["subscription"]["id"].as_i64().expect("an id");

    let listing = format!("/subscriptions?user={HELPER_SUBSCRIBER}");
    let listed = run_helper(&api, &["GET", &listing]).await;
    assert_eq!(
        listed.body,
        json!({ "subscriptions": [created.body["subscription"]] })
    );

    let path = format!("/subscriptions/{id}?user={HELPER_SUBSCRIBER}");
    let deleted = run_helper(&api, &["DELETE", &path]).await;
    assert!(deleted.status.success());
    assert_eq!(deleted.body, Value::Null);

    let missing = run_helper(&api, &["DELETE", &path]).await;
    assert!(!missing.status.success());
    assert_eq!(missing.body, json!({ "error": "not_found" }));
}
