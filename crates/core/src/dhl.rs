//! DHL Shipment Tracking - Unified client backing Aurora's `track_shipment`
//! tool. Coarse tier only: no `recipientPostalCode` is ever sent, so DHL
//! withholds address, sender/recipient and transit cities. See
//! `docs/superpowers/specs/2026-08-18-dhl-shipment-tracking-design.md`.

use std::time::Duration;

use eyre::{Result, WrapErr};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::APP_USER_AGENT;

pub const DEFAULT_BASE_URL: &str = "https://api-eu.dhl.com";
const TIMEOUT: Duration = Duration::from_secs(10);

/// Events forwarded to the model. A const, not architecture — move it if
/// transcripts show people asking about older stages.
pub const MAX_EVENTS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DhlError {
    /// Not yet scanned, a DHL division the Unified API does not source, or
    /// aged out of the backend. Never "this parcel does not exist".
    NotFound,
    RateLimitedSpike,
    RateLimitedDaily,
    RateLimited,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShipmentEvent {
    pub timestamp: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackedShipment {
    pub status_code: Option<String>,
    pub status_description: Option<String>,
    pub status_timestamp: Option<String>,
    pub estimated_delivery: Option<String>,
    pub destination: Option<String>,
    pub events: Vec<ShipmentEvent>,
}

// --- DHL wire types: only the fields the projection forwards. ---

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    shipments: Vec<WireShipment>,
}

#[derive(Debug, Deserialize)]
struct WireShipment {
    #[serde(default)]
    status: Option<WireStatus>,
    #[serde(default, rename = "estimatedTimeOfDelivery")]
    estimated_time_of_delivery: Option<String>,
    #[serde(default)]
    destination: Option<WirePlace>,
    #[serde(default)]
    events: Vec<WireEvent>,
}

#[derive(Debug, Deserialize)]
struct WireStatus {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default, rename = "statusCode")]
    status_code: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WirePlace {
    #[serde(default)]
    address: Option<WireAddress>,
}

#[derive(Debug, Deserialize)]
struct WireAddress {
    #[serde(default, rename = "addressLocality")]
    address_locality: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireEvent {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

pub struct DhlClient {
    http: reqwest::Client,
    base_url: String,
    api_key: SecretString,
}

impl DhlClient {
    pub fn new(api_key: SecretString) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .timeout(TIMEOUT)
            .build()
            .wrap_err("build dhl HTTP client")?;
        Ok(Self {
            http,
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key,
        })
    }

    /// Test hook: custom base URL (e.g. wiremock). Caller supplies the HTTP
    /// client (production [`Self::new`] sets [`APP_USER_AGENT`] and a 10s
    /// timeout).
    pub fn with_base_url(
        http: reqwest::Client,
        base_url: impl Into<String>,
        api_key: SecretString,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            api_key,
        }
    }

    // ponytail: no rate guard, DHL's 429 is the backstop — add a daily
    // counter if the 250/day cap ever actually bites
    pub async fn track(&self, tracking_number: &str) -> Result<TrackedShipment, DhlError> {
        let url = format!("{}/track/shipments", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(url)
            .header("DHL-API-Key", self.api_key.expose_secret())
            .query(&[("trackingNumber", tracking_number), ("language", "de")])
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(?error, "dhl request failed");
                DhlError::Unavailable
            })?;

        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(DhlError::NotFound);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Apigee emits distinct errorcodes for the two 429s. Substring
            // match on the raw body rather than parsing a shape we have not
            // verified DHL preserves; the generic arm covers a rewrap.
            let body = resp.text().await.unwrap_or_default();
            return Err(if body.contains("SpikeArrestViolation") {
                DhlError::RateLimitedSpike
            } else if body.contains("QuotaViolation") {
                DhlError::RateLimitedDaily
            } else {
                DhlError::RateLimited
            });
        }
        if !status.is_success() {
            tracing::warn!(%status, "dhl returned an unexpected status");
            return Err(DhlError::Unavailable);
        }

        let parsed: WireResponse = resp.json().await.map_err(|error| {
            tracing::warn!(?error, "dhl response did not parse");
            DhlError::Unavailable
        })?;

        let shipment = parsed
            .shipments
            .into_iter()
            .next()
            .ok_or(DhlError::NotFound)?;
        Ok(project(shipment))
    }
}

