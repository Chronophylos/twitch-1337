//! Client for Volanta's undocumented web API (`api.volanta.app`).
//!
//! Anonymous reads only. The live feed is the one source of a pilot's
//! active flight; the per-user and profile flight lists omit it. Volanta has
//! pushed back on scraping before, so every request carries a descriptive
//! User-Agent, a 429 is honored, and nothing here is retried.

use std::time::Duration;

use jiff::Timestamp;
use reqwest::{StatusCode, Url};
use serde::Deserialize;

use crate::util::http::{BodyError, SendTraced as _, read_capped};

pub const DEFAULT_BASE_URL: &str = "https://api.volanta.app";

const USER_AGENT: &str = concat!(
    "twitch-1337/",
    env!("CARGO_PKG_VERSION"),
    " (chat bot; +https://github.com/Chronophylos/twitch-1337)"
);

/// The feed is ~1.7 MB decompressed with ~1800 flights; leave headroom.
const FEED_MAX_BYTES: u64 = 16 * 1024 * 1024;
const SMALL_MAX_BYTES: u64 = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
/// Back-off when a 429 carries no usable `Retry-After`.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, thiserror::Error)]
pub enum VolantaError {
    #[error("rate limited, retry after {0:?}")]
    RateLimited(Duration),
    #[error("Volanta answered {0}")]
    Status(StatusCode),
    #[error("Volanta request failed")]
    Http(#[source] reqwest::Error),
    #[error("Volanta response body")]
    Body(#[source] BodyError),
    #[error("Volanta response did not parse")]
    Decode(#[source] serde_json::Error),
}

/// One flight in the live feed. Every field but `id` is optional: the API is
/// undocumented and a missing field must not drop the whole feed.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveFlight {
    pub id: String,
    #[serde(default)]
    pub network_user_id: Option<String>,
    #[serde(default)]
    pub network_user_name: Option<String>,
    #[serde(default)]
    pub callsign: Option<String>,
    /// Airline flight number (`VY1179`); `callsign` is the ATC callsign.
    #[serde(default)]
    pub flight_number: Option<String>,
    #[serde(default)]
    pub origin_icao: Option<String>,
    #[serde(default)]
    pub destination_icao: Option<String>,
    #[serde(default)]
    pub diversion_airport_icao: Option<String>,
    #[serde(default)]
    pub aircraft_icao: Option<String>,
    #[serde(default)]
    pub aircraft_registration: Option<String>,
    #[serde(default)]
    pub position: Option<LivePosition>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LivePosition {
    /// Feet MSL.
    #[serde(default)]
    pub altitude: Option<f64>,
    /// Knots.
    #[serde(default)]
    pub ground_speed: Option<f64>,
    #[serde(default)]
    pub on_ground: bool,
    #[serde(default)]
    pub time: Option<Timestamp>,
}

/// `Flights/{id}`: the flight's final state and landing data.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlightDetail {
    #[serde(default)]
    pub state: FlightState,
    #[serde(default)]
    pub destination_icao: Option<String>,
    #[serde(default)]
    pub diversion_airport_icao: Option<String>,
    /// Feet per minute; Volanta reports touchdown sink as a negative value.
    #[serde(default)]
    pub landing_rate: Option<f64>,
    #[serde(default)]
    pub distance_flown_in_nautical_miles: Option<f64>,
    /// UTC wall time without an offset (`2026-11-24T14:04:26.1002`).
    #[serde(default)]
    pub actual_real_time_off_blocks_time: Option<String>,
    #[serde(default)]
    pub actual_real_time_on_blocks_time: Option<String>,
    /// Recorded events; touchdowns are the ones with a `landingRate`.
    #[serde(default)]
    pub events: Vec<FlightEvent>,
}

/// One entry of a flight's event list. Volanta mixes touchdowns, state
/// changes and sim-rate toggles without a type field, so every field is
/// optional and a touchdown is recognised by its `landingRate`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlightEvent {
    /// UTC wall time without an offset.
    #[serde(default)]
    pub time: Option<String>,
    /// Feet per minute, negative for a descent.
    #[serde(default)]
    pub landing_rate: Option<f64>,
    #[serde(default)]
    pub g_force: Option<f64>,
    #[serde(default)]
    pub pitch: Option<f64>,
    #[serde(default)]
    pub roll: Option<f64>,
    /// Aircraft heading at touchdown, degrees.
    #[serde(default)]
    pub heading: Option<f64>,
    /// Direction the wind blows from, degrees.
    #[serde(default)]
    pub wind_heading: Option<f64>,
    /// Knots.
    #[serde(default)]
    pub wind_speed: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum FlightState {
    Active,
    Completed,
    TimedOut,
    Cancelled,
    Scheduled,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct Profile {
    #[serde(default)]
    id: Option<String>,
}

pub struct VolantaClient {
    http: reqwest::Client,
    base: Url,
}

impl VolantaClient {
    pub fn new(base: &str) -> eyre::Result<Self> {
        use eyre::WrapErr as _;
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(TIMEOUT)
            .build()
            .wrap_err("build Volanta HTTP client")?;
        let base = Url::parse(base).wrap_err("parse Volanta base URL")?;
        Ok(Self { http, base })
    }

    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(["api", "v1"]).extend(segments);
        }
        url
    }

    /// Body of a successful GET, `None` on 404.
    async fn get(&self, url: Url, max_bytes: u64) -> Result<Option<Vec<u8>>, VolantaError> {
        let response = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send_traced()
            .await
            .map_err(VolantaError::Http)?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(VolantaError::RateLimited(retry_after(
                response.headers(),
                Timestamp::now(),
            )));
        }
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(VolantaError::Status(status));
        }
        read_capped(response, max_bytes)
            .await
            .map(Some)
            .map_err(VolantaError::Body)
    }

    /// Every active Volanta flight (`Flights/Volanta/Slim`).
    pub async fn live_feed(&self) -> Result<Vec<LiveFlight>, VolantaError> {
        let body = self
            .get(self.url(&["Flights", "Volanta", "Slim"]), FEED_MAX_BYTES)
            .await?
            .ok_or(VolantaError::Status(StatusCode::NOT_FOUND))?;
        serde_json::from_slice(&body).map_err(VolantaError::Decode)
    }

    /// Volanta user id for `username`, `None` when no such user exists.
    pub async fn user_id(&self, username: &str) -> Result<Option<String>, VolantaError> {
        let Some(body) = self
            .get(self.url(&["Profiles", username]), SMALL_MAX_BYTES)
            .await?
        else {
            return Ok(None);
        };
        let profile: Profile = serde_json::from_slice(&body).map_err(VolantaError::Decode)?;
        Ok(profile.id.filter(|id| !id.is_empty()))
    }

    /// One flight's detail, `None` when Volanta no longer has it.
    pub async fn flight(&self, flight_id: &str) -> Result<Option<FlightDetail>, VolantaError> {
        let Some(body) = self
            .get(self.url(&["Flights", flight_id]), SMALL_MAX_BYTES)
            .await?
        else {
            return Ok(None);
        };
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(VolantaError::Decode)
    }
}

