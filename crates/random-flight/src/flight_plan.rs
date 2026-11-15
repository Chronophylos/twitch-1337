use std::time::Duration;

use twitch_1337_data::flight::{Aircraft, Airport, AirportSize};

use crate::error::Error;
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

/// Phase breakdown of a flight of a given distance, before taxi is added.
struct Phases {
    climb_time: Duration,
    climb_dist: f64,
    descent_time: Duration,
    descent_dist: f64,
    cruise_time: Duration,
    cruise_dist: f64,
    cruise_alt: u32,
}

/// Block-time model shared by [`calculate_flight_plan`] (real airports) and
/// the distance estimate / feasibility bounds (nominal profile). Cruise
/// altitude is reduced in 1000 ft steps until climb + descent fit within the
/// distance, mirroring real short-hop profiles.
fn phases_for_distance(
    aircraft: &Aircraft,
    distance_nm: f64,
    dep_elevation_ft: i32,
    arr_elevation_ft: i32,
) -> Phases {
    let dep_elev = dep_elevation_ft.max(0) as u32;
    let arr_elev = arr_elevation_ft.max(0) as u32;
    let cruise_speed = f64::from(aircraft.cruise_speed_ktas);
    let climb_speed = f64::from(aircraft.climb_speed_ktas);
    let descent_speed = f64::from(aircraft.descent_speed_ktas);

    let mut cruise_alt = aircraft.cruise_altitude_ft;
    let min_alt = dep_elev.max(arr_elev) + 1000;

    loop {
        let climb_ft = f64::from(cruise_alt.saturating_sub(dep_elev));
        let descent_ft = f64::from(cruise_alt.saturating_sub(arr_elev));

        let climb_time_hrs = climb_ft / f64::from(aircraft.climb_rate_fpm) / 60.0;
        let descent_time_hrs = descent_ft / f64::from(aircraft.descent_rate_fpm) / 60.0;

        let climb_dist = climb_speed * climb_time_hrs;
        let descent_dist = descent_speed * descent_time_hrs;

        if climb_dist + descent_dist < distance_nm || cruise_alt <= min_alt {
            let cruise_dist = (distance_nm - climb_dist - descent_dist).max(0.0);
            let cruise_time_hrs = cruise_dist / cruise_speed;

            let to_duration = |hrs: f64| Duration::from_secs_f64(hrs * 3600.0);

            return Phases {
                climb_time: to_duration(climb_time_hrs),
                climb_dist,
                descent_time: to_duration(descent_time_hrs),
                descent_dist,
                cruise_time: to_duration(cruise_time_hrs),
                cruise_dist,
                cruise_alt,
            };
        }

        // Reduce altitude and retry
        cruise_alt = cruise_alt.saturating_sub(1000).max(min_alt);
    }
}

fn nominal_taxi() -> Duration {
    taxi_out_for(AirportSize::Medium) + taxi_in_for(AirportSize::Medium)
}

/// Half the great-circle circumference of the Earth. No airport pair is
/// farther apart, so the fuel-limited ceiling must never be computed from a
/// longer distance: the A388's fuel range exceeds this, and without the clamp
/// requests just under its ceiling would pass the bounds check and then fail
/// all retries (issue #500's failure mode, surviving at the top of the range).
const EARTH_MAX_GC_NM: f64 = 10_800.0;

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

    let p = phases_for_distance(
        aircraft,
        distance_nm,
        departure.elevation_ft,
        arrival.elevation_ft,
    );
    let taxi_out = taxi_out_for(departure.size);
    let taxi_in = taxi_in_for(arrival.size);
    let block_time = p.climb_time + p.descent_time + p.cruise_time + taxi_out + taxi_in;

    FlightPlan {
        departure,
        arrival,
        aircraft: aircraft.clone(),
        distance_nm,
        block_time,
        taxi_out,
        taxi_in,
        cruise_altitude_ft: p.cruise_alt,
        climb_time: p.climb_time,
        climb_distance_nm: p.climb_dist,
        descent_time: p.descent_time,
        descent_distance_nm: p.descent_dist,
        cruise_time: p.cruise_time,
        cruise_distance_nm: p.cruise_dist,
    }
}

