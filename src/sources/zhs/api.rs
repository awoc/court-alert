use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::debug;
use uuid::Uuid;

use crate::model::{CourtCatalog, SlotObservation, Venue, VenueId};
use crate::ports::VenueAvailabilitySource;

use super::auth::Auth;
use super::dto::{
    BookingSlotDto, BookingSlotsDataDto, BookingSlotsQueryInput, BookingSlotsVariables,
    GraphQlRequest,
};

const BOOKING_SLOTS_QUERY: &str = "\nquery List_product_slots($productID: UUID!, $input: BookingSlotsInput!) {\n  booking_slots(product_id: $productID, input: $input) {\n    start\n    end\n    booking_period_start\n    booking_period_end\n    availability\n    already_booked\n    already_in_cart\n    already_on_waiting_list\n    blocked_by_resource\n }\n}";

const MAX_CONCURRENT_COURT_FETCHES: usize = 4;

pub struct ZhsSlotAvailabilitySource {
    auth: Auth,
}

impl ZhsSlotAvailabilitySource {
    pub fn new(auth: Auth) -> Self {
        Self { auth }
    }
}

#[async_trait]
impl VenueAvailabilitySource for ZhsSlotAvailabilitySource {
    async fn fetch(
        &self,
        venue: &Venue,
        catalog: &CourtCatalog,
        starts_at: DateTime<Utc>,
        ends_at: DateTime<Utc>,
    ) -> Result<Vec<SlotObservation>> {
        let courts: Vec<(Uuid, String)> = catalog
            .courts()
            .iter()
            .map(|court| (court.id(), court.name().to_owned()))
            .collect();
        let per_court = stream::iter(courts)
            .map(|(court_id, court_name)| async move {
                let slots = fetch_booking_slot_dtos(&self.auth, court_id, starts_at, ends_at)
                    .await
                    .with_context(|| {
                        format!("fetching availability for court {court_name} ({court_id})")
                    })?;
                Ok::<_, anyhow::Error>(
                    slots
                        .into_iter()
                        .map(|slot| normalize_booking_slot(&venue.id, court_id, &court_name, slot))
                        .collect::<Vec<_>>(),
                )
            })
            .buffer_unordered(MAX_CONCURRENT_COURT_FETCHES)
            .try_collect::<Vec<_>>()
            .await?;
        Ok(per_court.into_iter().flatten().collect())
    }
}

fn normalize_booking_slot(
    venue_id: &VenueId,
    court_id: Uuid,
    court_name: &str,
    slot: BookingSlotDto,
) -> SlotObservation {
    SlotObservation {
        venue_id: venue_id.clone(),
        court_id,
        court_name: court_name.to_owned(),
        starts_at: slot.start,
        ends_at: slot.end,
        booking_closes_at: slot.booking_period_end,
        available_places: slot.availability,
        already_booked: slot.already_booked > 0,
        already_in_cart: slot.already_in_cart > 0,
        already_on_waiting_list: slot.already_on_waiting_list > 0,
        blocked_by_resource: slot.blocked_by_resource,
    }
}

async fn fetch_booking_slot_dtos(
    auth: &Auth,
    product_id: Uuid,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
) -> Result<Vec<BookingSlotDto>> {
    let body = GraphQlRequest {
        query: BOOKING_SLOTS_QUERY,
        variables: BookingSlotsVariables {
            product_id,
            input: BookingSlotsQueryInput { starts_at, ends_at },
        },
    };
    let data: BookingSlotsDataDto = query(auth, &body).await?;
    Ok(data.booking_slots)
}

async fn query<V: Serialize + Sync, T: DeserializeOwned>(
    auth: &Auth,
    body: &GraphQlRequest<'_, V>,
) -> Result<T> {
    let url = format!("{}/api/query", auth.base_url());
    auth.request(
        |client| {
            if tracing::enabled!(tracing::Level::DEBUG)
                && let Ok(json) = serde_json::to_string(body)
            {
                debug!(body = %json, "post_query: sending");
            }
            client
                .post(&url)
                .header(reqwest::header::ACCEPT, "application/json")
                .json(body)
        },
        decode_query_response,
    )
    .await
}

