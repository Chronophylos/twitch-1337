use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::mpsc;
use tokio::task::JoinError;
use tracing::{info, warn};
use twitch_irc::{login::LoginCredentials, message::PrivmsgMessage, transport::Transport};

#[cfg(test)]
use crate::aviation::NearbyAircraft;
use crate::aviation::{AdsbProvider, AviationClient, AviationstackFlightMetadata};
use crate::lifecycle::{Lifecycle, TaskCompletion, TaskGroup};
use crate::twitch::ChatSender;
use crate::util::clock::Clock;

#[cfg(test)]
use super::metadata::aircraft_callsign;
use super::{
    CachedFlightInfo, FlightIdentifier, FlightPhase, FlightTrackerState, HexSource,
    TargetConfirmation, TrackedFlight, TrackerCommand, build_flight_view,
    debug_journal::{DebugHttpOutcome, FlightTrackerDebugEvent, append_debug_event},
    format::{msg_aviationstack_info, msg_flight_status, msg_flights_list, msg_track_started},
    metadata::{
        apply_aviationstack_metadata, metadata_callsign, normalize_flight_code, seed_flight_aliases,
    },
    schedule::poll_readiness,
    state::save_tracker_state,
};

use super::{MAX_FLIGHTS_PER_USER, MAX_TRACKED_FLIGHTS};

use super::advance::{
    Emit, FlightUpdate, Observation, PollOutcome, RemovalReason, apply_route_outcome,
    candidate_callsign_matches_flight, format_emit, identifier_callsign, last_seen_age_secs,
};
use super::poll_cycle::{complete_poll_cycle, fetch_observation, fetch_route, plan_adsb_lookup};
use super::poll_decision::{PollPlan, poll_plan};
use super::schedule::PollReadiness;

fn find_index_by_identifier(
    flights: &[TrackedFlight],
    identifier: &FlightIdentifier,
) -> Option<usize> {
    flights
        .iter()
        .position(|flight| &flight.identifier == identifier)
}

pub(crate) fn find_flight_index(flights: &[TrackedFlight], query: &str) -> Option<usize> {
    let upper = query.to_uppercase();
    flights.iter().position(|f| {
        f.identifier.as_str().eq_ignore_ascii_case(&upper)
            || f.callsign
                .as_ref()
                .is_some_and(|cs| cs.eq_ignore_ascii_case(&upper))
            || f.alias_callsigns
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(&upper))
            || f.hex
                .as_ref()
                .is_some_and(|h| h.eq_ignore_ascii_case(&upper))
    })
}

#[cfg(test)]
pub(crate) fn aircraft_matches_tracked_callsign(
    ac: &NearbyAircraft,
    flight: &TrackedFlight,
) -> bool {
    use super::advance::flight_matches_callsign;

    matches!(&flight.identifier, FlightIdentifier::Hex(_))
        || aircraft_callsign(ac).is_some_and(|actual| flight_matches_callsign(flight, actual))
}

fn duplicate_tracking_exists(
    state: &FlightTrackerState,
    identifier: &FlightIdentifier,
    callsign: Option<&str>,
    hex: Option<&str>,
) -> bool {
    state.flights.iter().any(|f| {
        &f.identifier == identifier
            || identifier.matches(f.callsign.as_deref(), f.hex.as_deref())
            || identifier_callsign(identifier)
                .is_some_and(|candidate| candidate_callsign_matches_flight(f, candidate))
            || callsign.is_some_and(|callsign| candidate_callsign_matches_flight(f, callsign))
            || hex.is_some_and(|hex| {
                f.hex
                    .as_deref()
                    .is_some_and(|existing| existing.eq_ignore_ascii_case(hex))
            })
    })
}

fn track_started_response(
    flight: &TrackedFlight,
    aviationstack_info: Option<&str>,
    aviationstack_fallback: bool,
) -> String {
    let mut response = msg_track_started(flight);
    if let Some(info) = aviationstack_info {
        response.push_str(" | ");
        response.push_str(info);
    }
    if aviationstack_fallback {
        response.push_str(" | AviationStack nix, ADS-B-only");
    }
    response
}

fn append_consumed_emits(
    response: &mut String,
    flight: &TrackedFlight,
    emits: &[Emit],
    now: DateTime<Utc>,
) {
    for emit in emits {
        match emit {
            Emit::AdsbVisible | Emit::TrackingLost | Emit::PendingExpired => {}
            other => {
                response.push_str(" | ");
                response.push_str(&format_emit(flight, other, now));
            }
        }
    }
}

struct AviationstackLookup {
    checked: bool,
    metadata: Option<AviationstackFlightMetadata>,
    cache_changed: bool,
    failed: bool,
}

fn push_cache_alias(aliases: &mut Vec<String>, value: Option<&str>) {
    let Some(alias) = value.and_then(normalize_flight_code) else {
        return;
    };
    if !aliases
        .iter()
        .any(|known| known.eq_ignore_ascii_case(&alias))
    {
        aliases.push(alias);
    }
}

