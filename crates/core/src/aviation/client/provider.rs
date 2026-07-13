use async_trait::async_trait;
use eyre::Result;

use crate::aviation::types::{FlightRoute, NearbyAircraft};

use super::AviationClient;

/// ADS-B position and route lookups used by the flight-tracker poll loop.
#[async_trait]
pub trait AdsbProvider: Send + Sync {
    async fn get_aircraft_by_hex(&self, hex: &str) -> Result<Option<NearbyAircraft>>;
    async fn get_aircraft_by_callsign(&self, callsign: &str) -> Result<Option<NearbyAircraft>>;
    async fn get_flight_route(&self, callsign: &str) -> Result<Option<FlightRoute>>;
    async fn resolve_callsign(&self, input: &str) -> String;
    fn aviationstack_enabled(&self) -> bool;
}

#[async_trait]
impl AdsbProvider for AviationClient {
    async fn get_aircraft_by_hex(&self, hex: &str) -> Result<Option<NearbyAircraft>> {
        self.get_aircraft_by_hex(hex).await
    }

    async fn get_aircraft_by_callsign(&self, callsign: &str) -> Result<Option<NearbyAircraft>> {
        self.get_aircraft_by_callsign(callsign).await
    }

    async fn get_flight_route(&self, callsign: &str) -> Result<Option<FlightRoute>> {
        self.get_flight_route(callsign).await
    }

    async fn resolve_callsign(&self, input: &str) -> String {
        self.resolve_callsign(input).await
    }

    fn aviationstack_enabled(&self) -> bool {
        self.aviationstack_enabled()
    }
}
