//! HTTP client for adsb.lol, adsbdb, Nominatim, and Aviationstack.

use std::time::Duration;

use eyre::{Result, WrapErr as _};
use secrecy::ExposeSecret as _;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::config::AviationstackConfig;
use crate::util::APP_USER_AGENT;

use super::location::{
    ResolvedLocation, airline_table, is_iata_flight_number, is_icao_flight_number,
};
use super::tracker::FlightIdentifier;
use super::types::{
    AdsbDbAirlineResponse, AdsbDbResponse, AdsbLolResponse, AviationstackFlightMetadata,
    AviationstackFlightsResponse, FlightRoute, NearbyAircraft,
};

const ADSBDB_BASE_URL: &str = "https://api.adsbdb.com/v0";
const ADSBLOL_BASE_URL: &str = "https://api.adsb.lol/v2";
const NOMINATIM_BASE_URL: &str = "https://nominatim.openstreetmap.org";
const AIRLINE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
struct NominatimResult {
    lat: String,
    lon: String,
    display_name: String,
}

#[derive(Clone)]
pub struct AviationClient {
    http: reqwest::Client,
    adsblol_base_url: String,
    adsbdb_base_url: String,
    nominatim_base_url: String,
    aviationstack: Option<AviationstackConfig>,
}

impl AviationClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .build()
            .wrap_err("Failed to build aviation HTTP client")?;
        Ok(Self::new_with_base_url(
            ADSBLOL_BASE_URL.to_owned(),
            ADSBDB_BASE_URL.to_owned(),
            NOMINATIM_BASE_URL.to_owned(),
            http,
        ))
    }

    pub fn new_with_base_url(
        adsblol_base_url: String,
        adsbdb_base_url: String,
        nominatim_base_url: String,
        http_client: reqwest::Client,
    ) -> Self {
        Self {
            http: http_client,
            adsblol_base_url,
            adsbdb_base_url,
            nominatim_base_url,
            aviationstack: None,
        }
    }

    pub fn with_aviationstack_config(mut self, aviationstack: Option<AviationstackConfig>) -> Self {
        self.aviationstack = aviationstack.filter(|cfg| cfg.enabled);
        self
    }

    pub fn aviationstack_enabled(&self) -> bool {
        self.aviationstack.is_some()
    }

    pub(super) async fn get_aircraft_nearby(
        &self,
        lat: f64,
        lon: f64,
        radius_nm: u16,
    ) -> Result<Vec<NearbyAircraft>> {
        let url = format!("{}/point/{lat}/{lon}/{radius_nm}", self.adsblol_base_url);
        debug!(url = %url, "Fetching nearby aircraft from adsb.lol");

        let resp: AdsbLolResponse = self
            .http
            .get(&url)
            .send()
            .await
            .wrap_err("Failed to send request to adsb.lol")?
            .error_for_status()
            .wrap_err("adsb.lol returned error status")?
            .json()
            .await
            .wrap_err("Failed to parse adsb.lol response")?;

        debug!(count = resp.ac.len(), "Received aircraft from adsb.lol");
        Ok(resp.ac)
    }

    pub async fn get_aircraft_by_hex(&self, hex: &str) -> Result<Option<NearbyAircraft>> {
        let url = format!("{}/hex/{hex}", self.adsblol_base_url);
        debug!(hex = %hex, "Fetching aircraft by hex from adsb.lol");

        let resp: AdsbLolResponse = self
            .http
            .get(&url)
            .send()
            .await
            .wrap_err("Failed to send request to adsb.lol")?
            .error_for_status()
            .wrap_err("adsb.lol returned error status")?
            .json()
            .await
            .wrap_err("Failed to parse adsb.lol response")?;

        Ok(resp.ac.into_iter().next())
    }

    pub async fn get_aircraft_by_callsign(&self, callsign: &str) -> Result<Option<NearbyAircraft>> {
        let url = format!("{}/callsign/{callsign}", self.adsblol_base_url);
        debug!(callsign = %callsign, "Fetching aircraft by callsign from adsb.lol");

        let resp: AdsbLolResponse = self
            .http
            .get(&url)
            .send()
            .await
            .wrap_err("Failed to send request to adsb.lol")?
            .error_for_status()
            .wrap_err("adsb.lol returned error status")?
            .json()
            .await
            .wrap_err("Failed to parse adsb.lol response")?;

        Ok(resp.ac.into_iter().next())
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

    #[tokio::test]
    async fn resolve_callsign_translates_iata() {
        let client = AviationClient::new().unwrap();
        assert_eq!(client.resolve_callsign("TP247").await, "TAP247");
        assert_eq!(client.resolve_callsign("LH5765").await, "DLH5765");
    }

    #[tokio::test]
    async fn resolve_callsign_passes_through_icao() {
        let client = AviationClient::new().unwrap();
        assert_eq!(client.resolve_callsign("TAP247").await, "TAP247");
        assert_eq!(client.resolve_callsign("DLH5765").await, "DLH5765");
    }

    #[tokio::test]
    async fn resolve_callsign_passes_through_hex() {
        let client = AviationClient::new().unwrap();
        assert_eq!(client.resolve_callsign("4CA87D").await, "4CA87D");
    }
}
