use std::collections::HashSet;
use std::sync::Arc;

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

use super::WebhookIntegration;
use crate::config::WebhookSettings;
use crate::integrations::{Integration, readiness};
use crate::model::CourtFilter;
use crate::store::SqliteStore;
use crate::subscriptions::testing::registry;
use crate::subscriptions::{BerlinClock, SubscriptionService};

const API_SECRET: &str = "api-secret";

struct Api {
    base: String,
    http: reqwest::Client,
}

async fn service() -> Arc<SubscriptionService> {
    let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
    Arc::new(SubscriptionService::new(
        store.clone(),
        store,
        HashSet::new(),
        registry(),
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
    let service = service().await;
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
