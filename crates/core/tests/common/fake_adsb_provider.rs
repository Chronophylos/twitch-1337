use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eyre::Result;
use twitch_1337_core::aviation::{AdsbProvider, Airport, FlightRoute, NearbyAircraft};

#[derive(Clone, Default)]
pub struct FakeAdsbProvider {
    by_hex: Arc<Mutex<HashMap<String, NearbyAircraft>>>,
    by_callsign: Arc<Mutex<HashMap<String, NearbyAircraft>>>,
    routes: Arc<Mutex<HashMap<String, FlightRoute>>>,
    aviationstack_enabled: bool,
}

impl FakeAdsbProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_aviationstack(mut self, enabled: bool) -> Self {
        self.aviationstack_enabled = enabled;
        self
    }

    pub fn insert_hex(self, hex: &str, aircraft: NearbyAircraft) -> Self {
        self.by_hex
            .lock()
            .unwrap()
            .insert(hex.to_uppercase(), aircraft);
        self
    }

    pub fn insert_callsign(self, callsign: &str, aircraft: NearbyAircraft) -> Self {
        self.by_callsign
            .lock()
            .unwrap()
            .insert(callsign.to_uppercase(), aircraft);
        self
    }

    pub fn insert_route(self, callsign: &str, origin: &str, dest: &str) -> Self {
        self.routes.lock().unwrap().insert(
            callsign.to_uppercase(),
            FlightRoute {
                origin: Airport {
                    iata_code: origin.to_string(),
                },
                destination: Airport {
                    iata_code: dest.to_string(),
                },
            },
        );
        self
    }
}

#[async_trait]
impl AdsbProvider for FakeAdsbProvider {
    async fn get_aircraft_by_hex(&self, hex: &str) -> Result<Option<NearbyAircraft>> {
        Ok(self
            .by_hex
            .lock()
            .unwrap()
            .get(&hex.to_uppercase())
            .cloned())
    }

    async fn get_aircraft_by_callsign(
        &self,
        callsign: &str,
        _preferred_hex: Option<&str>,
    ) -> Result<Option<NearbyAircraft>> {
        Ok(self
            .by_callsign
            .lock()
            .unwrap()
            .get(&callsign.to_uppercase())
            .cloned())
    }

    async fn get_flight_route(&self, callsign: &str) -> Result<Option<FlightRoute>> {
        Ok(self
            .routes
            .lock()
            .unwrap()
            .get(&callsign.to_uppercase())
            .cloned())
    }

    async fn resolve_callsign(&self, input: &str) -> String {
        input.to_string()
    }

    fn aviationstack_enabled(&self) -> bool {
        self.aviationstack_enabled
    }
}