fn project(s: WireShipment) -> TrackedShipment {
    let (status_code, status_description, status_timestamp) = match s.status {
        Some(st) => (st.status_code, st.description, st.timestamp),
        None => (None, None, None),
    };

    let mut events: Vec<ShipmentEvent> = s
        .events
        .into_iter()
        .filter_map(|e| {
            Some(ShipmentEvent {
                timestamp: e.timestamp?,
                description: e.description?,
            })
        })
        .collect();
    // DHL's ordering is not contractual. Timestamps are ISO 8601, so a
    // lexicographic reverse sort is a correct newest-first ordering.
    events.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    events.truncate(MAX_EVENTS);

    TrackedShipment {
        status_code,
        status_description,
        status_timestamp,
        estimated_delivery: s.estimated_time_of_delivery,
        destination: s
            .destination
            .and_then(|d| d.address)
            .and_then(|a| a.address_locality),
        events,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use secrecy::SecretString;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_client(server: &MockServer) -> DhlClient {
        crate::install_crypto_provider();
        DhlClient::with_base_url(
            reqwest::Client::new(),
            server.uri(),
            SecretString::new("test-key".into()),
        )
    }

    /// Newest event is 2026-08-18T20, oldest is 2026-08-18T00 (25 total,
    /// deliberately supplied oldest-first so the sort is actually exercised).
    fn shipment_with_25_events() -> serde_json::Value {
        let events: Vec<_> = (0..25)
            .map(|i| {
                serde_json::json!({
                    "timestamp": format!("2026-08-18T{i:02}:00:00"),
                    "description": format!("event {i}"),
                })
            })
            .collect();
        serde_json::json!({
            "shipments": [{
                "id": "00340434161094042557",
                "status": {
                    "timestamp": "2026-08-18T24:00:00",
                    "statusCode": "transit",
                    "description": "Die Sendung wurde im Start-Paketzentrum bearbeitet."
                },
                "estimatedTimeOfDelivery": "2026-08-19T12:00:00",
                "destination": { "address": { "addressLocality": "Berlin" } },
                "events": events
            }]
        })
    }

    #[tokio::test]
    async fn trims_to_ten_events_newest_first() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/track/shipments"))
            .and(query_param("trackingNumber", "00340434161094042557"))
            .and(query_param("language", "de"))
            .and(header("DHL-API-Key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(shipment_with_25_events()))
            .expect(1)
            .mount(&server)
            .await;

        let got = test_client(&server)
            .track("00340434161094042557")
            .await
            .expect("track ok");

        assert_eq!(got.events.len(), MAX_EVENTS);
        assert_eq!(got.events[0].description, "event 24");
        assert_eq!(got.events[9].description, "event 15");
        assert_eq!(got.status_code.as_deref(), Some("transit"));
        assert_eq!(got.destination.as_deref(), Some("Berlin"));
        assert_eq!(
            got.estimated_delivery.as_deref(),
            Some("2026-08-19T12:00:00")
        );
    }

    #[tokio::test]
    async fn not_found_maps_to_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "status": 404, "title": "No shipments found"
            })))
            .mount(&server)
            .await;

        assert_eq!(
            test_client(&server).track("12345678").await,
            Err(DhlError::NotFound)
        );
    }

    #[tokio::test]
    async fn spike_arrest_429_maps_to_spike() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "fault": {
                    "faultstring": "Spike arrest violation. Allowed rate : 1ps",
                    "detail": { "errorcode": "policies.ratelimit.SpikeArrestViolation" }
                }
            })))
            .mount(&server)
            .await;

        assert_eq!(
            test_client(&server).track("12345678").await,
            Err(DhlError::RateLimitedSpike)
        );
    }

    #[tokio::test]
    async fn quota_429_maps_to_daily() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "fault": {
                    "faultstring": "Rate limit quota violation. Quota limit exceeded.",
                    "detail": { "errorcode": "policies.ratelimit.QuotaViolation" }
                }
            })))
            .mount(&server)
            .await;

        assert_eq!(
            test_client(&server).track("12345678").await,
            Err(DhlError::RateLimitedDaily)
        );
    }

    /// Guards the unverified inference that DHL passes the Apigee fault
    /// through rather than rewrapping it. If DHL rewraps, this test stays
    /// green while the two above go red — that is the signal.
    #[tokio::test]
    async fn unrecognised_429_maps_to_generic_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "status": 429, "title": "Too Many Requests"
            })))
            .mount(&server)
            .await;

        assert_eq!(
            test_client(&server).track("12345678").await,
            Err(DhlError::RateLimited)
        );
    }

    #[tokio::test]
    async fn server_error_maps_to_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        assert_eq!(
            test_client(&server).track("12345678").await,
            Err(DhlError::Unavailable)
        );
    }

    #[tokio::test]
    async fn empty_shipments_array_maps_to_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "shipments": [] })),
            )
            .mount(&server)
            .await;

        assert_eq!(
            test_client(&server).track("12345678").await,
            Err(DhlError::NotFound)
        );
    }
}
