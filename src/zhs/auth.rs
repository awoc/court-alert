use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::debug;

use crate::config::Credentials;

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

    pub(super) fn invalidate_if_generation(&self, generation: u64) -> bool {
        self.authenticated_generation
            .compare_exchange(generation, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub(super) async fn client(&self) -> Result<(&reqwest::Client, u64)> {
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

    pub(super) fn base_url(&self) -> &str {
        &self.base_url
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
        let url = format!(
            "{}/services/identity/self-service/login/browser",
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

    use crate::zhs::testing::{CSRF, FLOW_ID, install_login_flow_mocks, login_success_response};
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn creds(email: &str, password: &str) -> Credentials {
        Credentials {
            email: email.into(),
            password: password.into(),
        }
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
        auth.client().await.expect("login");
        auth.client().await.expect("cached client"); // no extra POST — expect(1) enforces this on drop
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
        let err = auth.client().await.expect_err("expected failure");
        assert!(err.to_string().to_lowercase().contains("login failed"));
    }
}
