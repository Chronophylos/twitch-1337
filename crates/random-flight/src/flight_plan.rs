use std::time::Duration;

use twitch_1337_data::flight::{Aircraft, Airport, AirportSize};

use crate::geo::haversine_distance_nm;

fn taxi_out_for(size: AirportSize) -> Duration {
    match size {
        AirportSize::Large => Duration::from_secs(18 * 60),
        AirportSize::Medium => Duration::from_secs(10 * 60),
        AirportSize::Small => Duration::from_secs(5 * 60),
    }
}

fn taxi_in_for(size: AirportSize) -> Duration {
    match size {
        AirportSize::Large => Duration::from_secs(10 * 60),
        AirportSize::Medium => Duration::from_secs(6 * 60),
        AirportSize::Small => Duration::from_secs(3 * 60),
    }
}

#[derive(Debug, Clone)]
pub struct FlightPlan {
    pub departure: &'static Airport,
    pub arrival: &'static Airport,
    pub aircraft: Aircraft,
    pub distance_nm: f64,
    pub block_time: Duration,
    pub taxi_out: Duration,
    pub taxi_in: Duration,
    pub cruise_altitude_ft: u32,
    pub climb_time: Duration,
    pub climb_distance_nm: f64,
    pub descent_time: Duration,
    pub descent_distance_nm: f64,
    pub cruise_time: Duration,
    pub cruise_distance_nm: f64,
}

impl FlightPlan {
    /// Build a SimBrief dispatch URL pre-filled with this flight plan's parameters.
    pub fn simbrief_url(&self) -> String {
        format!(
            "https://dispatch.simbrief.com/options/custom?orig={}&dest={}&type={}&fl={}",
            self.departure.icao,
            self.arrival.icao,
            self.aircraft.icao_type,
            self.cruise_altitude_ft,
        )
    }
}

pub fn calculate_flight_plan(
    departure: &'static Airport,
    arrival: &'static Airport,
    aircraft: &Aircraft,
) -> FlightPlan {
    let distance_nm = haversine_distance_nm(
        departure.latitude,
        departure.longitude,
        arrival.latitude,
        arrival.longitude,
    );

    let cruise_speed = f64::from(aircraft.cruise_speed_ktas);
    let climb_speed = f64::from(aircraft.climb_speed_ktas);
    let descent_speed = f64::from(aircraft.descent_speed_ktas);

    let mut cruise_alt = aircraft.cruise_altitude_ft;
    let min_alt = (departure.elevation_ft.max(arrival.elevation_ft) + 1000).max(0) as u32;

    loop {
        let climb_ft = f64::from(cruise_alt.saturating_sub(departure.elevation_ft.max(0) as u32));
        let descent_ft = f64::from(cruise_alt.saturating_sub(arrival.elevation_ft.max(0) as u32));

        let climb_time_hrs = climb_ft / f64::from(aircraft.climb_rate_fpm) / 60.0;
        let descent_time_hrs = descent_ft / f64::from(aircraft.descent_rate_fpm) / 60.0;

        let climb_dist = climb_speed * climb_time_hrs;
        let descent_dist = descent_speed * descent_time_hrs;

        if climb_dist + descent_dist < distance_nm || cruise_alt <= min_alt {
            let cruise_dist = (distance_nm - climb_dist - descent_dist).max(0.0);
            let cruise_time_hrs = cruise_dist / cruise_speed;

            let to_duration = |hrs: f64| Duration::from_secs_f64(hrs * 3600.0);

            let climb_time = to_duration(climb_time_hrs);
            let descent_time = to_duration(descent_time_hrs);
            let cruise_time = to_duration(cruise_time_hrs);
            let taxi_out = taxi_out_for(departure.size);
            let taxi_in = taxi_in_for(arrival.size);
            let block_time = climb_time + descent_time + cruise_time + taxi_out + taxi_in;

            return FlightPlan {
                departure,
                arrival,
                aircraft: aircraft.clone(),
                distance_nm,
                block_time,
                taxi_out,
                taxi_in,
                cruise_altitude_ft: cruise_alt,
                climb_time,
                climb_distance_nm: climb_dist,
                descent_time,
                descent_distance_nm: descent_dist,
                cruise_time,
                cruise_distance_nm: cruise_dist,
            };
        }

        // Reduce altitude and retry
        cruise_alt = cruise_alt.saturating_sub(1000).max(min_alt);
    }
}

/// Estimate the total flight distance for a target block time.
/// Used by the selection algorithm to narrow the airport search band.
pub fn estimate_distance_for_block_time(aircraft: &Aircraft, target_block_time: Duration) -> f64 {
    let taxi_estimate = taxi_out_for(AirportSize::Medium) + taxi_in_for(AirportSize::Medium);
    let flight_time_hrs = target_block_time
        .saturating_sub(taxi_estimate)
        .as_secs_f64()
        / 3600.0;
    let effective_speed = f64::from(aircraft.cruise_speed_ktas) * 0.90;
    effective_speed * flight_time_hrs
}

