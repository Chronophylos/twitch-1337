//! Poll Decision: pure outcome for one Tracked Flight given poll readiness
//! and, when Due, an Observation — either Pending Expired or Advance.
//!
//! See CONTEXT.md "Poll Decision". No I/O. Sibling of Advance (Observation-only).

use chrono::{DateTime, Utc};
use tracing::error;

use super::TrackedFlight;
use super::advance::{Emit, FlightUpdate, Observation, RemovalReason, advance_flight};
use super::debug_journal::FlightTrackerDebugEvent;
use super::schedule::PollReadiness;

/// First-pass plan for the poll sweep. Encodes Skip / expire effects / go fetch.
#[derive(Debug)]
pub(crate) enum PollPlan {
    /// `NotDue` — skip this Tracked Flight for this sweep.
    Skip,
    /// `Expired` — Pending Expired effects already decided. No ADS-B.
    Apply(FlightUpdate),
    /// `Due` — caller must build an Observation, then call [`decide_poll`].
    Due,
}

/// Common-caller entry: readiness → plan. Does not call `poll_readiness`.
///
/// `Due` never requires an Observation here — that is the fetch signal.
/// I1 (Due ⇒ Observation) applies only to [`decide_poll`].
pub(crate) fn poll_plan(
    flight: &TrackedFlight,
    readiness: PollReadiness,
    now: DateTime<Utc>,
) -> PollPlan {
    match readiness {
        PollReadiness::NotDue(_) => PollPlan::Skip,
        PollReadiness::Expired => PollPlan::Apply(pending_expired_update(flight, now)),
        PollReadiness::Due => PollPlan::Due,
    }
}

/// Full Poll Decision. Mutates the Tracked Flight only on the Due → Advance path.
///
/// | readiness | observation | result |
/// |---|---|---|
/// | `NotDue` | ignored | `None` |
/// | `Expired` | ignored | `Some` Pending Expired effects |
/// | `Due` | `Some(obs)` | `Some(advance_flight(...))` |
/// | `Due` | `None` | I1 precondition — `debug_assert`; release: log + `None` |
pub(crate) fn decide_poll(
    flight: &mut TrackedFlight,
    readiness: PollReadiness,
    observation: Option<&Observation>,
    now: DateTime<Utc>,
) -> Option<FlightUpdate> {
    match readiness {
        PollReadiness::NotDue(_) => None,
        PollReadiness::Expired => Some(pending_expired_update(flight, now)),
        PollReadiness::Due => match observation {
            Some(obs) => Some(advance_flight(flight, obs, now)),
            None => {
                debug_assert!(
                    false,
                    "Poll Decision Due requires an Observation (caller must fetch first)"
                );
                error!(
                    identifier = %flight.identifier,
                    "Poll Decision Due without Observation; skipping Advance"
                );
                None
            }
        },
    }
}