fn query_cache_aliases(identifier: &FlightIdentifier, callsign: Option<&str>) -> Vec<String> {
    let mut aliases = Vec::new();
    if matches!(identifier, FlightIdentifier::Callsign(_)) {
        push_cache_alias(&mut aliases, Some(identifier.as_str()));
    }
    push_cache_alias(&mut aliases, callsign);
    aliases
}

fn metadata_cache_aliases(metadata: &AviationstackFlightMetadata) -> Vec<String> {
    let mut aliases = Vec::new();
    push_cache_alias(&mut aliases, metadata.flight_iata.as_deref());
    push_cache_alias(&mut aliases, metadata.flight_icao.as_deref());
    push_cache_alias(&mut aliases, metadata.operating_flight_iata.as_deref());
    push_cache_alias(&mut aliases, metadata_callsign(metadata).as_deref());
    aliases
}

fn metadata_flight_date(metadata: &AviationstackFlightMetadata) -> Option<String> {
    metadata
        .flight_date
        .as_deref()
        .map(str::trim)
        .filter(|date| !date.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            metadata
                .departure_scheduled
                .as_ref()
                .or(metadata.arrival_scheduled.as_ref())
                .or(metadata.arrival_estimated.as_ref())
                .map(|dt| dt.date_naive().to_string())
        })
}

fn flight_info_cache_expiry(
    metadata: Option<&AviationstackFlightMetadata>,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    let Some(metadata) = metadata else {
        return now + TimeDelta::minutes(10);
    };

    if metadata
        .flight_status
        .as_deref()
        .is_some_and(|status| status.eq_ignore_ascii_case("landed"))
    {
        return now + TimeDelta::hours(24);
    }

    let arrival = metadata
        .arrival_actual
        .as_ref()
        .cloned()
        .or_else(|| metadata.arrival_estimated.as_ref().cloned())
        .or_else(|| metadata.arrival_scheduled.as_ref().cloned());
    let expires_at = arrival
        .map(|arrival| arrival + TimeDelta::hours(4))
        .unwrap_or_else(|| now + TimeDelta::hours(4));
    if expires_at > now {
        expires_at
    } else {
        now + TimeDelta::hours(4)
    }
}

fn prune_flight_info_cache(state: &mut FlightTrackerState, now: DateTime<Utc>) -> bool {
    let before = state.flight_info_cache.len();
    state
        .flight_info_cache
        .retain(|entry| !entry.is_expired(now) && entry.is_current());
    before != state.flight_info_cache.len()
}

fn cached_flight_info(
    state: &FlightTrackerState,
    aliases: &[String],
    now: DateTime<Utc>,
) -> Option<Option<AviationstackFlightMetadata>> {
    state
        .flight_info_cache
        .iter()
        .filter(|entry| {
            !entry.is_expired(now) && entry.is_current() && entry.matches_any_alias(aliases)
        })
        .max_by_key(|entry| entry.cached_at)
        .map(|entry| entry.metadata.clone())
}

fn upsert_flight_info_cache(
    state: &mut FlightTrackerState,
    query_aliases: &[String],
    metadata: Option<AviationstackFlightMetadata>,
    now: DateTime<Utc>,
) {
    let mut aliases = query_aliases.to_vec();
    let flight_date = metadata.as_ref().and_then(metadata_flight_date);
    if let Some(metadata) = metadata.as_ref() {
        for alias in metadata_cache_aliases(metadata) {
            push_cache_alias(&mut aliases, Some(&alias));
        }
    }
    if aliases.is_empty() {
        return;
    }

    let expires_at = flight_info_cache_expiry(metadata.as_ref(), now);
    if let Some(entry) = state.flight_info_cache.iter_mut().find(|entry| {
        entry.matches_any_alias(&aliases)
            && (entry.flight_date == flight_date
                || entry.flight_date.is_none()
                || flight_date.is_none())
    }) {
        for alias in aliases {
            push_cache_alias(&mut entry.aliases, Some(&alias));
        }
        entry.flight_date = flight_date;
        entry.metadata = metadata;
        entry.cached_at = now;
        entry.expires_at = expires_at;
        entry.cache_version = super::FLIGHT_INFO_CACHE_VERSION;
        return;
    }

    state.flight_info_cache.push(CachedFlightInfo {
        aliases,
        flight_date,
        metadata,
        cached_at: now,
        expires_at,
        cache_version: super::FLIGHT_INFO_CACHE_VERSION,
    });
}

