use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::Mutex;
use tracing::debug;

use crate::config::Credentials;

use super::dto::GraphQlRequest;

const MAX_ATTEMPTS: u32 = 3;

pub struct Auth {
    client: reqwest::Client,
    base_url: String,
    email: String,
    password: String,
    generation: AtomicU64,
    authenticated_generation: AtomicU64,
    login_lock: Mutex<()>,
}

impl Auth {
    pub fn new(base_url: String, creds: Credentials) -> Result<Self> {
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36")
            .build()
            .context("building http client")?;
        Ok(Self {
            client,
            base_url,
            email: creds.email,
            password: creds.password,
            generation: AtomicU64::new(0),
            authenticated_generation: AtomicU64::new(0),
            login_lock: Mutex::new(()),
        })
    }

    fn invalidate_if_generation(&self, generation: u64) -> bool {
        self.authenticated_generation
            .compare_exchange(generation, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    async fn client(&self) -> Result<(&reqwest::Client, u64)> {
        let mut generation = self.authenticated_generation.load(Ordering::SeqCst);
        if generation == 0 {
            let _guard = self.login_lock.lock().await;
            generation = self.authenticated_generation.load(Ordering::SeqCst);
            if generation == 0 {
                self.login().await?;
                generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
                self.authenticated_generation
                    .store(generation, Ordering::SeqCst);
            }
        }
        Ok((&self.client, generation))
    }

    /// Runs a query with the session's retry and refresh policy. A refusal is
    /// retried once before invalidating the session that made the request.
    pub(super) async fn query<V: Serialize + Sync, T: DeserializeOwned>(
        &self,
        body: &GraphQlRequest<'_, V>,
    ) -> Result<T> {
        match self.query_with_retry(body).await {
            Err(FetchError::Unauthorized(_)) => {}
            result => return result.map_err(anyhow::Error::from),
        }
        match self.query_with_retry(body).await {
            Err(FetchError::Unauthorized(generation)) => {
                self.invalidate_if_generation(generation);
                self.query_with_retry(body)
                    .await
                    .map_err(anyhow::Error::from)
                    .context("retry after re-auth failed")
            }
            result => result.map_err(anyhow::Error::from),
        }
    }

    async fn query_with_retry<V: Serialize + Sync, T: DeserializeOwned>(
        &self,
        body: &GraphQlRequest<'_, V>,
    ) -> std::result::Result<T, FetchError> {
        for attempt in 1..=MAX_ATTEMPTS {
            match self.post_query(body).await {
                Ok(data) => return Ok(data),
                Err(FetchError::Unauthorized(generation)) => {
                    return Err(FetchError::Unauthorized(generation));
                }
                Err(error) if attempt == MAX_ATTEMPTS => return Err(error),
                Err(error) => {
                    let delay = retry_delay(attempt);
                    debug!(
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        error = %format!("{error:?}"),
                        "transient /api/query error; retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
        unreachable!("every retry-loop branch returns")
    }

    async fn post_query<V: Serialize + Sync, T: DeserializeOwned>(
        &self,
        body: &GraphQlRequest<'_, V>,
    ) -> std::result::Result<T, FetchError> {
        let url = format!("{}/api/query", self.base_url);
        let (client, auth_generation) = self.client().await.map_err(FetchError::from)?;
        if tracing::enabled!(tracing::Level::DEBUG)
            && let Ok(json) = serde_json::to_string(body)
        {
            debug!(body = %json, "post_query: sending");
        }
        let resp = client
            .post(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(body)
            .send()
            .await
            .context("posting GraphQL query")
            .map_err(FetchError::from)?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(FetchError::Unauthorized(auth_generation));
        }
        let text = resp
            .text()
            .await
            .context("reading GraphQL response body")
            .map_err(FetchError::from)?;
        debug!(%status, bytes = text.len(), "post_query: response received");
        if !status.is_success() {
            return Err(FetchError::Other(anyhow!(
                "GraphQL HTTP error: {status} {text}"
            )));
        }

        let parsed: GraphQlResponse<T> = serde_json::from_str(&text)
            .context("decoding GraphQL response")
            .map_err(FetchError::from)?;

        if let Some(errors) = parsed.errors {
            let joined = errors
                .into_iter()
                .map(|e| e.message)
                .collect::<Vec<_>>()
                .join("; ");
            return Err(FetchError::Other(anyhow!("GraphQL errors: {joined}")));
        }

        let data = parsed
            .data
            .ok_or_else(|| anyhow!("GraphQL response had no data"))
            .map_err(FetchError::from)?;
        Ok(data)
    }

    async fn login(&self) -> Result<()> {
        debug!("login: initializing flow");
        let flow_id = self.init_flow().await?;
        debug!(flow_id = %flow_id, "login: fetching csrf from flow details");
        let csrf = self.fetch_csrf(&flow_id).await?;
        debug!("login: submitting credentials");
        self.submit_credentials(&flow_id, &csrf).await?;
        debug!("login: complete");
        Ok(())
    }

    async fn init_flow(&self) -> Result<String> {
        // Without `refresh`, Kratos answers a browser that still holds a live
        // session with its return URL instead of a login flow, which would
        // leave a re-login no flow id to parse.
        let url = format!(
            "{}/services/identity/self-service/login/browser?refresh=true",
            self.base_url
        );
        let resp = self
            .client
            .get(&url)
            .header(reqwest::header::ACCEPT, "text/html")
            .send()
            .await
            .context("initiating login flow")?;
        debug!(
            status = %resp.status(),
            location = ?resp.headers().get(reqwest::header::LOCATION),
            "init_flow: response received"
        );
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .context("login flow init: no Location header")?
            .to_str()
            .context("login flow init: Location header not ASCII")?
            .to_string();
        extract_flow_id(&location)
            .with_context(|| format!("could not parse flow id from Location: {location}"))
    }

    async fn fetch_csrf(&self, flow_id: &str) -> Result<String> {
        let url = format!(
            "{}/services/identity/self-service/login/flows?id={}",
            self.base_url, flow_id
        );
        let flow: FlowResponse = self
            .client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .context("fetching login flow details")?
            .error_for_status()
            .context("fetching login flow details: status")?
            .json()
            .await
            .context("decoding login flow JSON")?;
        flow.csrf_token()
            .ok_or_else(|| anyhow!("flow response had no csrf_token node"))
    }

    async fn submit_credentials(&self, flow_id: &str, csrf: &str) -> Result<()> {
        let url = format!(
            "{}/services/identity/self-service/login?flow={}",
            self.base_url, flow_id
        );
        let form = [
            ("csrf_token", csrf),
            ("identifier", self.email.as_str()),
            ("password", self.password.as_str()),
            ("method", "password"),
        ];
        let resp = self
            .client
            .post(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form)
            .send()
            .await
            .context("submitting login credentials")?;
        let status = resp.status();
        debug!(%status, "submit_credentials: response received");
        if !status.is_success() && !status.is_redirection() {
            let body = resp.text().await.unwrap_or_default();
            bail!("login failed: {status} {body}");
        }
        Ok(())
    }
}

#[derive(Debug)]
enum FetchError {
    Unauthorized(u64),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for FetchError {
    fn from(e: anyhow::Error) -> Self {
        FetchError::Other(e)
    }
}

impl From<FetchError> for anyhow::Error {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::Unauthorized(_) => anyhow!("request unauthorized (HTTP 401/403)"),
            FetchError::Other(e) => e,
        }
    }
}

fn retry_delay(attempt: u32) -> std::time::Duration {
    match attempt {
        1 => std::time::Duration::from_millis(500),
        2 => std::time::Duration::from_secs(2),
        _ => std::time::Duration::from_secs(5),
    }
}

#[derive(Debug, Deserialize)]
struct GraphQlResponse<T> {
    data: Option<T>,
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphQlError {
    message: String,
}

fn extract_flow_id(url_or_path: &str) -> Option<String> {
    let query = url_or_path.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("flow=") {
            return Some(v.to_string());
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct FlowResponse {
    ui: FlowUi,
}

#[derive(Debug, Deserialize)]
struct FlowUi {
    nodes: Vec<FlowNode>,
}

#[derive(Debug, Deserialize)]
struct FlowNode {
    attributes: FlowNodeAttributes,
}

#[derive(Debug, Deserialize)]
struct FlowNodeAttributes {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    value: Option<serde_json::Value>,
}

impl FlowResponse {
    fn csrf_token(&self) -> Option<String> {
        for node in &self.ui.nodes {
            if node.attributes.name.as_deref() == Some("csrf_token")
                && let Some(v) = node.attributes.value.as_ref().and_then(|v| v.as_str())
            {
                return Some(v.to_string());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_flow_id_from_path() {
        assert_eq!(
            extract_flow_id("/auth/login?flow=abc-123"),
            Some("abc-123".to_string())
        );
    }

    #[test]
    fn extracts_flow_id_with_other_params() {
        assert_eq!(
            extract_flow_id("/auth/login?foo=bar&flow=xyz&baz=qux"),
            Some("xyz".to_string())
        );
    }

    #[test]
    fn no_flow_id_when_missing() {
        assert_eq!(extract_flow_id("/auth/login"), None);
        assert_eq!(extract_flow_id("/auth/login?foo=bar"), None);
    }

    #[test]
    fn csrf_extracted_from_flow_response() {
        let body = serde_json::json!({
            "ui": {
                "nodes": [
                    { "attributes": { "name": "csrf_token", "value": "the-token" } },
                    { "attributes": { "name": "identifier", "value": "" } },
                ]
            }
        });
        let flow: FlowResponse = serde_json::from_value(body).unwrap();
        assert_eq!(flow.csrf_token(), Some("the-token".to_string()));
    }

    use crate::sources::zhs::testing::{
        CSRF, FLOW_ID, install_login_flow_mocks, login_success_response,
    };
    use serde_json::json;
    use wiremock::matchers::{
        body_string_contains, header, method, path, query_param, query_param_is_missing,
    };
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn creds(email: &str, password: &str) -> Credentials {
        Credentials {
            email: email.into(),
            password: password.into(),
        }
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct QueryData {
        value: u32,
    }

    async fn query(auth: &Auth) -> Result<QueryData> {
        auth.query(&GraphQlRequest {
            query: "query { value }",
            variables: (),
        })
        .await
    }

    fn query_success_response() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"data": {"value": 42}}))
    }

    async fn accept_queries(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/api/query"))
            .respond_with(query_success_response())
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn login_flow_completes_and_caches_session() {
        let server = MockServer::start().await;
        install_login_flow_mocks(&server).await;

        Mock::given(method("POST"))
            .and(path("/services/identity/self-service/login"))
            .and(query_param("flow", FLOW_ID))
            .and(body_string_contains(format!("csrf_token={}", CSRF)))
            .and(body_string_contains("identifier=alice%40example.com"))
            .and(body_string_contains("password=hunter2"))
            .and(body_string_contains("method=password"))
            .and(header("accept", "application/json"))
            .respond_with(login_success_response())
            .expect(1)
            .mount(&server)
            .await;

        let auth = Auth::new(server.uri(), creds("alice@example.com", "hunter2")).unwrap();
        accept_queries(&server).await;
        assert_eq!(query(&auth).await.unwrap(), QueryData { value: 42 });
        assert_eq!(query(&auth).await.unwrap(), QueryData { value: 42 });
    }

    #[tokio::test]
    async fn generation_aware_invalidation_preserves_newer_session() {
        let server = MockServer::start().await;
        install_login_flow_mocks(&server).await;

        Mock::given(method("POST"))
            .and(path("/services/identity/self-service/login"))
            .respond_with(login_success_response())
            .expect(2)
            .mount(&server)
            .await;

        let auth = Auth::new(server.uri(), creds("alice@example.com", "hunter2")).unwrap();
        let (_, first_generation) = auth.client().await.expect("first login");
        assert!(auth.invalidate_if_generation(first_generation));
        let (_, second_generation) = auth.client().await.expect("re-login");
        assert!(second_generation > first_generation);

        assert!(!auth.invalidate_if_generation(first_generation));
        let (_, still_second_generation) = auth.client().await.expect("cached newer session");
        assert_eq!(still_second_generation, second_generation);
    }

    #[tokio::test]
    async fn login_refreshes_a_session_kratos_still_considers_live() {
        let server = MockServer::start().await;
        // Kratos hands an already-authenticated browser its return URL, with no
        // flow id in it, unless the request asks to refresh the session.
        Mock::given(method("GET"))
            .and(path("/services/identity/self-service/login/browser"))
            .and(query_param_is_missing("refresh"))
            .respond_with(
                ResponseTemplate::new(303)
                    .insert_header("Location", "https://kurse.zhs-muenchen.de"),
            )
            .mount(&server)
            .await;
        install_login_flow_mocks(&server).await;

        Mock::given(method("POST"))
            .and(path("/services/identity/self-service/login"))
            .respond_with(login_success_response())
            .expect(1)
            .mount(&server)
            .await;

        let auth = Auth::new(server.uri(), creds("alice@example.com", "hunter2")).unwrap();
        accept_queries(&server).await;
        assert_eq!(query(&auth).await.unwrap(), QueryData { value: 42 });
    }

    #[tokio::test]
    async fn login_fails_on_bad_credentials() {
        let server = MockServer::start().await;
        install_login_flow_mocks(&server).await;

        Mock::given(method("POST"))
            .and(path("/services/identity/self-service/login"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "ui": {"messages": [{"text": "invalid credentials"}]}
            })))
            .mount(&server)
            .await;

        let auth = Auth::new(server.uri(), creds("alice@example.com", "wrong")).unwrap();
        let err = query(&auth).await.expect_err("expected failure");
        assert!(err.to_string().to_lowercase().contains("login failed"));
    }

    #[tokio::test]
    async fn a_transient_query_failure_retries_without_refreshing() {
        let server = MockServer::start().await;
        install_login_flow_mocks(&server).await;
        Mock::given(method("POST"))
            .and(path("/services/identity/self-service/login"))
            .respond_with(login_success_response())
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/query"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/query"))
            .respond_with(query_success_response())
            .expect(1)
            .mount(&server)
            .await;

        let auth = Auth::new(server.uri(), creds("alice@example.com", "hunter2")).unwrap();
        assert_eq!(query(&auth).await.unwrap(), QueryData { value: 42 });
    }

    #[tokio::test]
    async fn repeated_refusal_stops_after_one_refresh() {
        for status in [401, 403] {
            let server = MockServer::start().await;
            install_login_flow_mocks(&server).await;
            Mock::given(method("POST"))
                .and(path("/services/identity/self-service/login"))
                .respond_with(login_success_response())
                .expect(2)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/query"))
                .respond_with(ResponseTemplate::new(status))
                .expect(3)
                .mount(&server)
                .await;

            let auth = Auth::new(server.uri(), creds("alice@example.com", "hunter2")).unwrap();
            let error = query(&auth).await.unwrap_err();
            assert!(format!("{error:#}").contains("retry after re-auth failed"));
            assert!(format!("{error:#}").contains("401/403"));
        }
    }

    #[tokio::test]
    async fn invalid_query_responses_exhaust_the_existing_retry_budget() {
        let responses = [
            (ResponseTemplate::new(503), "GraphQL HTTP error"),
            (
                ResponseTemplate::new(200).set_body_string("not json"),
                "decoding GraphQL response",
            ),
            (
                ResponseTemplate::new(200).set_body_json(json!({"data": {"value": "bad"}})),
                "decoding GraphQL response",
            ),
            (
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": {"value": 42}, "errors": [{"message": "partial failure"}]
                })),
                "partial failure",
            ),
            (
                ResponseTemplate::new(200).set_body_json(json!({})),
                "GraphQL response had no data",
            ),
        ];
        // Run the independent failure cases together so their backoff does not
        // make this test take five times as long.
        futures::future::join_all(
            responses
                .into_iter()
                .map(|(response, expected)| async move {
                    let server = MockServer::start().await;
                    install_login_flow_mocks(&server).await;
                    Mock::given(method("POST"))
                        .and(path("/services/identity/self-service/login"))
                        .respond_with(login_success_response())
                        .expect(1)
                        .mount(&server)
                        .await;
                    Mock::given(method("POST"))
                        .and(path("/api/query"))
                        .respond_with(response)
                        .expect(3)
                        .mount(&server)
                        .await;

                    let auth =
                        Auth::new(server.uri(), creds("alice@example.com", "hunter2")).unwrap();
                    let error = query(&auth).await.unwrap_err();
                    assert!(format!("{error:#}").contains(expected), "{error:#}");
                }),
        )
        .await;
    }

    #[tokio::test]
    async fn concurrent_queries_share_login_and_refresh() {
        let server = MockServer::start().await;
        install_login_flow_mocks(&server).await;
        Mock::given(method("POST"))
            .and(path("/services/identity/self-service/login"))
            .respond_with(login_success_response())
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/services/identity/self-service/login"))
            .respond_with(ResponseTemplate::new(200).insert_header(
                "Set-Cookie",
                "ory-session=refreshed-token; Path=/; HttpOnly",
            ))
            .expect(1)
            .mount(&server)
            .await;
        accept_queries(&server).await;

        let auth = Auth::new(server.uri(), creds("alice@example.com", "hunter2")).unwrap();
        let results = futures::future::join_all((0..4).map(|_| query(&auth))).await;
        for result in results {
            assert_eq!(result.unwrap(), QueryData { value: 42 });
        }

        // Expire the session at the remote end. Every request still carrying
        // that cookie must recover through the session module's interface.
        Mock::given(method("POST"))
            .and(path("/api/query"))
            .and(|request: &wiremock::Request| {
                request
                    .headers
                    .get("cookie")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| {
                        value
                            .split(';')
                            .any(|cookie| cookie.trim() == "ory-session=session-token")
                    })
            })
            .respond_with(ResponseTemplate::new(401))
            .with_priority(1)
            .mount(&server)
            .await;

        let results = futures::future::join_all((0..4).map(|_| query(&auth))).await;
        for result in results {
            assert_eq!(result.unwrap(), QueryData { value: 42 });
        }
    }
}