/// Plan a fixed departure → arrival route, rejecting what the aircraft can't
/// fly: the same airport twice, a runway too short at either end, or a
/// distance past its fuel range.
pub fn plan_route(
    departure: &'static Airport,
    arrival: &'static Airport,
    aircraft: &Aircraft,
) -> Result<FlightPlan, Error> {
    if std::ptr::eq(departure, arrival) {
        return Err(Error::TargetIsDeparture);
    }
    for airport in [departure, arrival] {
        if airport.runway_length_ft < aircraft.min_runway_length_ft {
            return Err(Error::RunwayTooShort {
                airport_icao: airport.icao.to_string(),
                required_ft: aircraft.min_runway_length_ft,
                available_ft: airport.runway_length_ft,
            });
        }
    }
    let distance_nm = haversine_distance_nm(
        departure.latitude,
        departure.longitude,
        arrival.latitude,
        arrival.longitude,
    );
    if distance_nm > aircraft.range_nm() {
        return Err(Error::RangeExceeded {
            distance_nm,
            range_nm: aircraft.range_nm(),
        });
    }
    Ok(calculate_flight_plan(departure, arrival, aircraft))
}

/// Block time of the nominal profile (sea-level airports, medium taxi) at a
/// given distance. Monotone non-decreasing in distance: at the
/// altitude-reduction boundaries the function jumps *up* (climb and descent
/// are slower than cruise, so each 1000 ft step adds more phase time than it
/// removes cruise time) by at most about 70 s on the slowest airframes —
/// which is exactly what keeps the bisection below valid, and is far below
/// the block-time tolerance.
fn nominal_block_time(aircraft: &Aircraft, distance_nm: f64) -> Duration {
    let p = phases_for_distance(aircraft, distance_nm, 0, 0);
    p.climb_time + p.descent_time + p.cruise_time + nominal_taxi()
}

