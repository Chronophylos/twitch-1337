//! Divert Watch: closure-based early diversion warning.
//!
//! A heading check (ground track vs. bearing to the destination) cannot tell
//! a diversion from a downwind leg, a holding pattern or radar vectors — all of
//! them point away from the airport for minutes at a time. The Divert Watch
//! instead tracks the closest distance to the destination seen so far and only
//! alerts when the aircraft has moved well back out of it, outside the
//! terminal area, for a sustained time. Normal approach manoeuvres stay within
//! a few tens of nm of the airport and never qualify.
//!
//! The alert fires once and re-arms only after the aircraft closes back in
//! (hysteresis between [`DIVERT_AWAY_MARGIN_NM`] and
//! [`DIVERT_REARM_MARGIN_NM`]). The definitive answer comes at touchdown:
//! [`landed_elsewhere`] compares the landing point against the destination.
//!
//! State is deliberately not persisted: after a restart the closest distance
//! starts at the current one, so a restart can never cause an alert.

use jiff::{SignedDuration, Timestamp};
use random_flight::geo::haversine_distance_nm;

use super::{FlightPhase, TrackedFlight};
use crate::aviation::nearest_airport;

/// Inside this distance of the destination no early alert fires; go-arounds
/// and diversions starting here are left to the landing message.
pub(crate) const DIVERT_TERMINAL_RADIUS_NM: f64 = 40.0;
/// Short hops that never report Cruise arm once this far from where the
/// aircraft was first seen airborne, clearing departure procedures that head
/// away from the destination.
pub(crate) const DIVERT_ARM_DISTANCE_NM: f64 = 40.0;
/// How far beyond the closest distance seen the aircraft must be to count as
/// moving away.
pub(crate) const DIVERT_AWAY_MARGIN_NM: f64 = 30.0;
/// After an alert, the watch re-arms once the aircraft is back within this
/// margin of its closest distance.
pub(crate) const DIVERT_REARM_MARGIN_NM: f64 = 10.0;
/// How long the aircraft must keep moving away before the alert fires.
pub(crate) const DIVERT_SUSTAIN: SignedDuration = SignedDuration::from_mins(5);
/// A touchdown within this distance of the destination counts as landing
/// there (multi-runway airports, coordinate offsets).
pub(crate) const LANDED_AT_DEST_RADIUS_NM: f64 = 5.0;

/// Per-flight Divert Watch state. Reset on the ground and on restart.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DivertWatch {
    /// First airborne position seen; arms short hops that skip Cruise.
    origin_fix: Option<(f64, f64)>,
    armed: bool,
    min_distance_nm: Option<f64>,
    away_since: Option<Timestamp>,
    alerted: bool,
}

/// What one Divert Watch observation decided, for the decision log.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DivertStep {
    pub distance_nm: f64,
    pub min_distance_nm: Option<f64>,
    pub armed: bool,
    pub away_secs: Option<i64>,
    pub alerted: bool,
    pub rearmed: bool,
    pub alert_emitted: bool,
}

