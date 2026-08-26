use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub(super) const FLOW_ID: &str = "bfa23e58-c5e3-473c-84cd-aa47ad77dd0b";
pub(super) const CSRF: &str = "the-csrf-token";

pub(super) async fn install_login_flow_mocks(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/services/identity/self-service/login/browser"))
        .and(query_param("refresh", "true"))
        .respond_with(
            ResponseTemplate::new(303)
                .insert_header("Location", format!("/auth/login?flow={FLOW_ID}").as_str())
                .insert_header(
                    "Set-Cookie",
                    "csrf_token_abc=cookie-value; Path=/; HttpOnly",
                ),
        )
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/services/identity/self-service/login/flows"))
        .and(query_param("id", FLOW_ID))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ui": {
                "nodes": [
                    {"attributes": {"name": "csrf_token", "value": CSRF}},
                    {"attributes": {"name": "identifier", "value": ""}},
                    {"attributes": {"name": "password", "value": ""}},
                    {"attributes": {"name": "method", "value": "password"}},
                ]
            }
        })))
        .mount(server)
        .await;
}

pub(super) fn login_success_response() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("Set-Cookie", "ory-session=session-token; Path=/; HttpOnly")
}
