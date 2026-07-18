use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("no airports match the aircraft's runway requirements")]
    NoValidAirports,

    #[error("no candidate arrival airports found within distance band")]
    NoCandidateArrivals,

    #[error("exhausted {attempts} retries without finding a valid pair")]
    RetriesExhausted { attempts: u32 },

    #[error("unknown airport ICAO code: {icao}")]
    UnknownAirport { icao: String },

    #[error("flight distance {distance_nm:.0} nm exceeds aircraft range of {range_nm:.0} nm")]
    RangeExceeded { distance_nm: f64, range_nm: f64 },

    #[error("runway at {airport_icao} is {available_ft} ft, aircraft requires {required_ft} ft")]
    RunwayTooShort {
        airport_icao: String,
        required_ft: u32,
        available_ft: u32,
    },
}