impl DivertWatch {
    /// Feed one confirmed airborne sample; the step says whether the alert fired.
    pub(crate) fn observe(
        &mut self,
        phase: FlightPhase,
        position: (f64, f64),
        destination: (f64, f64),
        now: Timestamp,
    ) -> DivertStep {
        let distance_nm =
            haversine_distance_nm(position.0, position.1, destination.0, destination.1);

        if !self.armed {
            let fix = *self.origin_fix.get_or_insert(position);
            self.armed = phase == FlightPhase::Cruise
                || haversine_distance_nm(fix.0, fix.1, position.0, position.1)
                    >= DIVERT_ARM_DISTANCE_NM;
        }

        let mut step = DivertStep {
            distance_nm,
            min_distance_nm: None,
            armed: self.armed,
            away_secs: None,
            alerted: self.alerted,
            rearmed: false,
            alert_emitted: false,
        };
        if !self.armed {
            return step;
        }

        let min = self
            .min_distance_nm
            .map_or(distance_nm, |m| m.min(distance_nm));
        self.min_distance_nm = Some(min);
        step.min_distance_nm = Some(min);

        if self.alerted {
            self.away_since = None;
            if distance_nm <= min + DIVERT_REARM_MARGIN_NM {
                self.alerted = false;
                step.rearmed = true;
            }
        } else if distance_nm <= DIVERT_TERMINAL_RADIUS_NM
            || distance_nm <= min + DIVERT_AWAY_MARGIN_NM
        {
            self.away_since = None;
        } else {
            let since = *self.away_since.get_or_insert(now);
            let away = now.duration_since(since);
            step.away_secs = Some(away.as_secs());
            if away >= DIVERT_SUSTAIN {
                self.alerted = true;
                self.away_since = None;
                step.alert_emitted = true;
            }
        }
        step.alerted = self.alerted;
        step
    }
}

