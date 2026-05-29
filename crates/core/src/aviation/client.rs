//! HTTP client for ADS-B aggregators (adsb.lol + fallbacks), adsbdb,
//! Nominatim, and Aviationstack.

use std::collections::HashMap;
use std::time::Duration;

use eyre::{Result, WrapErr as _};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::util::APP_USER_AGENT;

use super::location::{
    ResolvedLocation, airline_table, is_iata_flight_number, is_icao_flight_number,
};
use super::tracker::FlightIdentifier;
use super::types::{
    AdsbAircraftResponse, AdsbDbAirlineResponse, AdsbDbResponse, AviationstackFlightMetadata,
    AviationstackFlightsResponse, FlightRoute, NearbyAircraft,
};

/// Aviation-crate-internal runtime config for the Aviationstack HTTP enrichment
/// endpoint. `api_key` comes from config.toml bootstrap; `base_url` and
/// `timeout_secs` come from the dashboard settings store.
#[derive(Clone)]
pub(crate) struct AviationstackConfig {
    pub(crate) api_key: SecretString,
    pub(crate) base_url: String,
    pub(crate) timeout_secs: u64,
}

const ADSBDB_BASE_URL: &str = "https://api.adsbdb.com/v0";
const ADSBLOL_BASE_URL: &str = "https://api.adsb.lol/v2";
const AIRPLANES_LIVE_BASE_URL: &str = "https://api.airplanes.live/v2";
const ADSBFI_BASE_URL: &str = "https://opendata.adsb.fi/api/v2";
const ADSBONE_BASE_URL: &str = "https://api.adsb.one/v2";
const NOMINATIM_BASE_URL: &str = "https://nominatim.openstreetmap.org";
const AIRLINE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const ADSB_AGGREGATOR_TIMEOUT: Duration = Duration::from_secs(2);

/// Freshness equivalence window, in seconds. Position fixes whose `seen_pos`
/// falls in the same bucket are treated as equally fresh, so `rssi` decides.
const FRESH_WINDOW: f64 = 5.0;

/// Quantize `seen_pos` into a freshness bucket (lower = fresher). Missing,
/// negative, or non-finite values sort last — a bare `as u64` cast would
/// saturate negative/NaN to 0 (the freshest bucket) and let garbage win.
fn freshness_bucket(seen_pos: Option<f64>) -> u64 {
    match seen_pos {
        Some(s) if s.is_finite() && s >= 0.0 => (s / FRESH_WINDOW).floor() as u64,
        _ => u64::MAX,
    }
}

/// Pick the most-reliable copy of one aircraft: freshest `seen_pos` bucket,
/// then highest `rssi`. On a full tie, keeps the first element (callers push
/// in aggregator priority order, so the highest-priority backend wins).
fn pick_winner(group: Vec<NearbyAircraft>) -> NearbyAircraft {
    group
        .into_iter()
        .min_by(|a, b| {
            freshness_bucket(a.seen_pos)
                .cmp(&freshness_bucket(b.seen_pos))
                .then_with(|| {
                    b.rssi
                        .unwrap_or(f64::MIN)
                        .total_cmp(&a.rssi.unwrap_or(f64::MIN))
                })
        })
        .expect("group is never empty")
}

/// Union the per-backend aircraft lists into a coverage superset, deduping by
/// hex (case-insensitive key; the stored hex is left untouched) and keeping the
/// best copy of each. Hex-less aircraft are passed through unmerged. Output
/// order follows first-seen order across backends.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "wired into production in the parallel fan-out task (Task 5)"
    )
)]
fn merge_aircraft(per_backend: Vec<Vec<NearbyAircraft>>) -> Vec<NearbyAircraft> {
    let mut groups: HashMap<String, Vec<NearbyAircraft>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut hexless: Vec<NearbyAircraft> = Vec::new();

    for backend in per_backend {
        for aircraft in backend {
            match aircraft.hex.as_deref() {
                Some(hex) if !hex.is_empty() => {
                    let key = hex.to_ascii_lowercase();
                    if !groups.contains_key(&key) {
                        order.push(key.clone());
                    }
                    groups.entry(key).or_default().push(aircraft);
                }
                _ => hexless.push(aircraft),
            }
        }
    }

    let mut merged: Vec<NearbyAircraft> = order
        .into_iter()
        .map(|key| pick_winner(groups.remove(&key).expect("key came from groups")))
        .collect();
    merged.extend(hexless);
    merged
}