/// `Retry-After` as delta-seconds or an HTTP-date relative to `now`, not
/// shortened: the server's back-off is honored in full. A date already past
/// means retry now; an unparseable or missing header gets the default.
fn retry_after(headers: &reqwest::header::HeaderMap, now: Timestamp) -> Duration {
    let Some(value) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
    else {
        return DEFAULT_RETRY_AFTER;
    };
    if let Ok(secs) = value.parse::<u64>() {
        return Duration::from_secs(secs);
    }
    match jiff::fmt::rfc2822::parse(value) {
        Ok(at) => Duration::try_from(at.timestamp().duration_since(now)).unwrap_or(Duration::ZERO),
        Err(_) => DEFAULT_RETRY_AFTER,
    }
}

/// Block time from Volanta's offset-less UTC wall times.
pub fn block_time(detail: &FlightDetail) -> Option<jiff::SignedDuration> {
    let parse = |s: &Option<String>| {
        s.as_deref()
            .and_then(|s| s.parse::<jiff::civil::DateTime>().ok())
    };
    let off = parse(&detail.actual_real_time_off_blocks_time)?;
    let on = parse(&detail.actual_real_time_on_blocks_time)?;
    let span = on.duration_since(off);
    (span.is_positive()).then_some(span)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(v: &str) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, v.parse().unwrap());
        h
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates_unclamped() {
        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        assert_eq!(
            retry_after(&header("172800"), now),
            Duration::from_secs(172_800)
        );
        assert_eq!(
            retry_after(&header("Tue, 24 Nov 2026 12:30:00 GMT"), now),
            Duration::from_secs(1800)
        );
        assert_eq!(
            retry_after(&header("Tue, 24 Nov 2026 11:00:00 GMT"), now),
            Duration::ZERO
        );
        assert_eq!(retry_after(&header("soon"), now), DEFAULT_RETRY_AFTER);
        assert_eq!(
            retry_after(&reqwest::header::HeaderMap::new(), now),
            DEFAULT_RETRY_AFTER
        );
    }
}
