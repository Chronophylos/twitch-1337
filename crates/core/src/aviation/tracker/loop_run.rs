use std::path::PathBuf;
use std::sync::Arc;

use eyre::WrapErr as _;
use tokio::sync::mpsc;
use tracing::{Instrument as _, Span, info, info_span};
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::aviation::AviationClient;
use crate::lifecycle::Lifecycle;
use crate::twitch::ChatSender;
use crate::util::clock::Clock;

use super::{
    FlightPhase, TrackedFlight, TrackerCommand,
    commands::{process_command, try_poll_all_flights_with_commands},
    schedule::next_poll_at,
    state::load_tracker_state,
};

/// Returns `Err` when a nested flight-poll child fails, which Faults the
/// runtime.
pub async fn run_flight_tracker<T, L>(
    mut cmd_rx: mpsc::Receiver<TrackerCommand>,
    sender: Arc<ChatSender<T, L>>,
    channel: String,
    aviation_client: AviationClient,
    data_dir: PathBuf,
    clock: Arc<dyn Clock>,
    lifecycle: Lifecycle,
) -> eyre::Result<()>
where
    T: Transport,
    L: LoginCredentials,
{
    if lifecycle.is_cancelled() {
        return Ok(());
    }

    let mut state = load_tracker_state(&data_dir).await;
    info!(flights = state.flights.len(), "Flight tracker started");

    if lifecycle.is_cancelled() {
        return Ok(());
    }

    loop {
        if lifecycle.is_cancelled() {
            info!("Flight tracker lifecycle cancelled, shutting down");
            return Ok(());
        }
        // Startup, and after every command or poll cycle.
        record_flights_tracked(&state.flights);

        if state.flights.is_empty() {
            let cmd = tokio::select! {
                biased;
                _ = lifecycle.wait_for_cancellation() => {
                    info!("Flight tracker lifecycle cancelled, shutting down");
                    return Ok(());
                }
                cmd = cmd_rx.recv() => cmd,
            };
            let Some(cmd) = cmd else {
                info!("Flight tracker command channel closed, shutting down");
                return Ok(());
            };
            if lifecycle.is_cancelled() {
                return Ok(());
            }
            let span = command_span(&cmd);
            process_command(
                cmd,
                &mut state,
                &sender,
                &aviation_client,
                &data_dir,
                &*clock,
            )
            .instrument(span)
            .await;
        } else {
            loop {
                if lifecycle.is_cancelled() {
                    return Ok(());
                }
                match cmd_rx.try_recv() {
                    Ok(cmd) => {
                        let span = command_span(&cmd);
                        process_command(
                            cmd,
                            &mut state,
                            &sender,
                            &aviation_client,
                            &data_dir,
                            &*clock,
                        )
                        .instrument(span)
                        .await;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        info!("Flight tracker command channel closed, shutting down");
                        return Ok(());
                    }
                }
            }

            // One root trace per tick: the poll and deferred commands, with
            // their decision events. The sleep below stays outside it.
            let cycle_span = info_span!(
                parent: None,
                "tracker.poll_cycle",
                aviation.tracker.flights = state.flights.len(),
            );
            let next_at = async {
                let deferred_commands = try_poll_all_flights_with_commands(
                    &mut state,
                    &sender,
                    &channel,
                    &aviation_client,
                    &data_dir,
                    &*clock,
                    Some(&mut cmd_rx),
                    &lifecycle,
                )
                .await
                .wrap_err("nested flight-poll child failed")?;
                let Some(deferred_commands) = deferred_commands else {
                    info!("Flight tracker lifecycle cancelled during poll, shutting down");
                    return Ok(None);
                };
                for cmd in deferred_commands {
                    if lifecycle.is_cancelled() {
                        return Ok(None);
                    }
                    process_command(
                        cmd,
                        &mut state,
                        &sender,
                        &aviation_client,
                        &data_dir,
                        &*clock,
                    )
                    .await;
                }

                if lifecycle.is_cancelled() {
                    return Ok(None);
                }
                let now = clock.now_utc();
                let next_at = next_poll_at(&state.flights, now).unwrap_or(now);
                tracing::debug!(
                    next_poll_at = %next_at,
                    flights = state.flights.len(),
                    "Sleeping until next flight poll"
                );
                Ok::<_, eyre::Report>(Some(next_at))
            }
            .instrument(cycle_span)
            .await?;
            let Some(next_at) = next_at else {
                return Ok(());
            };
            record_flights_tracked(&state.flights);

            let cmd = tokio::select! {
                biased;
                _ = lifecycle.wait_for_cancellation() => {
                    info!("Flight tracker lifecycle cancelled, shutting down");
                    return Ok(());
                }
                _ = clock.sleep_until(next_at) => None,
                cmd = cmd_rx.recv() => cmd,
            };
            let Some(cmd) = cmd else {
                continue;
            };
            if lifecycle.is_cancelled() {
                return Ok(());
            }
            let span = command_span(&cmd);
            process_command(
                cmd,
                &mut state,
                &sender,
                &aviation_client,
                &data_dir,
                &*clock,
            )
            .instrument(span)
            .await;
        }
    }
}

/// Root span for a command handled between polls, so its decisions (the
/// `!track` lookups and initial poll included) land in one trace. Commands
/// deferred during a poll run inside `tracker.poll_cycle` instead.
fn command_span(cmd: &TrackerCommand) -> Span {
    let kind = match cmd {
        TrackerCommand::Track { .. } => "track",
        TrackerCommand::Untrack { .. } => "untrack",
        TrackerCommand::Status { .. } => "status",
        TrackerCommand::Info { .. } => "info",
        TrackerCommand::Snapshot { .. } => "snapshot",
        TrackerCommand::DeleteFromWeb { .. } => "delete_from_web",
    };
    info_span!(parent: None, "tracker.command", aviation.tracker.command = kind)
}

/// `tracker.flights.tracked` per Flight Phase, empty phases as 0.
fn record_flights_tracked(flights: &[TrackedFlight]) {
    crate::util::observability::metrics::flights_tracked(FlightPhase::ALL.map(|phase| {
        let count = flights.iter().filter(|f| f.phase == phase).count();
        (phase.label(), count as u64)
    }));
}