async fn lookup_aviationstack_metadata(
    state: &mut FlightTrackerState,
    aviation_client: &AviationClient,
    identifier: &FlightIdentifier,
    callsign: Option<&str>,
    now: DateTime<Utc>,
) -> AviationstackLookup {
    let cache_changed = prune_flight_info_cache(state, now);
    let query_aliases = query_cache_aliases(identifier, callsign);
    if query_aliases.is_empty() {
        return AviationstackLookup {
            checked: false,
            metadata: None,
            cache_changed,
            failed: false,
        };
    }

    if let Some(metadata) = cached_flight_info(state, &query_aliases, now) {
        return AviationstackLookup {
            checked: true,
            metadata,
            cache_changed,
            failed: false,
        };
    }

    if !aviation_client.aviationstack_enabled() {
        return AviationstackLookup {
            checked: false,
            metadata: None,
            cache_changed,
            failed: false,
        };
    }

    match aviation_client
        .get_aviationstack_flight_metadata(identifier, callsign)
        .await
    {
        Ok(metadata) => {
            upsert_flight_info_cache(state, &query_aliases, metadata.clone(), now);
            AviationstackLookup {
                checked: true,
                metadata,
                cache_changed: true,
                failed: false,
            }
        }
        Err(e) => {
            warn!(error = ?e, identifier = %identifier, "Aviationstack metadata lookup failed");
            AviationstackLookup {
                checked: true,
                metadata: None,
                cache_changed,
                failed: true,
            }
        }
    }
}

pub(crate) async fn remove_flight_at(
    state: &mut FlightTrackerState,
    query: &str,
    data_dir: &Path,
    source: &str,
    clock: &dyn Clock,
) -> Option<String> {
    let idx = find_flight_index(&state.flights, query)?;
    let now = clock.now_utc();
    let flight = &state.flights[idx];
    let label = flight
        .callsign
        .as_deref()
        .unwrap_or(flight.identifier.as_str())
        .to_owned();
    append_debug_event(
        data_dir,
        now,
        FlightTrackerDebugEvent::tracking_removal(flight, source, last_seen_age_secs(flight, now)),
    )
    .await;
    state.flights.remove(idx);
    save_tracker_state(data_dir, state).await;
    info!(identifier = %query, source = %source, "Flight untracked");
    Some(label)
}

async fn process_latency_sensitive_command(
    cmd: TrackerCommand,
    state: &mut FlightTrackerState,
    data_dir: &Path,
    clock: &dyn Clock,
) -> Option<TrackerCommand> {
    match cmd {
        TrackerCommand::Snapshot { reply } => {
            let now = clock.now_utc();
            let _ = reply.send(build_flight_view(state, now));
            None
        }
        TrackerCommand::DeleteFromWeb { identifier, reply } => {
            let removed = remove_flight_at(state, &identifier, data_dir, "web", clock).await;
            let _ = reply.send(removed);
            None
        }
        other => Some(other),
    }
}

async fn drain_latency_sensitive_commands(
    cmd_rx: &mut Option<&mut mpsc::Receiver<TrackerCommand>>,
    state: &mut FlightTrackerState,
    data_dir: &Path,
    clock: &dyn Clock,
    deferred_commands: &mut Vec<TrackerCommand>,
    lifecycle: &Lifecycle,
) {
    let Some(cmd_rx) = cmd_rx.as_mut() else {
        return;
    };
    while !lifecycle.is_cancelled() {
        let Ok(cmd) = cmd_rx.try_recv() else {
            return;
        };
        if let Some(deferred) = process_latency_sensitive_command(cmd, state, data_dir, clock).await
        {
            deferred_commands.push(deferred);
        }
    }
}

