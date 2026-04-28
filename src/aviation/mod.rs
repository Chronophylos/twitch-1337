pub mod commands;
mod location;
pub mod tracker;
mod types;

pub use tracker::{FlightIdentifier, TrackerCommand, run_flight_tracker};
pub use types::{Airport, AltBaro, AviationstackFlightMetadata, FlightRoute, NearbyAircraft};

pub(crate) use location::iata_to_coords;
pub(crate) use types::AdsbLolResponse;

use location::{
    ResolveResult, ResolvedLocation, airline_table, is_iata_flight_number, is_icao_flight_number,
    resolve_location,
};
use types::{AdsbDbAirlineResponse, AdsbDbResponse, AviationstackFlightsResponse};

use eyre::{Result, WrapErr};
use secrecy::ExposeSecret as _;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, warn};
use twitch_irc::{
    TwitchIRCClient, login::LoginCredentials, message::PrivmsgMessage, transport::Transport,
};

use crate::config::AviationstackConfig;
use crate::cooldown::format_cooldown_remaining;
use crate::util::{APP_USER_AGENT, MAX_RESPONSE_LENGTH, truncate_response};

const ADSBDB_BASE_URL: &str = "https://api.adsbdb.com/v0";
const ADSBLOL_BASE_URL: &str = "https://api.adsb.lol/v2";
const NOMINATIM_BASE_URL: &str = "https://nominatim.openstreetmap.org";
const UP_SEARCH_RADIUS_NM: u16 = 15;
const UP_COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const UP_ADSBLOL_TIMEOUT: Duration = Duration::from_secs(10);
const UP_ADSBDB_TIMEOUT: Duration = Duration::from_secs(5);
const AIRLINE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const UP_MAX_CANDIDATES: usize = 10;
const UP_MAX_RESULTS: usize = 5;
const UP_CONE_REFERENCE_ALT_FT: f64 = 35_000.0;

// --- Nominatim types ---

#[derive(Debug, Deserialize)]
struct NominatimResult {
    lat: String,
    lon: String,
    display_name: String,
}