fn pending_expired_update(flight: &TrackedFlight, _now: DateTime<Utc>) -> FlightUpdate {
    let mut update = FlightUpdate::default();
    update.debug.push(FlightTrackerDebugEvent::tracking_removal(
        flight,
        "pending_expired",
        None,
    ));
    update.emits.push(Emit::PendingExpired);
    update.removal = Some(RemovalReason::PendingExpired);
    update
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aviation::tracker::advance::{Observation, PollOutcome};
    use crate::aviation::tracker::test_support::dt;
    use crate::aviation::tracker::{
        FlightIdentifier, FlightPhase, TargetConfirmation, TrackedFlight,
    };

    fn pending_flight(tracked_at: DateTime<Utc>) -> TrackedFlight {
        TrackedFlight {
            identifier: FlightIdentifier::Callsign("DLH1234".to_string()),
            callsign: Some("DLH1234".to_string()),
            alias_callsigns: vec!["DLH1234".to_string()],
            hex: None,
            hex_source: None,
            observed_callsign: None,
            target_confirmation: TargetConfirmation::Pending,
            phase: FlightPhase::Unknown,
            route: None,
            aircraft_type: None,
            altitude_ft: None,
            vertical_rate_fpm: None,
            ground_speed_kts: None,
            lat: None,
            lon: None,
            squawk: None,
            tracked_by: "alice".to_string(),
            tracked_at,
            last_seen: None,
            last_visible_at: None,
            last_phase_change: None,
            polls_since_change: 0,
            takeoff_at: None,
            aviationstack_checked: false,
            scheduled_departure_at: None,
            last_adsb_poll_at: None,
            divert: Default::default(),
            dest_lat: None,
            dest_lon: None,
        }
    }

    #[test]
    fn poll_plan_not_due_skips() {
        let flight = pending_flight(dt("2026-04-18T12:00:00Z"));
        let now = dt("2026-04-18T12:00:00Z");
        match poll_plan(
            &flight,
            PollReadiness::NotDue(dt("2026-04-18T12:01:00Z")),
            now,
        ) {
            PollPlan::Skip => {}
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn poll_plan_expired_applies_pending_expired() {
        let flight = pending_flight(dt("2026-04-18T00:00:00Z"));
        let now = dt("2026-04-19T00:00:00Z");
        match poll_plan(&flight, PollReadiness::Expired, now) {
            PollPlan::Apply(update) => {
                assert_eq!(update.emits, vec![Emit::PendingExpired]);
                assert_eq!(update.removal, Some(RemovalReason::PendingExpired));
                assert_eq!(update.followups.len(), 0);
                assert_eq!(update.debug.len(), 1);
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn poll_plan_due_signals_fetch() {
        let flight = pending_flight(dt("2026-04-18T12:00:00Z"));
        match poll_plan(&flight, PollReadiness::Due, dt("2026-04-18T12:00:00Z")) {
            PollPlan::Due => {}
            other => panic!("expected Due, got {other:?}"),
        }
    }

    #[test]
    fn decide_poll_expired_matches_poll_plan_apply() {
        let flight = pending_flight(dt("2026-04-18T00:00:00Z"));
        let now = dt("2026-04-19T00:00:00Z");
        let from_plan = match poll_plan(&flight, PollReadiness::Expired, now) {
            PollPlan::Apply(u) => u,
            other => panic!("expected Apply, got {other:?}"),
        };
        let mut flight2 = pending_flight(dt("2026-04-18T00:00:00Z"));
        let from_decide = decide_poll(&mut flight2, PollReadiness::Expired, None, now)
            .expect("Expired yields Some");
        assert_eq!(from_plan.emits, from_decide.emits);
        assert_eq!(from_plan.removal, from_decide.removal);
        assert_eq!(from_plan.debug.len(), from_decide.debug.len());
        // Expired must not mutate the Tracked Flight body.
        assert_eq!(flight2.last_seen, None);
        assert_eq!(flight2.target_confirmation, TargetConfirmation::Pending);
    }

    #[test]
    fn decide_poll_due_delegates_to_advance() {
        let mut flight = pending_flight(dt("2026-04-18T12:00:00Z"));
        flight.hex = Some("3C6589".to_string());
        let obs = Observation {
            used_hex: true,
            outcome: PollOutcome::Miss,
        };
        let now = dt("2026-04-18T12:00:00Z");
        let update = decide_poll(&mut flight, PollReadiness::Due, Some(&obs), now)
            .expect("Due + Observation yields Some");
        // Miss on pending with no last_visible_at does not remove; Advance ran.
        assert!(update.removal.is_none());
        assert_eq!(flight.polls_since_change, 1);
    }

    #[test]
    fn decide_poll_not_due_returns_none() {
        let mut flight = pending_flight(dt("2026-04-18T12:00:00Z"));
        assert!(
            decide_poll(
                &mut flight,
                PollReadiness::NotDue(dt("2026-04-18T12:01:00Z")),
                None,
                dt("2026-04-18T12:00:00Z"),
            )
            .is_none()
        );
    }
}