#[derive(Clone)]
pub(super) struct AdsbAggregator {
    name: &'static str,
    base_url: String,
    path_style: AdsbPathStyle,
}

impl AdsbAggregator {
    pub(super) fn readsb_v2(name: &'static str, base_url: String) -> Self {
        Self {
            name,
            base_url,
            path_style: AdsbPathStyle::ReadsbV2,
        }
    }

    pub(super) fn adsb_fi(base_url: String) -> Self {
        Self {
            name: "adsb.fi",
            base_url,
            path_style: AdsbPathStyle::AdsbFiOpenData,
        }
    }
}

#[derive(Clone, Copy)]
enum AdsbPathStyle {
    ReadsbV2,
    AdsbFiOpenData,
}

#[derive(Clone, Copy)]
pub(super) enum AdsbEndpoint<'a> {
    Point { lat: f64, lon: f64, radius_nm: u16 },
    Hex(&'a str),
    Callsign(&'a str),
}

impl AdsbEndpoint<'_> {
    fn label(&self) -> &'static str {
        match self {
            Self::Point { .. } => "point",
            Self::Hex(_) => "hex",
            Self::Callsign(_) => "callsign",
        }
    }

    pub(super) fn url(&self, aggregator: &AdsbAggregator) -> String {
        let base = aggregator.base_url.trim_end_matches('/');
        match (aggregator.path_style, self) {
            (
                AdsbPathStyle::ReadsbV2,
                Self::Point {
                    lat,
                    lon,
                    radius_nm,
                },
            ) => format!("{base}/point/{lat}/{lon}/{radius_nm}"),
            (AdsbPathStyle::ReadsbV2, Self::Hex(hex)) => format!("{base}/hex/{hex}"),
            (AdsbPathStyle::ReadsbV2, Self::Callsign(callsign)) => {
                format!("{base}/callsign/{callsign}")
            }
            (
                AdsbPathStyle::AdsbFiOpenData,
                Self::Point {
                    lat,
                    lon,
                    radius_nm,
                },
            ) => format!("{base}/lat/{lat}/lon/{lon}/dist/{radius_nm}"),
            (AdsbPathStyle::AdsbFiOpenData, Self::Hex(hex)) => {
                format!("{base}/hex/{hex}")
            }
            (AdsbPathStyle::AdsbFiOpenData, Self::Callsign(callsign)) => {
                format!("{base}/callsign/{callsign}")
            }
        }
    }
}

enum AdsbFetchError {
    /// HTTP 429 — back off this backend (cooldown park).
    RateLimited,
    /// Request timed out. Retryable, but NOT counted toward parking.
    Timeout(eyre::Report),
    /// Other retryable failure (5xx, connect-refused, parse error).
    Retryable(eyre::Report),
    /// Non-retryable send error.
    Fatal(eyre::Report),
}

#[derive(Debug, Deserialize)]
struct NominatimResult {
    lat: String,
    lon: String,
    display_name: String,
}

#[derive(Clone)]
pub struct AviationClient {
    http: reqwest::Client,
    adsb_aggregators: Vec<AdsbAggregator>,
    adsb_aggregator_timeout: Duration,
    adsbdb_base_url: String,
    nominatim_base_url: String,
    aviationstack: Option<AviationstackConfig>,
}