/// If the flight touched down away from its destination, the label of the
/// nearest airport (`"Name (CODE)"`). `None` when it landed at the
/// destination or the destination or position is unknown.
pub(crate) fn landed_elsewhere(flight: &TrackedFlight) -> Option<String> {
    let (dest_lat, dest_lon) = (flight.dest_lat?, flight.dest_lon?);
    let (lat, lon) = (flight.lat?, flight.lon?);
    if haversine_distance_nm(lat, lon, dest_lat, dest_lon) <= LANDED_AT_DEST_RADIUS_NM {
        return None;
    }
    nearest_airport(lat, lon).map(|apt| format!("{} ({})", apt.name, apt.code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aviation::tracker::test_support::dt;

    /// Munich.
    const DEST: (f64, f64) = (48.354, 11.786);

    /// Point `nm` nautical miles due north of the destination.
    fn north_of_dest(nm: f64) -> (f64, f64) {
        (DEST.0 + nm / 60.0, DEST.1)
    }

    fn at(secs: i64) -> Timestamp {
        dt("2026-04-18T12:00:00Z") + SignedDuration::from_secs(secs)
    }

    fn armed_watch_at(nm: f64) -> DivertWatch {
        let mut watch = DivertWatch::default();
        let step = watch.observe(FlightPhase::Cruise, north_of_dest(nm), DEST, at(0));
        assert!(step.armed);
        watch
    }

    #[test]
    fn downwind_leg_inside_terminal_area_never_alerts() {
        // Arrive from the north, overfly to the south side, fly a 10-minute
        // downwind leg heading away, turn back in. Max ~25 nm from the field.
        let mut watch = armed_watch_at(120.0);
        let mut t = 0;
        for nm in [90.0, 60.0, 40.0, 20.0, 5.0] {
            t += 60;
            assert!(
                !watch
                    .observe(FlightPhase::Descent, north_of_dest(nm), DEST, at(t))
                    .alert_emitted
            );
        }
        for i in 0..20 {
            t += 30;
            let nm = -5.0 - f64::from(i);
            let step = watch.observe(FlightPhase::Approach, north_of_dest(nm), DEST, at(t));
            assert!(!step.alert_emitted, "false divert on downwind at {nm} nm");
        }
    }

    #[test]
    fn holding_pattern_never_alerts() {
        // Racetrack 45..60 nm out, 30 laps' worth of samples.
        let mut watch = armed_watch_at(100.0);
        for i in 0..120 {
            let nm = if i % 8 < 4 { 45.0 } else { 60.0 };
            let step = watch.observe(
                FlightPhase::Descent,
                north_of_dest(nm),
                DEST,
                at(30 * (i + 1)),
            );
            assert!(!step.alert_emitted, "false divert in hold at poll {i}");
        }
    }

    #[test]
    fn sustained_retreat_alerts_once_then_rearms_on_closing() {
        let mut watch = armed_watch_at(100.0);
        // Closest point 100 nm; retreat beyond 130 nm.
        let mut emitted = 0;
        for i in 1..=20 {
            let step = watch.observe(
                FlightPhase::Cruise,
                north_of_dest(140.0 + f64::from(i)),
                DEST,
                at(60 * i64::from(i)),
            );
            if step.alert_emitted {
                emitted += 1;
                assert!(i >= 6, "alert before sustain elapsed (poll {i})");
            }
        }
        assert_eq!(emitted, 1, "divert alert must fire exactly once");

        // Back within the re-arm margin, then away again → second alert.
        let step = watch.observe(FlightPhase::Cruise, north_of_dest(105.0), DEST, at(2000));
        assert!(step.rearmed);
        let mut second = false;
        for i in 1..=10 {
            let step = watch.observe(
                FlightPhase::Cruise,
                north_of_dest(150.0),
                DEST,
                at(2000 + 60 * i),
            );
            second |= step.alert_emitted;
        }
        assert!(second, "re-armed watch must alert on a second retreat");
    }

    #[test]
    fn brief_retreat_resets_sustain_timer() {
        let mut watch = armed_watch_at(100.0);
        for i in 1..=4 {
            watch.observe(FlightPhase::Cruise, north_of_dest(140.0), DEST, at(60 * i));
        }
        // Back inside the margin before 5 minutes elapse.
        watch.observe(FlightPhase::Cruise, north_of_dest(110.0), DEST, at(300));
        for i in 1..=4 {
            let step = watch.observe(
                FlightPhase::Cruise,
                north_of_dest(140.0),
                DEST,
                at(300 + 60 * i),
            );
            assert!(!step.alert_emitted);
        }
    }

    #[test]
    fn departure_heading_away_does_not_alert_before_arming() {
        // Depart 100 nm north of dest, climb out northbound for 10 minutes
        // but stay within the 40 nm arm distance of the first fix.
        let mut watch = DivertWatch::default();
        for i in 0..20 {
            let nm = 100.0 + f64::from(i) * 1.5;
            let step = watch.observe(
                FlightPhase::Climb,
                north_of_dest(nm),
                DEST,
                at(30 * i64::from(i)),
            );
            assert!(!step.armed);
            assert!(!step.alert_emitted);
        }
    }

    #[test]
    fn short_hop_arms_after_leaving_origin_area() {
        let mut watch = DivertWatch::default();
        assert!(
            !watch
                .observe(FlightPhase::Climb, north_of_dest(100.0), DEST, at(0))
                .armed
        );
        assert!(
            watch
                .observe(FlightPhase::Climb, north_of_dest(55.0), DEST, at(300))
                .armed
        );
    }

    #[test]
    fn landed_elsewhere_respects_destination_radius() {
        let mut f = crate::aviation::tracker::test_support::tracked_flight();
        f.dest_lat = Some(DEST.0);
        f.dest_lon = Some(DEST.1);
        f.lat = Some(DEST.0 + 0.03); // ~1.8 nm
        f.lon = Some(DEST.1);
        assert_eq!(landed_elsewhere(&f), None);

        // Nuremberg airport.
        f.lat = Some(49.4987);
        f.lon = Some(11.0669);
        let label = landed_elsewhere(&f).expect("landed away from MUC");
        assert!(label.contains("NUE"), "unexpected label {label}");

        f.dest_lat = None;
        assert_eq!(landed_elsewhere(&f), None);
    }

    #[test]
    fn legacy_flights_ron_with_divert_counter_still_loads() {
        let flight = crate::aviation::tracker::test_support::tracked_flight();
        let ron = ron::to_string(&flight).expect("serialize");
        assert!(
            !ron.contains("divert"),
            "Divert Watch must not be persisted"
        );
        let legacy = ron.replacen('(', "(divert_consecutive_polls:3,", 1);
        let loaded: TrackedFlight = ron::from_str(&legacy).expect("legacy field ignored");
        assert_eq!(loaded.divert, DivertWatch::default());
    }
}