/// Estimate the total flight distance for a target block time.
/// Used by the selection algorithm to narrow the airport search band.
///
/// Numerical inverse (bisection) of [`nominal_block_time`]. Exact for the
/// nominal profile in both regimes — full-altitude cruise and
/// reduced-altitude short hops, up to the ≤ 70 s step gaps — where the
/// previous `0.9 × speed` approximation's error grew linearly and made long
/// requests deterministically miss the block-time tolerance regardless of
/// retry count. Residual error against real plans is bounded by the profile
/// variance that airport elevations and taxi sizes introduce.
pub fn estimate_distance_for_block_time(aircraft: &Aircraft, target_block_time: Duration) -> f64 {
    // Nothing findable beyond the smaller of the aircraft's fuel range and
    // the planet's own extent; targets that large fail via the block-time
    // tolerance the same way as any other impossible request.
    let mut lo = 1.0_f64;
    let mut hi = aircraft.range_nm().clamp(2.0, EARTH_MAX_GC_NM);
    if nominal_block_time(aircraft, hi) < target_block_time {
        return hi;
    }

    for _ in 0..40 {
        let mid = (lo + hi) / 2.0;
        if nominal_block_time(aircraft, mid) < target_block_time {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo + hi) / 2.0
}

/// Longest sustainable block time: climb and descent plus cruising until the
/// usable fuel (the same 95% reserve [`Aircraft::range_nm`] assumes) is
/// spent, with maximum taxi on both ends. Requests above this can never
/// produce a plan. The distance is clamped to [`EARTH_MAX_GC_NM`]: fuel can
/// outlast the planet for long-range types, and no route can cover more.
pub fn max_block_time(aircraft: &Aircraft) -> Duration {
    let p = phases_for_distance(aircraft, aircraft.range_nm().min(EARTH_MAX_GC_NM), 0, 0);
    p.climb_time
        + p.descent_time
        + p.cruise_time
        + taxi_out_for(AirportSize::Large)
        + taxi_in_for(AirportSize::Large)
}

#[cfg(test)]
mod tests {
    use super::*;
    use twitch_1337_data::flight::{aircraft_by_icao_type, find_by_icao};

    #[test]
    fn plan_route_flies_the_given_pair() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        let (eddf, egll) = (find_by_icao("EDDF").unwrap(), find_by_icao("EGLL").unwrap());
        let fp = plan_route(eddf, egll, ac).expect("EDDF-EGLL in a B738");
        assert_eq!((fp.departure.icao, fp.arrival.icao), ("EDDF", "EGLL"));
    }

    #[test]
    fn plan_route_rejects_what_the_aircraft_cannot_fly() {
        let b738 = aircraft_by_icao_type("B738").expect("B738");
        let eddf = find_by_icao("EDDF").expect("EDDF");
        let yssy = find_by_icao("YSSY").expect("YSSY");
        assert!(matches!(
            plan_route(eddf, yssy, b738),
            Err(Error::RangeExceeded { .. })
        ));
        assert!(matches!(
            plan_route(eddf, eddf, b738),
            Err(Error::TargetIsDeparture)
        ));
        // Whatever the DB's shortest strip is, a 737 can't use it.
        let short = twitch_1337_data::flight::airports()
            .iter()
            .min_by_key(|a| a.runway_length_ft)
            .expect("airports");
        assert!(short.runway_length_ft < b738.min_runway_length_ft);
        assert!(matches!(
            plan_route(eddf, short, b738),
            Err(Error::RunwayTooShort { .. })
        ));
    }

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
    fn f27_simbrief_url_eddf_eham() {
        let dep = find_by_icao("EDDF").expect("EDDF");
        let arr = find_by_icao("EHAM").expect("EHAM");
        let ac = aircraft_by_icao_type("F27").expect("F27");

        let fp = calculate_flight_plan(dep, arr, ac);
        let url = fp.simbrief_url();

        assert!(url.contains("orig=EDDF"), "missing orig: {url}");
        assert!(url.contains("dest=EHAM"), "missing dest: {url}");
        assert!(url.contains("type=F27"), "missing type: {url}");
        assert!(
            url.contains(&format!("fl={}", fp.cruise_altitude_ft)),
            "missing or wrong fl: {url}"
        );
        eprintln!(
            "F27 EDDF-EHAM: {:.0} nm | FL{} | {url}",
            fp.distance_nm,
            fp.cruise_altitude_ft / 100
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

        // ~1h cruise at 460 kts + climb/descent distance ≈ 690 nm
        assert!(est > 500.0 && est < 900.0, "estimate was {est} nm");
    }

    #[test]
    fn estimate_inverts_block_time_model() {
        // The estimate must invert calculate_flight_plan's block-time model:
        // feed it a plan's actual block time and the reproduced distance must
        // map back to that block time within the selection tolerance. Residual
        // error comes from the nominal profile (airport elevation and taxi
        // size are unknown before selection; taxi varies by ±12 min between
        // small and large), so assert in the time domain, not distance.
        // High-elevation airports push the real block a few more minutes
        // below the nominal target; production absorbs that by re-rolling
        // departures across the 100 retries.
        let cases = [
            ("KJFK", "KLAX", "B738"),
            ("EDDF", "EGLL", "B738"),
            ("EDDF", "EDDM", "B738"),
            ("EDDF", "EDDM", "C172"),
            ("EDDF", "EHAM", "F27"),
            ("KJFK", "EGLL", "A388"),
        ];
        for (dep_icao, arr_icao, ac_icao) in cases {
            let dep = find_by_icao(dep_icao).expect(dep_icao);
            let arr = find_by_icao(arr_icao).expect(arr_icao);
            let ac = aircraft_by_icao_type(ac_icao).expect(ac_icao);

            let fp = calculate_flight_plan(dep, arr, ac);
            let est = estimate_distance_for_block_time(ac, fp.block_time);

            let err_min = (est - fp.distance_nm).abs() / f64::from(ac.cruise_speed_ktas) * 60.0;
            assert!(
                err_min <= 15.0,
                "{dep_icao}-{arr_icao} {ac_icao}: estimate {est:.0} nm vs actual {:.0} nm \
                 ({err_min:.1} min of block time off)",
                fp.distance_nm
            );
        }
    }

    #[test]
    fn realistic_block_times_within_ceiling() {
        let cases = [
            ("KJFK", "KLAX", "B738"),
            ("EDDF", "EDDM", "C172"),
            ("KJFK", "EGLL", "A388"),
        ];
        for (dep_icao, arr_icao, ac_icao) in cases {
            let dep = find_by_icao(dep_icao).expect(dep_icao);
            let arr = find_by_icao(arr_icao).expect(arr_icao);
            let ac = aircraft_by_icao_type(ac_icao).expect(ac_icao);

            let fp = calculate_flight_plan(dep, arr, ac);
            let max = max_block_time(ac);

            assert!(
                fp.block_time <= max,
                "{ac_icao}: actual block {:?} above ceiling {:?}",
                fp.block_time,
                max
            );
        }
    }

    #[test]
    fn a388_max_block_reflects_fuel_range() {
        let ac = aircraft_by_icao_type("A388").expect("A388");
        let max = max_block_time(ac);

        // With the Earth-extent clamp: usable range is the lesser of the
        // ~11,582 nm fuel range and the 10,800 nm great-circle maximum,
        // i.e. ~23.6h of cruise once climb/descent distance comes out;
        // fixed phases and Large/Large taxi add roughly half an hour on top.
        assert!(
            max > Duration::from_secs(23 * 3600) && max < Duration::from_secs(25 * 3600),
            "A388 max block was {max:?}"
        );
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