// --- AviationClient ---

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

    async fn get_aircraft_nearby(
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

    pub(in crate::aviation) async fn geocode_nominatim(
        &self,
        query: &str,
    ) -> Result<Option<ResolvedLocation>> {
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

fn aviationstack_query(
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

// --- Command ---

fn cone_distance_nm(ac: &NearbyAircraft, center_lat: f64, center_lon: f64) -> Option<f64> {
    let (Some(ac_lat), Some(ac_lon), Some(alt)) = (ac.lat, ac.lon, &ac.alt_baro) else {
        return None;
    };
    let alt_ft = match alt {
        AltBaro::Feet(ft) if *ft > 0 => *ft as f64,
        _ => return None,
    };
    let distance =
        random_flight::geo::haversine_distance_nm(center_lat, center_lon, ac_lat, ac_lon);
    let max_distance = alt_ft * f64::from(UP_SEARCH_RADIUS_NM) / UP_CONE_REFERENCE_ALT_FT;
    if distance <= max_distance {
        Some(distance)
    } else {
        None
    }
}

fn format_altitude(alt: &Option<AltBaro>) -> String {
    match alt {
        Some(AltBaro::Feet(ft)) if *ft >= 1000 => format!("FL{}", ft / 100),
        Some(AltBaro::Feet(ft)) => format!("{ft}ft"),
        Some(AltBaro::Ground) => "GND".to_string(),
        None => "?".to_string(),
    }
}

pub async fn up_command<T, L>(
    privmsg: &PrivmsgMessage,
    client: &Arc<TwitchIRCClient<T, L>>,
    aviation_client: &AviationClient,
    input: &str,
    cooldown: &crate::cooldown::PerUserCooldown,
) -> Result<()>
where
    T: Transport,
    L: LoginCredentials,
{
    let user = &privmsg.sender.login;
    let input = input.trim();

    // Empty input
    if input.is_empty() {
        if let Err(e) = client
            .say_in_reply_to(
                privmsg,
                "Benutzung: !up <PLZ/ICAO/IATA/Ort> FDM".to_string(),
            )
            .await
        {
            error!(error = ?e, "Failed to send usage message");
        }
        return Ok(());
    }

    // Check cooldown
    if let Some(remaining) = cooldown.check(user).await {
        debug!(user = %user, remaining_secs = remaining.as_secs(), "!up on cooldown");
        if let Err(e) = client
            .say_in_reply_to(
                privmsg,
                format!(
                    "Bitte warte noch {} Waiting",
                    format_cooldown_remaining(remaining)
                ),
            )
            .await
        {
            error!(error = ?e, "Failed to send cooldown message");
        }
        return Ok(());
    }

    cooldown.record(user).await;

    // Resolve location
    let location = match resolve_location(input, aviation_client).await {
        Ok(ResolveResult::Found(loc)) => loc,
        Ok(ResolveResult::PlzNotFound) => {
            if let Err(e) = client
                .say_in_reply_to(privmsg, "Kenne ich nicht die PLZ FDM".to_string())
                .await
            {
                error!(error = ?e, "Failed to send unknown PLZ message");
            }
            return Ok(());
        }
        Ok(ResolveResult::NotFound) => {
            if let Err(e) = client
                .say_in_reply_to(privmsg, "Kenne ich nicht FDM".to_string())
                .await
            {
                error!(error = ?e, "Failed to send not-found message");
            }
            return Ok(());
        }
        Err(e) => {
            error!(error = ?e, input = %input, "Location resolution failed");
            if let Err(e) = client
                .say_in_reply_to(privmsg, "Da ist was schiefgelaufen FDM".to_string())
                .await
            {
                error!(error = ?e, "Failed to send error message");
            }
            return Ok(());
        }
    };

    let ResolvedLocation {
        lat,
        lon,
        display_name,
    } = &location;
    debug!(input = %input, lat = %lat, lon = %lon, display = %display_name, "Looking up aircraft");

    // Wrap entire API flow in overall timeout
    let result = tokio::time::timeout(UP_COMMAND_TIMEOUT, async {
        // Fetch nearby aircraft
        let aircraft = tokio::time::timeout(
            UP_ADSBLOL_TIMEOUT,
            aviation_client.get_aircraft_nearby(*lat, *lon, UP_SEARCH_RADIUS_NM),
        )
        .await
        .map_err(|_| eyre::eyre!("adsb.lol request timed out"))?
        .wrap_err("adsb.lol request failed")?;

        // Filter by cone visibility, then by callsign
        let candidates: Vec<_> = aircraft
            .iter()
            .filter_map(|ac| {
                let distance_nm = cone_distance_nm(ac, *lat, *lon)?;
                let callsign = ac.flight.as_ref()?.trim();
                if callsign.is_empty() {
                    return None;
                }
                Some((callsign.to_string(), ac, distance_nm))
            })
            .take(UP_MAX_CANDIDATES)
            .collect();

        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        // Fetch routes concurrently
        let mut join_set = tokio::task::JoinSet::new();
        for (callsign, ac, distance_nm) in &candidates {
            let av_client = aviation_client.clone();
            let cs = callsign.clone();
            let icao_type = ac.t.clone();
            let alt = ac.alt_baro.clone();
            let dist = *distance_nm;
            let (ac_lat, ac_lon) = (
                ac.lat.expect("lat guaranteed by cone_distance_nm"),
                ac.lon.expect("lon guaranteed by cone_distance_nm"),
            );
            let bearing = random_flight::geo::initial_bearing(*lat, *lon, ac_lat, ac_lon);
            let direction = match random_flight::geo::cardinal_direction(bearing) {
                "N" => "↑",
                "NE" => "↗",
                "E" => "→",
                "SE" => "↘",
                "S" => "↓",
                "SW" => "↙",
                "W" => "←",
                "NW" => "↖",
                _ => "?",
            };
            join_set.spawn(async move {
                let route =
                    tokio::time::timeout(UP_ADSBDB_TIMEOUT, av_client.get_flight_route(&cs)).await;

                match route {
                    Ok(Ok(Some(fr))) => Some((cs, icao_type, alt, fr, dist, direction)),
                    Ok(Ok(None)) => None,
                    Ok(Err(e)) => {
                        warn!(callsign = %cs, error = ?e, "adsbdb lookup failed");
                        None
                    }
                    Err(_) => {
                        warn!(callsign = %cs, "adsbdb lookup timed out");
                        None
                    }
                }
            });
        }

        let mut results = Vec::new();
        while let Some(res) = join_set.join_next().await {
            if let Ok(Some(entry)) = res {
                results.push(entry);
            }
        }

        Ok::<_, eyre::Report>(results)
    })
    .await;

    let response = match result {
        Ok(Ok(entries)) if entries.is_empty() => {
            format!("Nix los über {display_name}")
        }
        Ok(Ok(entries)) => {
            let total = entries.len();
            let parts: Vec<String> = entries
                .iter()
                .take(UP_MAX_RESULTS)
                .map(|(cs, icao_type, alt, route, dist, direction)| {
                    let typ = icao_type.as_deref().unwrap_or("?");
                    format!(
                        "{cs} ({typ}) {origin}→{dest} {alt} {dist:.1}nm {direction}",
                        origin = route.origin.iata_code,
                        dest = route.destination.iata_code,
                        alt = format_altitude(alt),
                    )
                })
                .collect();
            let joined = parts.join(" | ");
            let msg = format!("✈ {total} Flieger über {display_name}: {joined}");
            truncate_response(&msg, MAX_RESPONSE_LENGTH)
        }
        Ok(Err(e)) => {
            error!(error = ?e, input = %input, "!up command failed");
            "Da ist was schiefgelaufen FDM".to_string()
        }
        Err(_) => {
            error!(input = %input, "!up command timed out");
            "Da ist was schiefgelaufen FDM".to_string()
        }
    };

    if let Err(e) = client.say_in_reply_to(privmsg, response).await {
        error!(error = ?e, "Failed to send !up response");
    }

    Ok(())
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
