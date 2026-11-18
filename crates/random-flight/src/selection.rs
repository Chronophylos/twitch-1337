use std::time::Duration;

use rand::{Rng, RngExt};
use twitch_1337_data::flight::{self, Aircraft, Airport};

use crate::error::Error;
use crate::flight_plan::{
    FlightPlan, calculate_flight_plan, estimate_distance_for_block_time, max_block_time,
};
use crate::geo::{bearing_difference, haversine_distance_nm, initial_bearing};

/// Accepted deviation between a generated plan's block time and the target.
const BLOCK_TIME_TOLERANCE: Duration = Duration::from_secs(15 * 60);
const MAX_RETRIES: u32 = 100;

/// Half-width of the accepted heading wedge, matching the 45 degree buckets of
/// [`crate::geo::cardinal_direction`].
pub const DIRECTION_TOLERANCE_DEG: f64 = 22.5;

/// Which way the flight should head.
#[derive(Debug, Clone, Copy)]
pub enum Direction {
    /// A fixed compass heading in degrees.
    Bearing(f64),
    /// Whatever heading points at this coordinate from the departure. Resolved
    /// per attempt, since the departure may be random.
    Towards { lat: f64, lon: f64 },
}

/// What to generate. Unset fields are rolled.
#[derive(Debug, Clone, Copy)]
pub struct FlightRequest<'a> {
    pub aircraft: &'a Aircraft,
    pub block_time: Duration,
    pub departure: Option<&'a str>,
    pub direction: Option<Direction>,
}

pub fn generate_flight_plan(req: &FlightRequest<'_>) -> Result<FlightPlan, Error> {
    let mut rng = rand::rng();
    generate_flight_plan_with_rng(req, &mut rng)
}