#[cfg(test)]
mod tests {
    use super::*;
    use twitch_1337_data::flight::{aircraft_by_icao_type, find_by_icao};

    #[test]
    fn basic_flight_plan_computes() {
        let dep = find_by_icao("KJFK").expect("KJFK");
        let arr = find_by_icao("KLAX").expect("KLAX");
        let ac = aircraft_by_icao_type("B738").expect("B738");

        let fp = calculate_flight_plan(dep, arr, ac);

        // JFK-LAX is ~2145 nm, B738 at 460 kts ~ 4.7h flight + taxi
        assert!(
            fp.distance_nm > 2000.0 && fp.distance_nm < 2300.0,
            "distance was {} nm",
            fp.distance_nm
        );
        assert!(fp.block_time.as_secs() > 4 * 3600, "block time too short");
        assert!(
            fp.block_time.as_secs() < 6 * 3600,
            "block time too long: {}s",
            fp.block_time.as_secs()
        );
        assert!(fp.cruise_distance_nm > 0.0);
        assert!(fp.climb_time.as_secs() > 0);
        assert!(fp.descent_time.as_secs() > 0);
    }

    #[test]
    fn short_flight_reduces_altitude() {
        let dep = find_by_icao("EDDF").expect("EDDF");
        let arr = find_by_icao("EDDM").expect("EDDM");
        let ac = aircraft_by_icao_type("B738").expect("B738");

        let fp = calculate_flight_plan(dep, arr, ac);

        // ~152 nm, jet can't reach FL360 — altitude should be reduced
        assert!(
            fp.cruise_altitude_ft < 36000,
            "cruise alt should be reduced for short flight, was {}",
            fp.cruise_altitude_ft
        );
        assert!(fp.cruise_distance_nm >= 0.0);
    }

    #[test]
    fn block_time_equals_sum_of_phases() {
        let dep = find_by_icao("KJFK").expect("KJFK");
        let arr = find_by_icao("EGLL").expect("EGLL");
        let ac = aircraft_by_icao_type("B738").expect("B738");

        let fp = calculate_flight_plan(dep, arr, ac);

        let sum = fp.climb_time + fp.cruise_time + fp.descent_time + fp.taxi_out + fp.taxi_in;
        assert!(
            fp.block_time.abs_diff(sum).as_millis() < 10,
            "block_time should equal sum of phases"
        );
    }

    #[test]
    fn c172_short_hop() {
        let dep = find_by_icao("EDDF").expect("EDDF");
        let arr = find_by_icao("EDDM").expect("EDDM");
        let ac = aircraft_by_icao_type("C172").expect("C172");

        let fp = calculate_flight_plan(dep, arr, ac);

        // ~152 nm at 122 kts ~ 1.25h + climb/descent + taxi ~ 1.5-2h
        assert!(fp.block_time.as_secs() > 3600, "too short");
        assert!(fp.block_time.as_secs() < 3 * 3600, "too long");
    }

    #[test]
    fn estimate_distance_reasonable() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        let target = Duration::from_secs(2 * 3600); // 2 hours

        let est = estimate_distance_for_block_time(ac, target);

        // B738 at ~414 kts effective * ~1.73h ≈ 717 nm
        assert!(est > 500.0 && est < 900.0, "estimate was {est} nm");
    }

    #[test]
    fn simbrief_url_contains_correct_parameters() {
        let dep = find_by_icao("KJFK").expect("KJFK");
        let arr = find_by_icao("KLAX").expect("KLAX");
        let ac = aircraft_by_icao_type("B738").expect("B738");

        let fp = calculate_flight_plan(dep, arr, ac);
        let url = fp.simbrief_url();

        assert!(
            url.starts_with("https://dispatch.simbrief.com/options/custom?"),
            "unexpected base URL: {url}"
        );
        assert!(url.contains("orig=KJFK"), "missing orig: {url}");
        assert!(url.contains("dest=KLAX"), "missing dest: {url}");
        assert!(url.contains("type=B738"), "missing type: {url}");
        assert!(
            url.contains(&format!("fl={}", fp.cruise_altitude_ft)),
            "missing or wrong fl: {url}"
        );
    }

    #[test]
    fn taxi_times_vary_by_airport_size() {
        let dep_large = find_by_icao("KJFK").expect("KJFK"); // large
        let arr = find_by_icao("KLAX").expect("KLAX"); // large
        let ac = aircraft_by_icao_type("B738").expect("B738");

        let fp = calculate_flight_plan(dep_large, arr, ac);
        assert_eq!(
            fp.taxi_out,
            Duration::from_secs(18 * 60),
            "large airport taxi_out should be 18 min"
        );
        assert_eq!(
            fp.taxi_in,
            Duration::from_secs(10 * 60),
            "large airport taxi_in should be 10 min"
        );
    }
}