pub(crate) async fn process_command<T, L>(
    cmd: TrackerCommand,
    state: &mut FlightTrackerState,
    sender: &Arc<ChatSender<T, L>>,
    aviation_client: &AviationClient,
    data_dir: &Path,
    clock: &dyn Clock,
) where
    T: Transport,
    L: LoginCredentials,
{
    match cmd {
        TrackerCommand::Track {
            identifier,
            requested_by,
            reply_to,
        } => {
            handle_track(
                identifier,
                &requested_by,
                &reply_to,
                state,
                sender,
                aviation_client,
                data_dir,
                clock,
            )
            .await;
        }
        TrackerCommand::Untrack {
            identifier,
            requested_by,
            is_mod,
            reply_to,
        } => {
            handle_untrack(
                &identifier,
                &requested_by,
                is_mod,
                &reply_to,
                state,
                sender,
                data_dir,
                clock,
            )
            .await;
        }
        TrackerCommand::Status {
            identifier,
            reply_to,
        } => {
            handle_status(identifier.as_deref(), &reply_to, state, sender, clock).await;
        }
        TrackerCommand::Info {
            identifier,
            reply_to,
        } => {
            handle_info(
                identifier,
                &reply_to,
                state,
                sender,
                aviation_client,
                data_dir,
                clock,
            )
            .await;
        }
        cmd @ (TrackerCommand::Snapshot { .. } | TrackerCommand::DeleteFromWeb { .. }) => {
            let _ = process_latency_sensitive_command(cmd, state, data_dir, clock).await;
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "command handler threads tracker state, client, and reply context"
)]
async fn handle_track<T, L>(
    identifier: FlightIdentifier,
    requested_by: &str,
    reply_to: &PrivmsgMessage,
    state: &mut FlightTrackerState,
    sender: &Arc<ChatSender<T, L>>,
    aviation_client: &AviationClient,
    data_dir: &Path,
    clock: &dyn Clock,
) where
    T: Transport,
    L: LoginCredentials,
{
    if state.flights.len() >= MAX_TRACKED_FLIGHTS {
        sender
            .reply(
                reply_to,
                format!("Maximal {MAX_TRACKED_FLIGHTS} Flüge gleichzeitig FDM"),
            )
            .await;
        return;
    }

    let user_count = state
        .flights
        .iter()
        .filter(|f| f.tracked_by == requested_by)
        .count();
    if user_count >= MAX_FLIGHTS_PER_USER {
        sender
            .reply(
                reply_to,
                format!("Du trackst schon {MAX_FLIGHTS_PER_USER} Flüge FDM"),
            )
            .await;
        return;
    }

    if duplicate_tracking_exists(state, &identifier, None, None) {
        sender
            .reply(reply_to, format!("{identifier} wird schon getrackt FDM"))
            .await;
        return;
    }

    let resolved_callsign = match &identifier {
        FlightIdentifier::Callsign(cs) => Some(aviation_client.resolve_callsign(cs).await),
        FlightIdentifier::Hex(_) => None,
    };

    if let Some(resolved) = &resolved_callsign
        && !resolved.eq_ignore_ascii_case(identifier.as_str())
        && duplicate_tracking_exists(state, &identifier, Some(resolved), None)
    {
        sender
            .reply(reply_to, format!("{identifier} wird schon getrackt FDM"))
            .await;
        return;
    }

    let now = clock.now_utc();
    let aviationstack_lookup = if matches!(&identifier, FlightIdentifier::Callsign(_)) {
        lookup_aviationstack_metadata(
            state,
            aviation_client,
            &identifier,
            resolved_callsign.as_deref(),
            now,
        )
        .await
    } else {
        AviationstackLookup {
            checked: false,
            metadata: None,
            cache_changed: false,
            failed: false,
        }
    };
    let aviationstack_cache_changed = aviationstack_lookup.cache_changed;
    let metadata = aviationstack_lookup.metadata;
    let aviationstack_checked = aviationstack_lookup.checked;
    let aviationstack_info = metadata
        .as_ref()
        .map(|metadata| msg_aviationstack_info(metadata, now));
    if matches!(&identifier, FlightIdentifier::Callsign(_)) {
        let outcome = if metadata.is_some() {
            DebugHttpOutcome::success("aviationstack", "flight_metadata")
        } else {
            DebugHttpOutcome::miss("aviationstack", "flight_metadata")
        };
        append_debug_event(
            data_dir,
            now,
            FlightTrackerDebugEvent::aviationstack_metadata(
                &identifier,
                resolved_callsign.as_deref(),
                outcome,
                metadata.as_ref(),
            ),
        )
        .await;
    }
    let aviationstack_fallback = aviationstack_checked
        && metadata.is_none()
        && matches!(&identifier, FlightIdentifier::Callsign(_));
    if aviationstack_fallback {
        warn!(identifier = %identifier, "AviationStack did not resolve flight; falling back to ADS-B-only tracking");
    }

    let initial_hex = match &identifier {
        FlightIdentifier::Hex(hex) => Some(hex.clone()),
        FlightIdentifier::Callsign(_) => None,
    };
    let mut flight = TrackedFlight {
        identifier: identifier.clone(),
        callsign: resolved_callsign.clone(),
        alias_callsigns: Vec::new(),
        hex: initial_hex,
        hex_source: matches!(&identifier, FlightIdentifier::Hex(_)).then_some(HexSource::UserInput),
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
        tracked_by: requested_by.to_string(),
        tracked_at: now,
        last_seen: None,
        last_visible_at: None,
        last_phase_change: None,
        polls_since_change: 0,
        takeoff_at: None,
        aviationstack_checked,
        scheduled_departure_at: None,
        last_adsb_poll_at: None,
        divert: Default::default(),
        dest_lat: None,
        dest_lon: None,
    };
    seed_flight_aliases(&mut flight);

    if let Some(metadata) = metadata.clone() {
        apply_aviationstack_metadata(&mut flight, metadata);
    }
    if duplicate_tracking_exists(
        state,
        &identifier,
        flight.callsign.as_deref(),
        flight.hex.as_deref(),
    ) {
        if aviationstack_cache_changed {
            save_tracker_state(data_dir, state).await;
        }
        sender
            .reply(reply_to, format!("{identifier} wird schon getrackt FDM"))
            .await;
        return;
    }

    let keep_pending = matches!(&identifier, FlightIdentifier::Callsign(_))
        && aviation_client.aviationstack_enabled();

    let mut first_cycle: Option<FlightUpdate> = None;
    match poll_readiness(&flight, now) {
        PollReadiness::NotDue(_) => {}
        PollReadiness::Due | PollReadiness::Expired => {
            let plan = plan_adsb_lookup(&flight);
            let observation = fetch_observation(plan, aviation_client).await;
            let discard_reply = match &observation.outcome {
                PollOutcome::Hit(_) | PollOutcome::Miss => {
                    format!("{identifier} nicht gefunden im ADS-B FDM")
                }
                PollOutcome::Error => "ADS-B Anfrage fehlgeschlagen FDM".to_string(),
                PollOutcome::Timeout => "ADS-B Anfrage Timeout FDM".to_string(),
            };
            match complete_poll_cycle(&mut flight, observation, now, aviation_client).await {
                Some(update) => {
                    let seed_applied = flight.last_visible_at.is_some();
                    if !seed_applied && !keep_pending {
                        sender.reply(reply_to, discard_reply).await;
                        return;
                    }
                    first_cycle = Some(update);
                }
                None => {
                    if !keep_pending {
                        sender.reply(reply_to, discard_reply).await;
                        return;
                    }
                }
            }
        }
    }

    if aviation_client.aviationstack_enabled()
        && !flight.aviationstack_checked
        && flight.callsign.is_some()
    {
        flight.aviationstack_checked = true;
        let lookup = lookup_aviationstack_metadata(
            state,
            aviation_client,
            &identifier,
            flight.callsign.as_deref(),
            now,
        )
        .await;
        let outcome = if lookup.metadata.is_some() {
            DebugHttpOutcome::success("aviationstack", "flight_metadata")
        } else {
            DebugHttpOutcome::miss("aviationstack", "flight_metadata")
        };
        append_debug_event(
            data_dir,
            now,
            FlightTrackerDebugEvent::aviationstack_metadata(
                &identifier,
                flight.callsign.as_deref(),
                outcome,
                lookup.metadata.as_ref(),
            ),
        )
        .await;
        if let Some(metadata) = lookup.metadata {
            apply_aviationstack_metadata(&mut flight, metadata);
        }
    }

    if duplicate_tracking_exists(
        state,
        &identifier,
        flight.callsign.as_deref(),
        flight.hex.as_deref(),
    ) {
        sender
            .reply(reply_to, format!("{identifier} wird schon getrackt FDM"))
            .await;
        return;
    }

    resolve_route_if_needed(&mut flight, aviation_client, data_dir, now).await;

    let mut response = track_started_response(
        &flight,
        aviationstack_info.as_deref(),
        aviationstack_fallback,
    );
    if let Some(update) = first_cycle {
        for event in update.debug {
            append_debug_event(data_dir, now, event).await;
        }
        append_consumed_emits(&mut response, &flight, &update.emits, now);
    }
    append_debug_event(
        data_dir,
        now,
        FlightTrackerDebugEvent::track_started(&flight),
    )
    .await;
    state.flights.push(flight);
    save_tracker_state(data_dir, state).await;

    info!(identifier = %identifier, requested_by = %requested_by, "Flight tracking started");
    sender.reply(reply_to, response).await;
}

#[expect(
    clippy::too_many_arguments,
    reason = "command handler threads shared tracker deps + injected clock"
)]
async fn handle_untrack<T, L>(
    identifier: &str,
    requested_by: &str,
    is_mod: bool,
    reply_to: &PrivmsgMessage,
    state: &mut FlightTrackerState,
    sender: &Arc<ChatSender<T, L>>,
    data_dir: &Path,
    clock: &dyn Clock,
) where
    T: Transport,
    L: LoginCredentials,
{
    let Some(idx) = find_flight_index(&state.flights, identifier) else {
        sender
            .reply(reply_to, format!("{identifier} nicht gefunden FDM"))
            .await;
        return;
    };

    let flight = &state.flights[idx];
    if flight.tracked_by != requested_by && !is_mod {
        sender
            .reply(
                reply_to,
                "Nur der Tracker oder Mods können das untracking machen FDM",
            )
            .await;
        return;
    }

    let name = remove_flight_at(state, identifier, data_dir, requested_by, clock)
        .await
        .unwrap_or_else(|| identifier.to_owned());

    sender
        .reply(reply_to, format!("{name} wird nicht mehr getrackt Okayge"))
        .await;
}

