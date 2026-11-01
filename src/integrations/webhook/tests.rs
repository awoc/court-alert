use std::collections::HashSet;
use std::sync::Arc;

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

use super::WebhookIntegration;
use crate::config::WebhookSettings;
use crate::integrations::{Integration, readiness};
use crate::model::{BookableSlot, CourtFilter, VenueId};
use crate::ports::BookableSlotSnapshotRepository;
use crate::store::SqliteStore;
use crate::subscriptions::testing::registry;
use crate::subscriptions::{BerlinClock, SubscriptionService};
use uuid::Uuid;

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
            with(tennis.clone(), json!({"courts": ["Court 2", "Court 99"]})),
            json!({
                "error": "unknown_courts",
                "unknown": ["Court 99"],
                "available": ["Court 2", "Court 5", "Court 19 - Synthetic"],
            }),
        ),
        (
            with(tennis.clone(), json!({"club": "nowhere"})),
            json!({"error": "unknown_club", "unknown": "nowhere", "available": ["ZHS München"]}),
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
        with(tennis.clone(), json!({"weekday": null})).to_string(),
        with(tennis.clone(), json!({"date": "2030-01-01"})).to_string(),
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
