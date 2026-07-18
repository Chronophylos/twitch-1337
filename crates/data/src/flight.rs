#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AirportSize {
    Large,
    Medium,
    Small,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FuelType {
    Jet,
    Avgas,
}

#[derive(Debug, Clone)]
pub struct Airport {
    pub icao: &'static str,
    pub latitude: f64,
    pub longitude: f64,
    pub elevation_ft: i32,
    pub runway_length_ft: u32,
    pub size: AirportSize,
}

#[derive(Debug, Clone)]
pub struct Aircraft {
    pub name: &'static str,
    pub icao_type: &'static str,
    pub cruise_speed_ktas: u16,
    pub cruise_altitude_ft: u32,
    pub service_ceiling_ft: u32,
    pub min_runway_length_ft: u32,
    pub climb_speed_ktas: u16,
    pub climb_rate_fpm: u16,
    pub descent_speed_ktas: u16,
    pub descent_rate_fpm: u16,
    pub fuel_capacity_kg: f64,
    pub fuel_type: FuelType,
    pub fuel_flow_climb_kg_per_hour: f64,
    pub fuel_flow_cruise_kg_per_hour: f64,
    pub fuel_flow_descent_kg_per_hour: f64,
}

impl Aircraft {
    pub fn range_nm(&self) -> f64 {
        (self.fuel_capacity_kg * 0.95 / self.fuel_flow_cruise_kg_per_hour)
            * f64::from(self.cruise_speed_ktas)
    }
}

include!(concat!(env!("OUT_DIR"), "/airport_db.rs"));
include!(concat!(env!("OUT_DIR"), "/aircraft_db.rs"));

pub fn airports() -> &'static [Airport] {
    AIRPORTS
}

pub fn aircraft() -> &'static [Aircraft] {
    AIRCRAFT_DB
}

pub fn aircraft_by_icao_type(icao_type: &str) -> Option<&'static Aircraft> {
    AIRCRAFT_DB
        .iter()
        .find(|a| a.icao_type.eq_ignore_ascii_case(icao_type))
}

pub fn find_by_icao(icao: &str) -> Option<&'static Airport> {
    AIRPORTS.iter().find(|a| a.icao.eq_ignore_ascii_case(icao))
}

pub fn filter_by_runway(min_length_ft: u32) -> Vec<&'static Airport> {
    AIRPORTS
        .iter()
        .filter(|a| a.runway_length_ft >= min_length_ft)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b738_and_kjfk_exist() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        assert_eq!(ac.icao_type, "B738");
        let apt = find_by_icao("KJFK").expect("KJFK");
        assert_eq!(apt.icao, "KJFK");
        assert!(apt.runway_length_ft > 10000);
    }

    #[test]
    fn airport_has_no_name_field() {
        assert!(find_by_icao("EDDF").is_some());
        let db = include_str!(concat!(env!("OUT_DIR"), "/airport_db.rs"));
        assert!(
            !db.contains("name:"),
            "generated airport_db.rs must not emit a name field"
        );
    }
}