async fn handle_info<T, L>(
    identifier: FlightIdentifier,
    reply_to: &PrivmsgMessage,
    state: &mut FlightTrackerState,
    sender: &Arc<ChatSender<T, L>>,
    aviation_client: &AviationClient,
    data_dir: &Path,
    clock: &dyn Clock,
) where
    T: Transport,
    L: LoginCredentials,
{
    let tracked_callsign = find_flight_index(state.flights.as_slice(), identifier.as_str())
        .and_then(|idx| state.flights[idx].callsign.clone());
    let resolved_callsign = match (&identifier, tracked_callsign.as_deref()) {
        (_, Some(callsign)) => Some(callsign.to_string()),
        (FlightIdentifier::Callsign(callsign), None) => {
            Some(aviation_client.resolve_callsign(callsign).await)
        }
        (FlightIdentifier::Hex(_), None) => None,
    };

    let now = clock.now_utc();
    let lookup = lookup_aviationstack_metadata(
        state,
        aviation_client,
        &identifier,
        resolved_callsign.as_deref(),
        now,
    )
    .await;
    if lookup.cache_changed {
        save_tracker_state(data_dir, state).await;
    }

    let response = if let Some(metadata) = lookup.metadata {
        msg_aviationstack_info(&metadata, now)
    } else if lookup.failed {
        "AviationStack Anfrage fehlgeschlagen FDM".to_string()
    } else if lookup.checked || aviation_client.aviationstack_enabled() {
        format!("{identifier} nicht bei AviationStack gefunden FDM")
    } else {
        "AviationStack nicht konfiguriert FDM".to_string()
    };

    sender.reply(reply_to, response).await;
}