pub(crate) fn generate_flight_plan_with_rng(
    req: &FlightRequest<'_>,
    rng: &mut impl Rng,
) -> Result<FlightPlan, Error> {
    let aircraft = req.aircraft;

    // Requests within one tolerance of the ceiling cannot reliably find a
    // route within tolerance, so reject them up front with the honest
    // achievable maximum instead of burning retries (both call sites map
    // this to a user-facing hint).
    let achievable_max = max_block_time(aircraft) - BLOCK_TIME_TOLERANCE;
    if req.block_time > achievable_max {
        return Err(Error::BlockTimeTooLong {
            max: achievable_max,
        });
    }

    let pinned = match req.departure {
        Some(icao) => {
            let Some(airport) = flight::find_by_icao(icao) else {
                return Err(Error::UnknownAirport {
                    icao: icao.to_string(),
                });
            };
            if airport.runway_length_ft < aircraft.min_runway_length_ft {
                return Err(Error::RunwayTooShort {
                    airport_icao: airport.icao.to_string(),
                    required_ft: aircraft.min_runway_length_ft,
                    available_ft: airport.runway_length_ft,
                });
            }
            Some(airport)
        }
        None => None,
    };

    let eligible = flight::filter_by_runway(aircraft.min_runway_length_ft);
    if eligible.is_empty() {
        return Err(Error::NoValidAirports);
    }

    let target_dist = estimate_distance_for_block_time(aircraft, req.block_time);
    let tolerance_hrs = BLOCK_TIME_TOLERANCE.as_secs_f64() / 3600.0;
    let dist_margin = f64::from(aircraft.cruise_speed_ktas) * tolerance_hrs;

    let min_dist = (target_dist - dist_margin).max(1.0);
    let max_dist = target_dist + dist_margin;

    let mut candidates: Vec<&'static Airport> = Vec::new();

    for _attempt in 0..MAX_RETRIES {
        let departure = pinned.unwrap_or_else(|| eligible[rng.random_range(0..eligible.len())]);

        let wanted_bearing = match req.direction {
            None => None,
            Some(Direction::Bearing(b)) => Some(b.rem_euclid(360.0)),
            Some(Direction::Towards { lat, lon }) => {
                if haversine_distance_nm(departure.latitude, departure.longitude, lat, lon) < 1.0 {
                    // Bearing to yourself is undefined; due north would be a lie.
                    if pinned.is_some() {
                        return Err(Error::TargetIsDeparture);
                    }
                    continue;
                }
                Some(initial_bearing(
                    departure.latitude,
                    departure.longitude,
                    lat,
                    lon,
                ))
            }
        };

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
            if d < min_dist || d > max_dist || d > aircraft.range_nm() {
                return false;
            }
            match wanted_bearing {
                None => true,
                Some(wanted) => {
                    let b = initial_bearing(
                        departure.latitude,
                        departure.longitude,
                        a.latitude,
                        a.longitude,
                    );
                    bearing_difference(b, wanted) <= DIRECTION_TOLERANCE_DEG
                }
            }
        }));

        if candidates.is_empty() {
            // A pinned departure gives the same candidate set every attempt, so
            // there is nothing to retry.
            if pinned.is_some() {
                return Err(if req.direction.is_some() {
                    Error::NoRouteInDirection
                } else {
                    Error::NoCandidateArrivals
                });
            }
            continue;
        }

        let arrival = candidates[rng.random_range(0..candidates.len())];
        let fp = calculate_flight_plan(departure, arrival, aircraft);

        if fp.block_time.abs_diff(req.block_time) <= BLOCK_TIME_TOLERANCE {
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

    fn req<'a>(aircraft: &'a Aircraft, hours: u64) -> FlightRequest<'a> {
        FlightRequest {
            aircraft,
            block_time: Duration::from_secs(hours * 3600),
            departure: None,
            direction: None,
        }
    }

    fn plan_bearing(fp: &FlightPlan) -> f64 {
        initial_bearing(
            fp.departure.latitude,
            fp.departure.longitude,
            fp.arrival.latitude,
            fp.arrival.longitude,
        )
    }

    #[test]
    fn generates_plan_within_tolerance() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        let target = Duration::from_secs(2 * 3600);

        let mut rng = seeded_rng();
        let fp =
            generate_flight_plan_with_rng(&req(ac, 2), &mut rng).expect("should find a flight");

        assert!(
            fp.block_time.abs_diff(target) <= BLOCK_TIME_TOLERANCE,
            "block time {} min not within tolerance of target 120 min",
            fp.block_time.as_secs() / 60
        );
        assert!(fp.departure.runway_length_ft >= ac.min_runway_length_ft);
        assert!(fp.arrival.runway_length_ft >= ac.min_runway_length_ft);
    }

    #[test]
    fn pinned_departure_is_respected() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        let fp = generate_flight_plan(&FlightRequest {
            departure: Some("EDDF"),
            ..req(ac, 1)
        })
        .expect("should find a flight from EDDF");
        assert_eq!(fp.departure.icao, "EDDF");
        assert!(fp.block_time.abs_diff(Duration::from_secs(3600)) <= BLOCK_TIME_TOLERANCE);
    }

    #[test]
    fn pinned_departure_rejects_unknown_icao() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        let err = generate_flight_plan(&FlightRequest {
            departure: Some("ZZZZ"),
            ..req(ac, 1)
        })
        .unwrap_err();
        assert!(matches!(err, Error::UnknownAirport { .. }));
    }

    #[test]
    fn pinned_departure_rejects_short_runway() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        let short = flight::airports()
            .iter()
            .find(|a| a.runway_length_ft < ac.min_runway_length_ft)
            .expect("some short runway exists");
        let err = generate_flight_plan(&FlightRequest {
            departure: Some(short.icao),
            ..req(ac, 1)
        })
        .unwrap_err();
        assert!(matches!(err, Error::RunwayTooShort { .. }));
    }

    #[test]
    fn bearing_direction_is_respected() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        let fp = generate_flight_plan(&FlightRequest {
            departure: Some("EDDF"),
            direction: Some(Direction::Bearing(90.0)),
            ..req(ac, 1)
        })
        .expect("should find an eastbound flight from EDDF");
        let diff = bearing_difference(plan_bearing(&fp), 90.0);
        assert!(diff <= DIRECTION_TOLERANCE_DEG, "bearing off by {diff}");
    }

    #[test]
    fn towards_target_is_respected() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        // Berlin Brandenburg, roughly.
        let (lat, lon) = (52.36, 13.50);
        let fp = generate_flight_plan(&FlightRequest {
            departure: Some("EDDF"),
            direction: Some(Direction::Towards { lat, lon }),
            ..req(ac, 1)
        })
        .expect("should find a flight from EDDF towards Berlin");
        let wanted = initial_bearing(fp.departure.latitude, fp.departure.longitude, lat, lon);
        let diff = bearing_difference(plan_bearing(&fp), wanted);
        assert!(diff <= DIRECTION_TOLERANCE_DEG, "bearing off by {diff}");
    }

    #[test]
    fn impossible_direction_fails_loudly() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        // Due north of Honolulu at a 1h block time is open Pacific: nothing
        // sits in the distance band inside the heading wedge.
        let err = generate_flight_plan(&FlightRequest {
            departure: Some("PHNL"),
            direction: Some(Direction::Bearing(0.0)),
            ..req(ac, 1)
        })
        .unwrap_err();
        assert!(
            matches!(err, Error::NoRouteInDirection),
            "expected NoRouteInDirection, got {err:?}"
        );
    }

    #[test]
    fn target_equal_to_pinned_departure_errors() {
        let ac = aircraft_by_icao_type("B738").expect("B738");
        let eddf = flight::find_by_icao("EDDF").expect("EDDF");
        let err = generate_flight_plan(&FlightRequest {
            departure: Some("EDDF"),
            direction: Some(Direction::Towards {
                lat: eddf.latitude,
                lon: eddf.longitude,
            }),
            ..req(ac, 1)
        })
        .unwrap_err();
        assert!(matches!(err, Error::TargetIsDeparture));
    }
    #[test]
    fn a388_finds_ten_hour_plan() {
        // Regression for issue #500: the old proportional distance estimate
        // drifted ~44 min off at 10h, so every retry missed the ±15 min
        // block-time tolerance and !fl A388 10h always failed.
        let ac = aircraft_by_icao_type("A388").expect("A388");
        let target = Duration::from_secs(10 * 3600);

        let fp = generate_flight_plan_with_rng(&req(ac, 10), &mut seeded_rng())
            .expect("A388 should find a 10h route");

        assert!(
            fp.block_time.abs_diff(target) <= BLOCK_TIME_TOLERANCE,
            "block time {} min not within tolerance of 600 min",
            fp.block_time.as_secs() / 60
        );
    }

    #[test]
    fn grid_across_aircraft_and_durations() {
        // Every aircraft must find plans across its feasible duration range —
        // short hops (reduced-altitude regime) and the long end where the old
        // estimate broke down. The skip predicate mirrors the generator's
        // up-front guard exactly, so the tested band and the accepted band
        // are the same thing by construction.
        let durations = [20 * 60, 45 * 60, 2 * 3600, 6 * 3600, 10 * 3600];
        for ac in flight::aircraft() {
            let max = max_block_time(ac);
            for secs in durations {
                let target = Duration::from_secs(secs);
                if target > max - BLOCK_TIME_TOLERANCE {
                    continue; // generator's BlockTimeTooLong band
                }
                let request = FlightRequest {
                    aircraft: ac,
                    block_time: target,
                    departure: None,
                    direction: None,
                };
                let fp = generate_flight_plan_with_rng(&request, &mut seeded_rng()).unwrap_or_else(
                    |e| panic!("{} at {}s should find a route: {e:?}", ac.icao_type, secs),
                );
                assert!(
                    fp.block_time.abs_diff(target) <= BLOCK_TIME_TOLERANCE,
                    "{} at {}s: block time {} min off target",
                    ac.icao_type,
                    secs,
                    fp.block_time.as_secs() / 60
                );
            }
        }
    }

    #[test]
    fn achievable_ceiling_generates_for_every_aircraft() {
        // The advertised achievable maximum must actually be achievable. For
        // the A388 the fuel ceiling exceeds the longest realizable pair on
        // the planet (10,793 nm SCAR-ZJSY among eligible airports), so the
        // bounds clamp to Earth's great-circle extent — without that clamp,
        // the top of the A388's advertised range fails all retries, i.e.
        // issue #500's failure mode surviving at the ceiling.
        for ac in flight::aircraft() {
            let target = max_block_time(ac) - BLOCK_TIME_TOLERANCE;
            let request = FlightRequest {
                aircraft: ac,
                block_time: target,
                departure: None,
                direction: None,
            };
            let fp =
                generate_flight_plan_with_rng(&request, &mut seeded_rng()).unwrap_or_else(|e| {
                    panic!(
                        "{} at {:?} should find a route: {e:?}",
                        ac.icao_type, target
                    )
                });
            assert!(
                fp.block_time.abs_diff(target) <= BLOCK_TIME_TOLERANCE,
                "{} at ceiling: block time {} min off target",
                ac.icao_type,
                fp.block_time.as_secs() / 60
            );
        }
    }
}