async fn decode_query_response<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    let text = response
        .text()
        .await
        .context("reading GraphQL response body")?;
    debug!(%status, bytes = text.len(), "post_query: response received");
    anyhow::ensure!(status.is_success(), "GraphQL HTTP error: {status} {text}");

    let parsed: GraphQlResponse<T> =
        serde_json::from_str(&text).context("decoding GraphQL response")?;
    if let Some(errors) = parsed.errors {
        let joined = errors
            .into_iter()
            .map(|e| e.message)
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!("GraphQL errors: {joined}");
    }
    parsed
        .data
        .ok_or_else(|| anyhow!("GraphQL response had no data"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Credentials;
    use crate::sources::zhs::testing::{install_login_flow_mocks, login_success_response};
    use chrono::TimeZone;
    use serde_json::json;
    use std::collections::HashSet;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn creds() -> Credentials {
        Credentials {
            email: "alice@example.com".into(),
            password: "hunter2".into(),
        }
    }

    const PRODUCT_ID: &str = "92db7384-2dec-4888-a92a-4c2b6faac5f7";

    fn sample_window() -> (DateTime<Utc>, DateTime<Utc>) {
        (
            Utc.with_ymd_and_hms(2026, 6, 1, 22, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 6, 8, 22, 0, 0).unwrap(),
        )
    }

    fn sample_slots_response() -> serde_json::Value {
        json!({
            "data": {
                "booking_slots": [
                    {
                        "start": "2026-06-02T08:00:00Z",
                        "end": "2026-06-02T09:00:00Z",
                        "booking_period_start": "2026-05-26T08:00:00Z",
                        "booking_period_end": "2026-06-02T07:00:00Z",
                        "availability": 1,
                        "already_booked": 0,
                        "already_in_cart": 0,
                        "already_on_waiting_list": 0,
                        "blocked_by_resource": false
                    },
                    {
                        "start": "2026-06-02T09:00:00Z",
                        "end": "2026-06-02T10:00:00Z",
                        "booking_period_start": "2026-05-26T09:00:00Z",
                        "booking_period_end": "2026-06-02T08:00:00Z",
                        "availability": 0,
                        "already_booked": 0,
                        "already_in_cart": 0,
                        "already_on_waiting_list": 0,
                        "blocked_by_resource": true
                    }
                ]
            }
        })
    }

    #[tokio::test]
    async fn happy_path_returns_parsed_slots() {
        let server = MockServer::start().await;
        install_login_flow_mocks(&server).await;
        Mock::given(method("POST"))
            .and(path("/services/identity/self-service/login"))
            .respond_with(login_success_response())
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/query"))
            .and(body_partial_json(json!({
                "variables": {
                    "productID": PRODUCT_ID,
                    "input": {
                        "start": "2026-06-01T22:00:00.000Z",
                        "end": "2026-06-08T22:00:00.000Z"
                    }
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_slots_response()))
            .expect(1)
            .mount(&server)
            .await;

        let auth = Auth::new(server.uri(), creds()).unwrap();
        let (start, end) = sample_window();
        let slots =
            fetch_booking_slot_dtos(&auth, Uuid::parse_str(PRODUCT_ID).unwrap(), start, end)
                .await
                .expect("fetch");

        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].availability, 1);
        assert_eq!(
            slots[0].booking_period_end,
            Some(Utc.with_ymd_and_hms(2026, 6, 2, 7, 0, 0).unwrap())
        );
        assert!(!slots[0].blocked_by_resource);
        assert_eq!(slots[1].availability, 0);
        assert!(slots[1].blocked_by_resource);
    }

    #[test]
    fn api_dto_is_normalized_before_crossing_the_port() {
        let mut response: BookingSlotsDataDto =
            serde_json::from_value(sample_slots_response()["data"].clone()).unwrap();
        let dto = response.booking_slots.remove(0);
        let court_id = Uuid::parse_str(PRODUCT_ID).unwrap();

        let venue_id = VenueId::new("zhs-munich");
        let observation = normalize_booking_slot(&venue_id, court_id, "Court 1", dto);

        assert_eq!(observation.venue_id, venue_id);
        assert_eq!(observation.court_id, court_id);
        assert_eq!(observation.court_name, "Court 1");
        assert_eq!(observation.available_places, 1);
        assert!(!observation.already_booked);
        assert_eq!(
            observation.booking_closes_at,
            Some(Utc.with_ymd_and_hms(2026, 6, 2, 7, 0, 0).unwrap())
        );
    }

    #[tokio::test]
    async fn a_single_unauthorized_is_retried_on_the_same_session() {
        let server = MockServer::start().await;
        install_login_flow_mocks(&server).await;

        Mock::given(method("POST"))
            .and(path("/services/identity/self-service/login"))
            .respond_with(login_success_response())
            .expect(1) // the blip must not cost a re-login
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/query"))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_slots_response()))
            .expect(1)
            .mount(&server)
            .await;

        let auth = Auth::new(server.uri(), creds()).unwrap();
        let (start, end) = sample_window();
        let slots =
            fetch_booking_slot_dtos(&auth, Uuid::parse_str(PRODUCT_ID).unwrap(), start, end)
                .await
                .expect("fetch after retry");
        assert_eq!(slots.len(), 2);
    }

    #[tokio::test]
    async fn auth_expiry_triggers_one_retry() {
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
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(2)
            .expect(2)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_slots_response()))
            .expect(1)
            .mount(&server)
            .await;

        let auth = Auth::new(server.uri(), creds()).unwrap();
        let (start, end) = sample_window();
        let slots =
            fetch_booking_slot_dtos(&auth, Uuid::parse_str(PRODUCT_ID).unwrap(), start, end)
                .await
                .expect("fetch after re-auth");
        assert_eq!(slots.len(), 2);
    }

    #[tokio::test]
    async fn a_venue_fetch_covers_every_court_in_its_catalog() {
        use crate::model::{
            Court, CourtAttributes, CourtCatalog, CourtSurface, Sport, Venue, VenueId,
            VenueIdentity,
        };

        let server = MockServer::start().await;
        install_login_flow_mocks(&server).await;
        Mock::given(method("POST"))
            .and(path("/services/identity/self-service/login"))
            .respond_with(login_success_response())
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_slots_response()))
            .expect(2)
            .mount(&server)
            .await;

        let venue = Venue {
            id: VenueId::new("zhs-munich"),
            display_name: "ZHS München".into(),
            sport: Sport::Tennis,
            identity: VenueIdentity::Zhs {
                base_url: server.uri(),
            },
            poll_interval_secs: None,
            lookahead_days: None,
            operating_window: None,
        };
        let catalog = CourtCatalog::new(vec![
            Court::new(
                Uuid::parse_str(PRODUCT_ID).unwrap(),
                "Court 2".into(),
                CourtAttributes::tennis(CourtSurface::Clay),
            ),
            Court::new(
                Uuid::from_u128(5),
                "Court 5".into(),
                CourtAttributes::tennis(CourtSurface::Clay),
            ),
        ]);
        let source = ZhsSlotAvailabilitySource::new(Auth::new(server.uri(), creds()).unwrap());
        let (start, end) = sample_window();

        let observations = source.fetch(&venue, &catalog, start, end).await.unwrap();

        assert_eq!(observations.len(), 4);
        assert!(observations.iter().all(|o| o.venue_id == venue.id));
        let names: HashSet<&str> = observations.iter().map(|o| o.court_name.as_str()).collect();
        assert_eq!(names, HashSet::from(["Court 2", "Court 5"]));
    }

    #[tokio::test]
    async fn graphql_errors_propagate() {
        let server = MockServer::start().await;
        install_login_flow_mocks(&server).await;
        Mock::given(method("POST"))
            .and(path("/services/identity/self-service/login"))
            .respond_with(login_success_response())
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{"message": "bad query"}]
            })))
            .mount(&server)
            .await;

        let auth = Auth::new(server.uri(), creds()).unwrap();
        let (start, end) = sample_window();
        let err = fetch_booking_slot_dtos(&auth, Uuid::parse_str(PRODUCT_ID).unwrap(), start, end)
            .await
            .expect_err("expected error");
        assert!(err.to_string().contains("bad query"), "got: {err}");
    }

    #[tokio::test]
    async fn a_failed_court_never_returns_partial_venue_availability() {
        use crate::model::{Court, CourtAttributes, CourtSurface, Sport, VenueIdentity};

        let server = MockServer::start().await;
        install_login_flow_mocks(&server).await;
        Mock::given(method("POST"))
            .and(path("/services/identity/self-service/login"))
            .respond_with(login_success_response())
            .expect(1)
            .mount(&server)
            .await;
        let failed_court = Uuid::from_u128(5);
        Mock::given(method("POST"))
            .and(path("/api/query"))
            .and(body_partial_json(
                json!({"variables": {"productID": failed_court}}),
            ))
            .respond_with(ResponseTemplate::new(503))
            .expect(3)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/query"))
            .and(body_partial_json(
                json!({"variables": {"productID": PRODUCT_ID}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_slots_response()))
            .expect(1)
            .mount(&server)
            .await;
        let venue = Venue {
            id: VenueId::new("zhs-munich"),
            display_name: "ZHS München".into(),
            sport: Sport::Tennis,
            identity: VenueIdentity::Zhs {
                base_url: server.uri(),
            },
            poll_interval_secs: None,
            lookahead_days: None,
            operating_window: None,
        };
        let catalog = CourtCatalog::new(vec![
            Court::new(
                Uuid::parse_str(PRODUCT_ID).unwrap(),
                "Court 2".into(),
                CourtAttributes::tennis(CourtSurface::Clay),
            ),
            Court::new(
                failed_court,
                "Court 5".into(),
                CourtAttributes::tennis(CourtSurface::Clay),
            ),
        ]);
        let source = ZhsSlotAvailabilitySource::new(Auth::new(server.uri(), creds()).unwrap());
        let (start, end) = sample_window();

        let error = source
            .fetch(&venue, &catalog, start, end)
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("Court 5"), "{message}");
        assert!(message.contains("503"), "{message}");
    }
    #[derive(Debug, Deserialize)]
    struct QueryData {
        #[serde(rename = "value")]
        _value: u32,
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

                    let auth = Auth::new(server.uri(), creds()).unwrap();
                    let body = GraphQlRequest {
                        query: "query { value }",
                        variables: (),
                    };
                    let error = query::<_, QueryData>(&auth, &body).await.unwrap_err();
                    assert!(format!("{error:#}").contains(expected), "{error:#}");
                }),
        )
        .await;
    }
}