async fn handle_status<T, L>(
    identifier: Option<&str>,
    reply_to: &PrivmsgMessage,
    state: &FlightTrackerState,
    sender: &Arc<ChatSender<T, L>>,
    clock: &dyn Clock,
) where
    T: Transport,
    L: LoginCredentials,
{
    let response = match identifier {
        None => msg_flights_list(&state.flights),
        Some(id) => match find_flight_index(&state.flights, id) {
            Some(idx) => msg_flight_status(&state.flights[idx], clock.now_utc()),
            None => format!("{id} nicht gefunden FDM"),
        },
    };

    sender.reply(reply_to, response).await;
}

/// On the `!track` initial poll: if the flight has a callsign and no route yet,
/// resolve it. Keyed on `flight.callsign` so it covers both a callsign the Seed
/// just resolved (hex track) and one known from the request.
async fn resolve_route_if_needed(
    flight: &mut TrackedFlight,
    aviation_client: &AviationClient,
    data_dir: &Path,
    now: DateTime<Utc>,
) {
    if flight.route.is_none()
        && let Some(callsign) = flight.callsign.clone()
    {
        let outcome = fetch_route(aviation_client, &callsign).await;
        for event in apply_route_outcome(flight, &callsign, outcome) {
            append_debug_event(data_dir, now, event).await;
        }
    }
}

pub async fn poll_all_flights_with_commands<T, L, P>(
    state: &mut FlightTrackerState,
    sender: &Arc<ChatSender<T, L>>,
    channel: &str,
    aviation_client: &P,
    data_dir: &Path,
    clock: &dyn Clock,
    cmd_rx: Option<&mut mpsc::Receiver<TrackerCommand>>,
) -> Vec<TrackerCommand>
where
    T: Transport,
    L: LoginCredentials,
    P: AdsbProvider + Clone + Send + Sync + 'static,
{
    let lifecycle = Lifecycle::new();
    try_poll_all_flights_with_commands(
        state,
        sender,
        channel,
        aviation_client,
        data_dir,
        clock,
        cmd_rx,
        &lifecycle,
    )
    .await
    .unwrap_or_else(|source| {
        panic!("nested flight-poll child failed: {source}");
    })
    .unwrap_or_default()
}