impl AviationClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .wrap_err("Failed to build aviation HTTP client")?;
        Ok(Self::new_with_adsb_aggregators(
            Self::default_adsb_aggregators(),
            ADSBDB_BASE_URL.to_owned(),
            NOMINATIM_BASE_URL.to_owned(),
            http,
        ))
    }

    pub fn new_with_base_url(
        adsb_base_url: String,
        adsbdb_base_url: String,
        nominatim_base_url: String,
        http_client: reqwest::Client,
    ) -> Self {
        Self::new_with_adsb_aggregators(
            vec![AdsbAggregator::readsb_v2("adsb.lol", adsb_base_url)],
            adsbdb_base_url,
            nominatim_base_url,
            http_client,
        )
    }

    pub(super) fn new_with_adsb_aggregators(
        adsb_aggregators: Vec<AdsbAggregator>,
        adsbdb_base_url: String,
        nominatim_base_url: String,
        http_client: reqwest::Client,
    ) -> Self {
        Self {
            http: http_client,
            adsb_aggregators,
            adsb_aggregator_timeout: ADSB_AGGREGATOR_TIMEOUT,
            adsbdb_base_url,
            nominatim_base_url,
            aviationstack: None,
        }
    }

    fn default_adsb_aggregators() -> Vec<AdsbAggregator> {
        vec![
            AdsbAggregator::readsb_v2("adsb.lol", ADSBLOL_BASE_URL.to_owned()),
            AdsbAggregator::readsb_v2("airplanes.live", AIRPLANES_LIVE_BASE_URL.to_owned()),
            AdsbAggregator::adsb_fi(ADSBFI_BASE_URL.to_owned()),
            AdsbAggregator::readsb_v2("ADSB.One", ADSBONE_BASE_URL.to_owned()),
        ]
    }

    /// Enable Aviationstack flight-metadata enrichment.
    ///
    /// `api_key` comes from the bootstrap secret in config.toml.
    /// `base_url` and `timeout_secs` come from the dashboard settings store.
    /// Passing `None` for `api_key` disables Aviationstack enrichment.
    pub fn with_aviationstack(
        mut self,
        api_key: Option<SecretString>,
        base_url: String,
        timeout_secs: u64,
    ) -> Self {
        self.aviationstack = api_key.map(|key| AviationstackConfig {
            api_key: key,
            base_url,
            timeout_secs,
        });
        self
    }

    #[cfg(test)]
    fn with_adsb_aggregator_timeout(mut self, timeout: Duration) -> Self {
        self.adsb_aggregator_timeout = timeout;
        self
    }

    pub fn aviationstack_enabled(&self) -> bool {
        self.aviationstack.is_some()
    }

    async fn fetch_adsb_response(
        &self,
        endpoint: AdsbEndpoint<'_>,
    ) -> Result<AdsbAircraftResponse> {
        let mut last_retryable_error = None;

        for aggregator in &self.adsb_aggregators {
            let url = endpoint.url(aggregator);
            debug!(
                provider = aggregator.name,
                endpoint = endpoint.label(),
                url = %url,
                "Fetching aircraft from ADS-B aggregator"
            );

            match self.fetch_adsb_response_once(aggregator, &url).await {
                Ok(resp) => {
                    debug!(
                        provider = aggregator.name,
                        count = resp.aircraft.len(),
                        "Received aircraft from ADS-B aggregator"
                    );
                    return Ok(resp);
                }
                Err(AdsbFetchError::RateLimited) => {
                    warn!(
                        provider = aggregator.name,
                        "ADS-B aggregator rate-limited; trying fallback"
                    );
                    last_retryable_error = Some(eyre::eyre!("{} returned 429", aggregator.name));
                }
                Err(AdsbFetchError::Timeout(error) | AdsbFetchError::Retryable(error)) => {
                    warn!(
                        provider = aggregator.name,
                        error = ?error,
                        "ADS-B aggregator failed; trying fallback"
                    );
                    last_retryable_error = Some(error);
                }
                Err(AdsbFetchError::Fatal(error)) => return Err(error),
            }
        }

        match last_retryable_error {
            Some(error) => Err(error.wrap_err("All ADS-B aggregators failed")),
            None => Err(eyre::eyre!("No ADS-B aggregators configured")),
        }
    }

    async fn fetch_adsb_response_once(
        &self,
        aggregator: &AdsbAggregator,
        url: &str,
    ) -> std::result::Result<AdsbAircraftResponse, AdsbFetchError> {
        let resp = self
            .http
            .get(url)
            .timeout(self.adsb_aggregator_timeout)
            .send()
            .await
            .map_err(|e| {
                let error = eyre::eyre!("Failed to send request to {}: {e}", aggregator.name);
                if e.is_timeout() {
                    AdsbFetchError::Timeout(error)
                } else if e.is_connect() {
                    AdsbFetchError::Retryable(error)
                } else {
                    // Only known-retryable transport failures (timeout, connect) get
                    // retried; all other send errors are treated as fatal.
                    AdsbFetchError::Fatal(error)
                }
            })?;

        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AdsbFetchError::RateLimited);
        }
        if !status.is_success() {
            return Err(AdsbFetchError::Retryable(eyre::eyre!(
                "{} returned {status}",
                aggregator.name
            )));
        }

        resp.json().await.map_err(|e| {
            AdsbFetchError::Retryable(eyre::eyre!(
                "Failed to parse {} response: {e}",
                aggregator.name
            ))
        })
    }

    pub(super) async fn get_aircraft_nearby(
        &self,
        lat: f64,
        lon: f64,
        radius_nm: u16,
    ) -> Result<Vec<NearbyAircraft>> {
        let resp = self
            .fetch_adsb_response(AdsbEndpoint::Point {
                lat,
                lon,
                radius_nm,
            })
            .await?;
        Ok(resp.aircraft)
    }

    pub async fn get_aircraft_by_hex(&self, hex: &str) -> Result<Option<NearbyAircraft>> {
        debug!(hex = %hex, "Fetching aircraft by hex from ADS-B aggregators");

        let resp = self.fetch_adsb_response(AdsbEndpoint::Hex(hex)).await?;

        Ok(resp.aircraft.into_iter().next())
    }

    pub async fn get_aircraft_by_callsign(&self, callsign: &str) -> Result<Option<NearbyAircraft>> {
        debug!(callsign = %callsign, "Fetching aircraft by callsign from ADS-B aggregators");

        let resp = self
            .fetch_adsb_response(AdsbEndpoint::Callsign(callsign))
            .await?;

        Ok(resp.aircraft.into_iter().next())
    }

    pub async fn get_flight_route(&self, callsign: &str) -> Result<Option<FlightRoute>> {
        let url = format!("{}/callsign/{callsign}", self.adsbdb_base_url);
        debug!(callsign = %callsign, "Fetching flight route from adsbdb");

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .wrap_err("Failed to send request to adsbdb")?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let body: AdsbDbResponse = resp
            .json()
            .await
            .wrap_err("Failed to parse adsbdb response")?;

        Ok(body.response.flightroute)
    }

    pub async fn get_aviationstack_flight_metadata(
        &self,
        identifier: &FlightIdentifier,
        callsign: Option<&str>,
    ) -> Result<Option<AviationstackFlightMetadata>> {
        let Some(config) = &self.aviationstack else {
            return Ok(None);
        };
        let Some((query_key, query_value)) = aviationstack_query(identifier, callsign) else {
            debug!(identifier = %identifier, "Skipping aviationstack lookup: no callsign query");
            return Ok(None);
        };

        let url = format!("{}/flights", config.base_url.trim_end_matches('/'));
        debug!(
            query_key,
            query_value = %query_value,
            "Fetching flight metadata from aviationstack"
        );

        let timeout = Duration::from_secs(config.timeout_secs);
        let resp: AviationstackFlightsResponse = self
            .http
            .get(&url)
            .query(&[
                ("access_key", config.api_key.expose_secret()),
                (query_key, query_value.as_str()),
                ("limit", "1"),
            ])
            .timeout(timeout)
            .send()
            .await
            .wrap_err("Failed to send request to aviationstack")?
            .error_for_status()
            .wrap_err("aviationstack returned error status")?
            .json()
            .await
            .wrap_err("Failed to parse aviationstack response")?;

        Ok(resp
            .data
            .into_iter()
            .next()
            .map(AviationstackFlightMetadata::from))
    }

    /// Resolve a potential IATA flight number to an ICAO callsign.
    pub async fn resolve_callsign(&self, input: &str) -> String {
        if !is_iata_flight_number(input) {
            return input.to_string();
        }

        let (airline_iata, flight_num) = input.split_at(2);

        // Try static CSV lookup first
        if let Some(&icao) = airline_table().get(airline_iata) {
            debug!(iata = %airline_iata, icao = %icao, "Resolved airline code via CSV");
            return format!("{icao}{flight_num}");
        }

        // Fallback: query adsbdb airline API
        debug!(iata = %airline_iata, "Airline not in CSV, trying adsbdb API");
        match tokio::time::timeout(
            AIRLINE_LOOKUP_TIMEOUT,
            self.lookup_airline_icao(airline_iata),
        )
        .await
        {
            Ok(Ok(Some(icao))) => {
                warn!(
                    iata = %airline_iata,
                    icao = %icao,
                    "Resolved airline via adsbdb API — consider adding to airlines.csv"
                );
                format!("{icao}{flight_num}")
            }
            Ok(Ok(None)) => {
                debug!(iata = %airline_iata, "Airline not found in adsbdb");
                input.to_string()
            }
            Ok(Err(e)) => {
                warn!(error = ?e, iata = %airline_iata, "adsbdb airline lookup failed");
                input.to_string()
            }
            Err(_) => {
                warn!(iata = %airline_iata, "adsbdb airline lookup timed out");
                input.to_string()
            }
        }
    }

    /// Query adsbdb for an airline's ICAO code by IATA code.
    async fn lookup_airline_icao(&self, iata: &str) -> Result<Option<String>> {
        let url = format!("{}/airline/{iata}", self.adsbdb_base_url);
        debug!(url = %url, "Fetching airline from adsbdb");

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .wrap_err("Failed to send request to adsbdb")?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let body: AdsbDbAirlineResponse = resp
            .json()
            .await
            .wrap_err("Failed to parse adsbdb airline response")?;

        Ok(body.response.into_iter().next().map(|a| a.icao))
    }

    pub(super) async fn geocode_nominatim(&self, query: &str) -> Result<Option<ResolvedLocation>> {
        let url = format!("{}/search", self.nominatim_base_url);
        debug!(query = %query, "Geocoding via Nominatim");

        let resp = self
            .http
            .get(&url)
            .query(&[("q", query), ("format", "json"), ("limit", "1")])
            .send()
            .await
            .wrap_err("Failed to send request to Nominatim")?
            .error_for_status()
            .wrap_err("Nominatim returned error status")?;

        let results: Vec<NominatimResult> = resp
            .json()
            .await
            .wrap_err("Failed to parse Nominatim response")?;

        let Some(first) = results.into_iter().next() else {
            debug!(query = %query, "Nominatim returned no results");
            return Ok(None);
        };

        let lat: f64 = first.lat.parse().wrap_err("Invalid lat from Nominatim")?;
        let lon: f64 = first.lon.parse().wrap_err("Invalid lon from Nominatim")?;

        // Trim display_name to first comma-separated segment
        let display_name = first
            .display_name
            .split(',')
            .next()
            .unwrap_or(&first.display_name)
            .trim()
            .to_string();

        debug!(query = %query, lat = %lat, lon = %lon, display = %display_name, "Nominatim resolved");
        Ok(Some(ResolvedLocation {
            lat,
            lon,
            display_name,
        }))
    }
}

