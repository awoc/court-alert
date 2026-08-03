//! Playtomic padel clubs.
//!
//! Two surfaces, neither of them an offered public API (`robots.txt` disallows
//! `/api`): the availability JSON route the club page's own frontend calls, and
//! the page itself, which is the only place a `resource_id` is given a name.
//! Both are pinned behind tests so a payload change fails loudly.

mod availability;
mod discovery;
mod rsc;

pub use availability::PlaytomicAvailabilitySource;
pub use discovery::PlaytomicCatalogSource;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate};
use serde::de::DeserializeOwned;
use tracing::debug;
use uuid::Uuid;

use crate::model::{Sport, Venue, VenueId, VenueIdentity};

const BASE_URL: &str = "https://playtomic.com";

/// The site serves the flight payload only to something that looks like a
/// browser; this is the string the investigation used throughout.
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Between the sequential per-date requests within one club.
///
/// This is not a public API and publishes no quota, so the poll stays
/// deliberately unhurried. At the low end of the useful range: 15 dates cost
/// ~7.5 s of wall clock per club per tick, which is nothing against a 300 s
/// interval but is why it should not be raised casually.
const INTER_DATE_DELAY: Duration = Duration::from_millis(500);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One shared HTTP client across every Playtomic venue.
///
/// A client per venue would give each its own connection pool and defeat the
/// cross-club reuse the scheduling design depends on.
#[derive(Clone)]
pub struct PlaytomicClient {
    http: Arc<reqwest::Client>,
    base_url: String,
    inter_date_delay: Duration,
}

impl PlaytomicClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: Arc::new(
                reqwest::Client::builder()
                    .timeout(REQUEST_TIMEOUT)
                    .user_agent(USER_AGENT)
                    .build()
                    .context("building the Playtomic HTTP client")?,
            ),
            base_url: BASE_URL.to_string(),
            inter_date_delay: INTER_DATE_DELAY,
        })
    }

    #[cfg(test)]
    fn for_test(base_url: String) -> Self {
        Self {
            http: Arc::new(reqwest::Client::new()),
            base_url,
            inter_date_delay: Duration::ZERO,
        }
    }

    async fn pause_between_dates(&self) {
        tokio::time::sleep(self.inter_date_delay).await;
    }

    /// The club page as its raw RSC flight payload.
    ///
    /// `RSC: 1` matters: the plain HTML is 2.6× larger and carries the same
    /// JSON backslash-escaped and split across `self.__next_f` chunks.
    async fn club_page(&self, slug: &str) -> Result<String> {
        let url = format!("{}/clubs/{slug}", self.base_url);
        let response = self
            .http
            .get(&url)
            .header("RSC", "1")
            .send()
            .await
            .with_context(|| format!("requesting the club page {url}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("reading the club page body")?;
        anyhow::ensure!(status.is_success(), "club page {url} returned {status}");
        debug!(slug, bytes = body.len(), "fetched club page");
        Ok(body)
    }

    async fn availability<T: DeserializeOwned>(
        &self,
        tenant_id: Uuid,
        date: NaiveDate,
        sport_id: &str,
    ) -> Result<Vec<T>> {
        let mut url = reqwest::Url::parse(&format!("{}/api/clubs/availability", self.base_url))
            .context("building the availability URL")?;
        url.query_pairs_mut()
            .append_pair("tenant_id", &tenant_id.to_string())
            .append_pair("date", &date.to_string())
            // Always sent explicitly: it defaults to PADEL, so a Playtomic
            // tennis club would otherwise silently return padel courts.
            .append_pair("sport_id", sport_id);
        let response = self
            .http
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("requesting availability for {date}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("reading the availability body")?;
        anyhow::ensure!(
            status.is_success(),
            "availability for {date} returned {status}: {body}"
        );
        serde_json::from_str(&body)
            .with_context(|| format!("decoding the availability response for {date}"))
    }
}

/// What the club page says about a venue beyond its courts.
#[derive(Debug, Clone)]
struct ClubMeta {
    timezone: String,
    /// Opening time per upper-case weekday name, as the payload spells it.
    opening_hours: HashMap<String, String>,
}

impl ClubMeta {
    fn opening_time_on(&self, date: NaiveDate) -> Option<&str> {
        self.opening_hours
            .get(weekday_key(date))
            .map(String::as_str)
    }
}

fn weekday_key(date: NaiveDate) -> &'static str {
    match date.weekday() {
        chrono::Weekday::Mon => "MONDAY",
        chrono::Weekday::Tue => "TUESDAY",
        chrono::Weekday::Wed => "WEDNESDAY",
        chrono::Weekday::Thu => "THURSDAY",
        chrono::Weekday::Fri => "FRIDAY",
        chrono::Weekday::Sat => "SATURDAY",
        chrono::Weekday::Sun => "SUNDAY",
    }
}

/// Carries the club-page facts the availability adapter needs but cannot see.
///
/// Discovery writes it; the availability fetch reads it to run the
/// opening-hours canary. A side channel rather than a wider
/// `CourtCatalogSource` return type, so the port stays about catalogs.
#[derive(Clone, Default)]
pub struct ClubDirectory(Arc<RwLock<HashMap<VenueId, ClubMeta>>>);

impl ClubDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self, venue_id: VenueId, meta: ClubMeta) {
        self.0
            .write()
            .expect("club directory poisoned")
            .insert(venue_id, meta);
    }

    fn get(&self, venue_id: &VenueId) -> Option<ClubMeta> {
        self.0
            .read()
            .expect("club directory poisoned")
            .get(venue_id)
            .cloned()
    }
}