#[expect(
    clippy::too_many_arguments,
    reason = "poll cycle threads shared tracker dependencies, command input, and lifecycle"
)]
pub(crate) async fn try_poll_all_flights_with_commands<T, L, P>(
    state: &mut FlightTrackerState,
    sender: &Arc<ChatSender<T, L>>,
    channel: &str,
    aviation_client: &P,
    data_dir: &Path,
    clock: &dyn Clock,
    mut cmd_rx: Option<&mut mpsc::Receiver<TrackerCommand>>,
    lifecycle: &Lifecycle,
) -> Result<Option<Vec<TrackerCommand>>, JoinError>
where
    T: Transport,
    L: LoginCredentials,
    P: AdsbProvider + Clone + Send + Sync + 'static,
{
    if lifecycle.is_cancelled() {
        return Ok(None);
    }
    let now = clock.now_utc();
    let mut changed = false;
    let mut removals: Vec<FlightIdentifier> = Vec::new();
    let mut messages: Vec<String> = Vec::new();
    let mut deferred_commands = Vec::new();

    let mut poll_tasks: TaskGroup<PollAttempt> = TaskGroup::new(lifecycle, "flight poll children");
    let mut fetch_results: Vec<PollAttempt> = Vec::new();

    for (child_index, flight) in state.flights.iter().enumerate() {
        let readiness = poll_readiness(flight, now);
        match poll_plan(flight, readiness, now) {
            PollPlan::Skip => continue,
            PollPlan::Apply(update) => {
                let identifier = flight.identifier.clone();
                for event in update.debug {
                    if lifecycle.is_cancelled() {
                        return Ok(None);
                    }
                    append_debug_event(data_dir, now, event).await;
                }
                debug_assert!(
                    update.followups.is_empty(),
                    "Pending Expired must not queue Route Resolution"
                );
                for emit in &update.emits {
                    messages.push(format_emit(flight, emit, now));
                }
                if let Some(reason) = update.removal {
                    match reason {
                        RemovalReason::PendingExpired => {
                            info!(
                                identifier = %identifier,
                                "Removing pending flight: ADS-B never appeared"
                            );
                        }
                        RemovalReason::TrackingLost { secs } => {
                            info!(
                                identifier = %identifier,
                                "Removing flight: tracking lost for {secs}s"
                            );
                        }
                    }
                    removals.push(identifier);
                }
                changed = true;
                continue;
            }
            PollPlan::Due => {}
        }

        if lifecycle.is_cancelled() {
            return Ok(None);
        }
        let ac = aviation_client.clone();
        let id = flight.identifier.clone();
        let plan = plan_adsb_lookup(flight);
        let cancellation = poll_tasks.cancellation_token();
        poll_tasks
            .spawn(format!("poll-{child_index}"), async move {
                if cancellation.is_cancelled() {
                    return None;
                }
                // This provider request is the atomic operation. Once started it is
                // allowed to finish (and is bounded by POLL_TIMEOUT); cancellation
                // prevents queued children and all later state/chat effects.
                let observation = fetch_observation(plan, &ac).await;
                Some((id, observation))
            })
            .await;
    }

    let child_cancellation = poll_tasks.cancellation_token();
    let (completions, poll_cancelled) = if let Some(cmd_rx) = cmd_rx.as_mut() {
        let mut command_channel_open = true;
        let mut join_all = Box::pin(poll_tasks.join_all());
        loop {
            enum JoinWait {
                Completed(Vec<TaskCompletion<PollAttempt>>),
                Cancelled,
            }
            let completed = tokio::select! {
                biased;
                _ = lifecycle.wait_for_cancellation() => JoinWait::Cancelled,
                completions = &mut join_all => JoinWait::Completed(completions),
                cmd = cmd_rx.recv(), if command_channel_open => {
                    match cmd {
                        Some(cmd) => {
                            if lifecycle.is_cancelled() {
                                JoinWait::Cancelled
                            } else if let Some(deferred) = process_latency_sensitive_command(
                                cmd,
                                state,
                                data_dir,
                                clock,
                            )
                            .await
                            {
                                deferred_commands.push(deferred);
                                continue;
                            } else {
                                continue;
                            }
                        }
                        None => {
                            command_channel_open = false;
                            continue;
                        }
                    }
                }
            };
            match completed {
                JoinWait::Completed(completions) => break (completions, false),
                JoinWait::Cancelled => {
                    child_cancellation.cancel();
                    break (join_all.await, true);
                }
            }
        }
    } else {
        let mut join_all = Box::pin(poll_tasks.join_all());
        let completed = tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => None,
            completions = &mut join_all => Some(completions),
        };
        match completed {
            Some(completions) => (completions, false),
            None => {
                child_cancellation.cancel();
                (join_all.await, true)
            }
        }
    };

    // Join errors win even when cancellation was observed, so a panicking
    // provider child still faults the parent task.
    fetch_results.extend(collect_poll_results(completions)?);
    if poll_cancelled || lifecycle.is_cancelled() {
        return Ok(None);
    }

    for attempt in fetch_results {
        let Some((identifier, observation)) = attempt else {
            continue;
        };
        if lifecycle.is_cancelled() {
            return Ok(None);
        }
        drain_latency_sensitive_commands(
            &mut cmd_rx,
            state,
            data_dir,
            clock,
            &mut deferred_commands,
            lifecycle,
        )
        .await;

        let Some(idx) = find_index_by_identifier(&state.flights, &identifier) else {
            continue;
        };

        let Some(update) = ({
            let flight = &mut state.flights[idx];
            complete_poll_cycle(flight, observation, now, aviation_client).await
        }) else {
            continue;
        };
        if lifecycle.is_cancelled() {
            return Ok(None);
        }
        changed = true;

        for event in update.debug {
            if lifecycle.is_cancelled() {
                return Ok(None);
            }
            append_debug_event(data_dir, now, event).await;
        }
        debug_assert!(
            update.followups.is_empty(),
            "Poll Cycle must resolve Route Resolution followups"
        );

        for emit in &update.emits {
            let msg = format_emit(&state.flights[idx], emit, now);
            messages.push(msg);
        }

        if let Some(reason) = update.removal {
            match reason {
                RemovalReason::TrackingLost { secs } => {
                    info!(identifier = %identifier, "Removing flight: tracking lost for {secs}s");
                }
                RemovalReason::PendingExpired => {
                    info!(
                        identifier = %identifier,
                        "Removing pending flight: ADS-B never appeared"
                    );
                }
            }
            removals.push(identifier.clone());
        }
    }

    if lifecycle.is_cancelled() {
        return Ok(None);
    }
    for identifier in removals {
        if let Some(idx) = find_index_by_identifier(&state.flights, &identifier) {
            state.flights.remove(idx);
            changed = true;
        }
    }

    if lifecycle.is_cancelled() {
        return Ok(None);
    }
    drain_latency_sensitive_commands(
        &mut cmd_rx,
        state,
        data_dir,
        clock,
        &mut deferred_commands,
        lifecycle,
    )
    .await;

    // Save before announcing so a restart after the say cannot re-emit the
    // same phase change. At-most-once: cancellation mid-loop drops the rest.
    if changed {
        if lifecycle.is_cancelled() {
            return Ok(None);
        }
        save_tracker_state(data_dir, state).await;
    }

    for msg in messages {
        if lifecycle.is_cancelled() {
            return Ok(None);
        }
        sender.say(channel.to_string(), msg).await;
    }

    Ok(Some(deferred_commands))
}