pub(super) fn aviationstack_query(
    identifier: &FlightIdentifier,
    callsign: Option<&str>,
) -> Option<(&'static str, String)> {
    let candidate = match identifier {
        FlightIdentifier::Callsign(value) => value.as_str(),
        FlightIdentifier::Hex(_) => callsign?,
    }
    .trim();

    if candidate.is_empty() {
        return None;
    }

    let candidate = candidate.to_uppercase();
    if is_iata_flight_number(&candidate) {
        Some(("flight_iata", candidate))
    } else if is_icao_flight_number(&candidate)
        || !matches!(identifier, FlightIdentifier::Hex(_))
        || callsign.is_some()
    {
        Some(("flight_icao", candidate))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn aviation_client() -> AviationClient {
        crate::install_crypto_provider();
        AviationClient::new().unwrap()
    }

    fn test_client_with_aggregators(adsb_aggregators: Vec<AdsbAggregator>) -> AviationClient {
        crate::install_crypto_provider();
        AviationClient::new_with_adsb_aggregators(
            adsb_aggregators,
            "http://adsbdb.test".to_string(),
            "http://nominatim.test".to_string(),
            reqwest::Client::new(),
        )
    }

    fn aircraft_response(flight: &str) -> serde_json::Value {
        serde_json::json!({
            "ac": [{
                "hex": "3c6589",
                "flight": flight,
                "alt_baro": 35000,
                "lat": 50.0,
                "lon": 8.5
            }],
            "ctime": 0,
            "now": 0,
            "total": 1
        })
    }

    #[tokio::test]
    async fn resolve_callsign_translates_iata() {
        let client = aviation_client();
        assert_eq!(client.resolve_callsign("TP247").await, "TAP247");
        assert_eq!(client.resolve_callsign("LH5765").await, "DLH5765");
    }

    #[tokio::test]
    async fn resolve_callsign_passes_through_icao() {
        let client = aviation_client();
        assert_eq!(client.resolve_callsign("TAP247").await, "TAP247");
        assert_eq!(client.resolve_callsign("DLH5765").await, "DLH5765");
    }

    #[tokio::test]
    async fn resolve_callsign_passes_through_hex() {
        let client = aviation_client();
        assert_eq!(client.resolve_callsign("4CA87D").await, "4CA87D");
    }

    #[tokio::test]
    async fn adsb_falls_back_from_5xx_to_next_aggregator() {
        let (primary, backup) = tokio::join!(MockServer::start(), MockServer::start());

        Mock::given(method("GET"))
            .and(path("/hex/3c6589"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&primary)
            .await;

        Mock::given(method("GET"))
            .and(path("/hex/3c6589"))
            .respond_with(ResponseTemplate::new(200).set_body_json(aircraft_response("DLH1234")))
            .mount(&backup)
            .await;

        let client = test_client_with_aggregators(vec![
            AdsbAggregator::readsb_v2("primary", primary.uri()),
            AdsbAggregator::readsb_v2("backup", backup.uri()),
        ]);

        let aircraft = client
            .get_aircraft_by_hex("3c6589")
            .await
            .expect("lookup succeeds through fallback")
            .expect("aircraft returned");

        assert_eq!(aircraft.flight.as_deref(), Some("DLH1234"));
        assert_eq!(primary.received_requests().await.unwrap().len(), 1);
        assert_eq!(backup.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn adsb_falls_back_from_timeout_to_next_aggregator() {
        let (primary, backup) = tokio::join!(MockServer::start(), MockServer::start());

        Mock::given(method("GET"))
            .and(path("/callsign/DLH1234"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(500))
                    .set_body_json(aircraft_response("DLH1234")),
            )
            .mount(&primary)
            .await;

        Mock::given(method("GET"))
            .and(path("/callsign/DLH1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(aircraft_response("DLH1234")))
            .mount(&backup)
            .await;

        let client = test_client_with_aggregators(vec![
            AdsbAggregator::readsb_v2("primary", primary.uri()),
            AdsbAggregator::readsb_v2("backup", backup.uri()),
        ])
        .with_adsb_aggregator_timeout(Duration::from_millis(50));

        let aircraft = client
            .get_aircraft_by_callsign("DLH1234")
            .await
            .expect("lookup succeeds through fallback")
            .expect("aircraft returned");

        assert_eq!(aircraft.flight.as_deref(), Some("DLH1234"));
        assert_eq!(primary.received_requests().await.unwrap().len(), 1);
        assert_eq!(backup.received_requests().await.unwrap().len(), 1);
    }

    #[test]
    fn adsb_fi_point_url_uses_open_data_path() {
        let aggregator = AdsbAggregator::adsb_fi("https://opendata.adsb.fi/api/v2/".to_string());
        let url = AdsbEndpoint::Point {
            lat: 1.5,
            lon: 2.25,
            radius_nm: 15,
        }
        .url(&aggregator);

        assert_eq!(
            url,
            "https://opendata.adsb.fi/api/v2/lat/1.5/lon/2.25/dist/15"
        );
    }

    #[test]
    fn adsb_response_accepts_aircraft_alias() {
        let response: AdsbAircraftResponse = serde_json::from_value(serde_json::json!({
            "aircraft": [{
                "hex": "3c6589",
                "flight": "DLH1234",
                "alt_baro": 35000
            }]
        }))
        .unwrap();

        assert_eq!(response.aircraft.len(), 1);
        assert_eq!(response.aircraft[0].flight.as_deref(), Some("DLH1234"));
    }

    fn ac(hex: Option<&str>, seen_pos: Option<f64>, rssi: Option<f64>) -> NearbyAircraft {
        NearbyAircraft {
            hex: hex.map(str::to_owned),
            flight: None,
            r: None,
            t: None,
            alt_baro: None,
            lat: None,
            lon: None,
            gs: None,
            baro_rate: None,
            geom_rate: None,
            squawk: None,
            nav_modes: None,
            rssi,
            seen_pos,
        }
    }

    #[test]
    fn merge_unions_distinct_aircraft() {
        let merged = merge_aircraft(vec![
            vec![ac(Some("aaa111"), Some(1.0), Some(-10.0))],
            vec![ac(Some("bbb222"), Some(1.0), Some(-10.0))],
        ]);
        let hexes: Vec<_> = merged.iter().filter_map(|a| a.hex.clone()).collect();
        assert_eq!(merged.len(), 2);
        assert!(hexes.contains(&"aaa111".to_owned()));
        assert!(hexes.contains(&"bbb222".to_owned()));
    }

    #[test]
    fn merge_keeps_fresher_copy() {
        let merged = merge_aircraft(vec![
            vec![ac(Some("aaa111"), Some(1.0), Some(-30.0))],
            vec![ac(Some("aaa111"), Some(30.0), Some(-5.0))],
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].seen_pos, Some(1.0));
    }

    #[test]
    fn merge_breaks_freshness_tie_by_rssi() {
        let merged = merge_aircraft(vec![
            vec![ac(Some("aaa111"), Some(1.0), Some(-30.0))],
            vec![ac(Some("aaa111"), Some(2.0), Some(-5.0))],
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].rssi, Some(-5.0));
    }

    #[test]
    fn merge_bucket_beats_rssi() {
        let merged = merge_aircraft(vec![
            vec![ac(Some("aaa111"), Some(1.0), Some(-30.0))],
            vec![ac(Some("aaa111"), Some(20.0), Some(-1.0))],
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].seen_pos, Some(1.0));
    }

    #[test]
    fn merge_missing_seen_pos_loses() {
        let merged = merge_aircraft(vec![
            vec![ac(Some("aaa111"), None, Some(-1.0))],
            vec![ac(Some("aaa111"), Some(10.0), Some(-30.0))],
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].seen_pos, Some(10.0));
    }

    #[test]
    fn merge_garbage_seen_pos_loses() {
        for garbage in [-1.0_f64, f64::NAN, f64::INFINITY] {
            let merged = merge_aircraft(vec![
                vec![ac(Some("aaa111"), Some(garbage), Some(-1.0))],
                vec![ac(Some("aaa111"), Some(10.0), Some(-30.0))],
            ]);
            assert_eq!(merged.len(), 1);
            assert_eq!(
                merged[0].seen_pos,
                Some(10.0),
                "garbage {garbage} must lose"
            );
        }
    }

    #[test]
    fn merge_tie_keeps_first_backend() {
        let mut first = ac(Some("aaa111"), Some(1.0), Some(-10.0));
        first.flight = Some("FIRST".to_owned());
        let mut second = ac(Some("aaa111"), Some(1.0), Some(-10.0));
        second.flight = Some("SECOND".to_owned());
        let merged = merge_aircraft(vec![vec![first], vec![second]]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].flight.as_deref(), Some("FIRST"));
    }

    #[test]
    fn merge_dedups_case_insensitively_but_keeps_original_case() {
        let merged = merge_aircraft(vec![
            vec![ac(Some("3C6589"), Some(1.0), Some(-5.0))],
            vec![ac(Some("3c6589"), Some(30.0), Some(-50.0))],
        ]);
        assert_eq!(merged.len(), 1, "different-case hex must dedup");
        assert_eq!(
            merged[0].hex.as_deref(),
            Some("3C6589"),
            "stored hex unchanged"
        );
    }

    #[test]
    fn merge_keeps_hexless_aircraft() {
        let merged = merge_aircraft(vec![vec![
            ac(Some("aaa111"), Some(1.0), Some(-10.0)),
            ac(None, Some(1.0), Some(-10.0)),
        ]]);
        assert_eq!(merged.len(), 2);
    }

    #[tokio::test]
    async fn fetch_once_classifies_429_as_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/hex/3c6589"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let client =
            test_client_with_aggregators(vec![AdsbAggregator::readsb_v2("primary", server.uri())]);
        let aggregator = AdsbAggregator::readsb_v2("primary", server.uri());
        let url = format!("{}/hex/3c6589", server.uri());

        let err = client
            .fetch_adsb_response_once(&aggregator, &url)
            .await
            .expect_err("429 is an error");
        assert!(matches!(err, AdsbFetchError::RateLimited));
    }
}