fn playtomic_sport_id(sport: Sport) -> &'static str {
    match sport {
        Sport::Tennis => "TENNIS",
        Sport::Padel => "PADEL",
    }
}

fn playtomic_identity(venue: &Venue) -> Result<(Uuid, &str)> {
    match &venue.identity {
        VenueIdentity::Playtomic { tenant_id, slug } => Ok((*tenant_id, slug.as_str())),
        other => anyhow::bail!(
            "venue {} is a {} venue, not a Playtomic one",
            venue.id,
            other.provider()
        ),
    }
}

/// Playtomic serves today through today+14 inclusive: 15 distinct dates.
///
/// Stated as a number because "the 14-day horizon" invites an off-by-one, and
/// because `berlin_day_window_at` builds a half-open range — so `15` covers
/// exactly the horizon and `16` adds a guaranteed-empty request.
pub const MAX_LOOKAHEAD_DAYS: i64 = 15;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CourtCatalog, Sport, VenueId};
    use crate::ports::{CourtCatalogSource, VenueAvailabilitySource};
    use chrono::{TimeZone, Utc};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TENANT: &str = "f8483f72-1d14-49eb-a98b-e4b89d969c78";
    const CLUB_PAGE: &str = include_str!("testdata/club.rsc");
    const AVAILABILITY: &str = include_str!("testdata/availability.json");

    fn venue(sport: Sport) -> Venue {
        Venue {
            id: VenueId::new("casa-padel"),
            display_name: "Casa Padel Pineapple Park".into(),
            sport,
            identity: VenueIdentity::Playtomic {
                tenant_id: Uuid::parse_str(TENANT).unwrap(),
                slug: "casa-padel-pineapple-park".into(),
            },
            poll_interval_secs: None,
            lookahead_days: None,
            operating_window: None,
        }
    }

    async fn club_page_server(body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/clubs/casa-padel-pineapple-park"))
            .and(header("RSC", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn discovery_builds_a_catalog_from_the_club_page() {
        let server = club_page_server(CLUB_PAGE).await;
        let source = PlaytomicCatalogSource::new(
            PlaytomicClient::for_test(server.uri()),
            ClubDirectory::new(),
        );

        let catalog = source.discover(&venue(Sport::Padel)).await.unwrap();

        assert_eq!(catalog.courts().len(), 9);
        assert_eq!(catalog.names()[0], "Court 1 (Indoor)");
        assert_eq!(
            catalog.attributes_of(Uuid::parse_str("b2d8552d-5794-4abc-879f-61d53f587978").unwrap()),
            Some(&crate::model::CourtAttributes::padel(Some(
                crate::model::CourtLocation::Indoor
            )))
        );
        assert_eq!(
            catalog.attributes_of(Uuid::parse_str("4f5332e6-d392-4d53-96af-6287e2d411b9").unwrap()),
            Some(&crate::model::CourtAttributes::padel(Some(
                crate::model::CourtLocation::Outdoor
            )))
        );
    }

    /// Multi-sport clubs are real, and an unfiltered catalog would put beach
    /// volleyball courts in a padel venue.
    #[tokio::test]
    async fn discovery_keeps_only_the_venues_own_sport() {
        let mixed = CLUB_PAGE.replace(
            r#"{"resourceId":"4f5332e6-d392-4d53-96af-6287e2d411b9","name":"Court 6 (Outdoor)","sport":"PADEL","features":["outdoor","double","crystal"]}"#,
            r#"{"resourceId":"4f5332e6-d392-4d53-96af-6287e2d411b9","name":"Beach 1","sport":"BEACH_VOLLEY","features":["indoor","sand"]}"#,
        );
        assert_ne!(mixed, CLUB_PAGE, "the fixture court to swap was not found");
        let server = club_page_server(&mixed).await;
        let source = PlaytomicCatalogSource::new(
            PlaytomicClient::for_test(server.uri()),
            ClubDirectory::new(),
        );

        let catalog = source.discover(&venue(Sport::Padel)).await.unwrap();

        assert_eq!(catalog.courts().len(), 8);
        assert!(!catalog.names().contains(&"Beach 1".to_string()));
    }

    /// A slug can be re-pointed at a different club, and monitoring the wrong
    /// venue under the right name would be invisible.
    #[tokio::test]
    async fn discovery_rejects_a_club_page_for_a_different_tenant() {
        let elsewhere = CLUB_PAGE.replace(TENANT, "11111111-2222-3333-4444-555555555555");
        let server = club_page_server(&elsewhere).await;
        let source = PlaytomicCatalogSource::new(
            PlaytomicClient::for_test(server.uri()),
            ClubDirectory::new(),
        );

        let error = source.discover(&venue(Sport::Padel)).await.unwrap_err();

        assert!(
            format!("{error:#}").contains("tenant"),
            "unhelpful error: {error:#}"
        );
    }

    #[tokio::test]
    async fn discovery_fails_when_the_club_serves_none_of_the_venues_sport() {
        let server = club_page_server(CLUB_PAGE).await;
        let source = PlaytomicCatalogSource::new(
            PlaytomicClient::for_test(server.uri()),
            ClubDirectory::new(),
        );

        // The fixture club is padel-only.
        assert!(source.discover(&venue(Sport::Tennis)).await.is_err());
    }

    #[tokio::test]
    async fn a_failing_club_page_is_an_error_not_an_empty_catalog() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let source = PlaytomicCatalogSource::new(
            PlaytomicClient::for_test(server.uri()),
            ClubDirectory::new(),
        );

        assert!(source.discover(&venue(Sport::Padel)).await.is_err());
    }

    #[tokio::test]
    async fn availability_asks_for_one_date_per_day_of_the_window() {
        let server = MockServer::start().await;
        for date in ["2026-08-05", "2026-08-06"] {
            Mock::given(method("GET"))
                .and(path("/api/clubs/availability"))
                .and(query_param("tenant_id", TENANT))
                .and(query_param("date", date))
                // Sent explicitly: the endpoint defaults to PADEL, so a tennis
                // club would silently come back with padel courts.
                .and(query_param("sport_id", "PADEL"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(if date == "2026-08-05" {
                        AVAILABILITY
                    } else {
                        "[]"
                    }),
                )
                .expect(1)
                .mount(&server)
                .await;
        }
        let source = PlaytomicAvailabilitySource::new(
            PlaytomicClient::for_test(server.uri()),
            ClubDirectory::new(),
        );

        // Berlin-midnight bounds spanning exactly 5 and 6 August.
        let observations = source
            .fetch(
                &venue(Sport::Padel),
                &CourtCatalog::default(),
                Utc.with_ymd_and_hms(2026, 8, 4, 22, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 8, 6, 22, 0, 0).unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(observations.len(), 5);
        assert!(
            observations
                .iter()
                .all(|o| o.venue_id.as_str() == "casa-padel")
        );
    }

    /// One club's outage must surface as an error rather than an empty poll,
    /// which the monitor would read as "everything got booked".
    #[tokio::test]
    async fn a_failing_date_fails_the_whole_venue_fetch() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/clubs/availability"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let source = PlaytomicAvailabilitySource::new(
            PlaytomicClient::for_test(server.uri()),
            ClubDirectory::new(),
        );

        let error = source
            .fetch(
                &venue(Sport::Padel),
                &CourtCatalog::default(),
                Utc.with_ymd_and_hms(2026, 8, 4, 22, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 8, 5, 22, 0, 0).unwrap(),
            )
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("429"), "got: {error:#}");
    }

    #[test]
    fn the_sport_id_is_playtomics_own_vocabulary() {
        assert_eq!(playtomic_sport_id(Sport::Padel), "PADEL");
        assert_eq!(playtomic_sport_id(Sport::Tennis), "TENNIS");
    }

    #[test]
    fn a_non_playtomic_venue_is_rejected_by_the_adapter() {
        let zhs = Venue {
            identity: VenueIdentity::Zhs {
                base_url: "https://example.test".into(),
            },
            ..venue(Sport::Tennis)
        };
        assert!(playtomic_identity(&zhs).is_err());
    }
}