type PollAttempt = Option<(FlightIdentifier, Observation)>;

fn collect_poll_results(
    completions: Vec<TaskCompletion<PollAttempt>>,
) -> Result<Vec<PollAttempt>, JoinError> {
    completions
        .into_iter()
        .map(TaskCompletion::into_result)
        .collect()
}

#[cfg(test)]
mod poll_task_group_tests {
    use super::{Observation, PollAttempt, PollOutcome, collect_poll_results};
    use crate::aviation::tracker::FlightIdentifier;
    use crate::lifecycle::{Lifecycle, TaskGroup};

    #[tokio::test]
    async fn successful_child_results_are_collected() {
        let lifecycle = Lifecycle::new();
        let mut group = TaskGroup::new(&lifecycle, "test flight poll children");
        group
            .spawn("poll-0", async {
                let attempt: PollAttempt = Some((
                    FlightIdentifier::Callsign("TEST".to_owned()),
                    Observation {
                        used_hex: false,
                        outcome: PollOutcome::Error,
                    },
                ));
                attempt
            })
            .await;

        let results = collect_poll_results(group.join_all().await).unwrap();

        assert_eq!(results.len(), 1);
        assert!(
            matches!(results[0].as_ref().unwrap().0, FlightIdentifier::Callsign(ref call) if call == "TEST")
        );
    }

    #[tokio::test]
    async fn cancellation_prevents_a_queued_provider_child_from_starting() {
        let lifecycle = Lifecycle::new();
        let mut group = TaskGroup::new(&lifecycle, "test flight poll children");
        let cancellation = group.cancellation_token();
        let (start_queued_child, child_may_check) = tokio::sync::oneshot::channel();
        group
            .spawn("poll-0", async move {
                child_may_check.await.expect("test releases queued child");
                if cancellation.is_cancelled() {
                    return None;
                }
                Some((
                    FlightIdentifier::Callsign("SHOULD_NOT_START".to_owned()),
                    Observation {
                        used_hex: false,
                        outcome: PollOutcome::Error,
                    },
                ))
            })
            .await;
        group.cancel();
        start_queued_child
            .send(())
            .expect("queued child is still owned by the group");

        let results = collect_poll_results(group.join_all().await).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_none());
    }

    #[tokio::test]
    async fn nested_poll_join_error_is_not_discarded() {
        let lifecycle = Lifecycle::new();
        let mut group = TaskGroup::new(&lifecycle, "test flight poll children");
        group
            .spawn("poll-0", async { panic!("provider child failed") })
            .await;

        let source = collect_poll_results(group.join_all().await)
            .expect_err("a child panic must be propagated to the flight tracker parent");

        assert!(source.is_panic());
    }
}

#[cfg(test)]
mod consume_emits_tests {
    use super::*;
    use crate::aviation::tracker::advance::Emit;
    use crate::aviation::tracker::test_support::{dt, tracked_flight};

    #[test]
    fn append_consumed_emits_swallows_adsb_visible_and_appends_takeoff() {
        let flight = tracked_flight();
        let now = dt("2026-04-18T12:00:00Z");
        let mut response = "Tracke DLH1929 Okayge".to_string();
        append_consumed_emits(
            &mut response,
            &flight,
            &[Emit::AdsbVisible, Emit::Takeoff],
            now,
        );
        assert!(
            !response.contains("ADS-B sichtbar"),
            "AdsbVisible must be swallowed: {response}"
        );
        assert!(
            response.contains("ist gestartet"),
            "Takeoff must be appended: {response}"
        );
        assert!(response.starts_with("Tracke DLH1929 Okayge | "));
    }
}
