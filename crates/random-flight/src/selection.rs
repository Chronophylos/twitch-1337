use std::time::Duration;

use rand::{Rng, RngExt};
use twitch_1337_data::flight::{self, Aircraft, Airport};

use crate::error::Error;
use crate::flight_plan::{FlightPlan, calculate_flight_plan, estimate_distance_for_block_time};
use crate::geo::haversine_distance_nm;

const TOLERANCE: Duration = Duration::from_secs(15 * 60);
const MAX_RETRIES: u32 = 100;

pub fn generate_flight_plan(
    aircraft: &Aircraft,
    target_block_time: Duration,
) -> Result<FlightPlan, Error> {
    let mut rng = rand::rng();
    generate_flight_plan_with_rng(None, aircraft, target_block_time, &mut rng)
}

/// Same as [`generate_flight_plan`], but departing from a fixed airport.
pub fn generate_flight_plan_from(
    departure_icao: &str,
    aircraft: &Aircraft,
    target_block_time: Duration,
) -> Result<FlightPlan, Error> {
    let Some(departure) = flight::find_by_icao(departure_icao) else {
        return Err(Error::UnknownAirport {
            icao: departure_icao.to_string(),
        });
    };
    if departure.runway_length_ft < aircraft.min_runway_length_ft {
        return Err(Error::RunwayTooShort {
            airport_icao: departure.icao.to_string(),
            required_ft: aircraft.min_runway_length_ft,
            available_ft: departure.runway_length_ft,
        });
    }
    let mut rng = rand::rng();
    generate_flight_plan_with_rng(Some(departure), aircraft, target_block_time, &mut rng)
}

pub(crate) fn generate_flight_plan_with_rng(
    pinned_departure: Option<&'static Airport>,
    aircraft: &Aircraft,
    target_block_time: Duration,
    rng: &mut impl Rng,
) -> Result<FlightPlan, Error> {
    let eligible = flight::filter_by_runway(aircraft.min_runway_length_ft);
    if eligible.is_empty() {
        return Err(Error::NoValidAirports);
    }

    let target_dist = estimate_distance_for_block_time(aircraft, target_block_time);
    let tolerance_hrs = TOLERANCE.as_secs_f64() / 3600.0;
    let dist_margin = f64::from(aircraft.cruise_speed_ktas) * tolerance_hrs;

    let min_dist = (target_dist - dist_margin).max(1.0);
    let max_dist = target_dist + dist_margin;

    let mut candidates: Vec<&'static Airport> = Vec::new();

    for _attempt in 0..MAX_RETRIES {
        let departure =
            pinned_departure.unwrap_or_else(|| eligible[rng.random_range(0..eligible.len())]);

        candidates.clear();
        candidates.extend(eligible.iter().copied().filter(|a| {
            if std::ptr::eq(*a, departure) {
                return false;
            }
            let d = haversine_distance_nm(
                departure.latitude,
                departure.longitude,
                a.latitude,
                a.longitude,
            );
            d >= min_dist && d <= max_dist && d <= aircraft.range_nm()
        }));

        if candidates.is_empty() {
            if pinned_departure.is_some() {
                return Err(Error::NoCandidateArrivals);
            }
            continue;
        }

        let arrival = candidates[rng.random_range(0..candidates.len())];
        let fp = calculate_flight_plan(departure, arrival, aircraft);

        if fp.block_time.abs_diff(target_block_time) <= TOLERANCE {
            return Ok(fp);
        }
    }

    Err(Error::RetriesExhausted {
        attempts: MAX_RETRIES,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::SmallRng;
    use twitch_1337_data::flight::aircraft_by_icao_type;

    fn seeded_rng() -> SmallRng {
        SmallRng::seed_from_u64(42)
    }

    #[test]
    fn generates_plan_within_tolerance() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        let target = Duration::from_secs(2 * 3600);

        let mut rng = seeded_rng();
        let fp = generate_flight_plan_with_rng(None, ac, target, &mut rng)
            .expect("should find a flight");

        assert!(
            fp.block_time.abs_diff(target) <= Duration::from_secs(15 * 60),
            "block time {} min not within tolerance of target 120 min",
            fp.block_time.as_secs() / 60
        );
    }

    #[test]
    fn pinned_departure_is_respected() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        let fp = generate_flight_plan_from("EDDF", ac, Duration::from_secs(3600))
            .expect("should find a flight from EDDF");
        assert_eq!(fp.departure.icao, "EDDF");
        assert!(fp.block_time.abs_diff(Duration::from_secs(3600)) <= TOLERANCE);
    }

    #[test]
    fn pinned_departure_rejects_unknown_icao() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        let err = generate_flight_plan_from("ZZZZ", ac, Duration::from_secs(3600)).unwrap_err();
        assert!(matches!(err, Error::UnknownAirport { .. }));
    }

    #[test]
    fn pinned_departure_rejects_short_runway() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        let short = flight::airports()
            .iter()
            .find(|a| a.runway_length_ft < ac.min_runway_length_ft)
            .expect("some short runway exists");
        let err = generate_flight_plan_from(short.icao, ac, Duration::from_secs(3600)).unwrap_err();
        assert!(matches!(err, Error::RunwayTooShort { .. }));
    }
}
