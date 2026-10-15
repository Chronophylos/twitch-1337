use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::info;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::aviation::AviationClient;
use crate::lifecycle::Lifecycle;
use crate::twitch::ChatSender;
use crate::util::clock::Clock;

use super::{
    TrackerCommand,
    commands::{process_command, try_poll_all_flights_with_commands},
    debug_journal::{DEBUG_JOURNAL_KEEP_FILES, prune_debug_journals},
    schedule::next_poll_at,
    state::load_tracker_state,
};

pub async fn run_flight_tracker<T, L>(
    mut cmd_rx: mpsc::Receiver<TrackerCommand>,
    sender: Arc<ChatSender<T, L>>,
    channel: String,
    aviation_client: AviationClient,
    data_dir: PathBuf,
    clock: Arc<dyn Clock>,
    lifecycle: Lifecycle,
) where
    T: Transport,
    L: LoginCredentials,
{
    if lifecycle.is_cancelled() {
        return;
    }

    let mut state = load_tracker_state(&data_dir).await;
    info!(flights = state.flights.len(), "Flight tracker started");

    if lifecycle.is_cancelled() {
        return;
    }
    prune_debug_journals(&data_dir, DEBUG_JOURNAL_KEEP_FILES).await;
    if lifecycle.is_cancelled() {
        return;
    }
    let mut last_journal_date = clock.now_utc().date_naive();

    loop {
        if lifecycle.is_cancelled() {
            info!("Flight tracker lifecycle cancelled, shutting down");
            return;
        }

        if state.flights.is_empty() {
            let cmd = tokio::select! {
                biased;
                _ = lifecycle.wait_for_cancellation() => {
                    info!("Flight tracker lifecycle cancelled, shutting down");
                    return;
                }
                cmd = cmd_rx.recv() => cmd,
            };
            let Some(cmd) = cmd else {
                info!("Flight tracker command channel closed, shutting down");
                return;
            };
            if lifecycle.is_cancelled() {
                return;
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
        } else {
            loop {
                if lifecycle.is_cancelled() {
                    return;
                }
                match cmd_rx.try_recv() {
                    Ok(cmd) => {
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
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        info!("Flight tracker command channel closed, shutting down");
                        return;
                    }
                }
            }

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
            .unwrap_or_else(|source| {
                panic!("nested flight-poll child failed: {source}");
            });
            let Some(deferred_commands) = deferred_commands else {
                info!("Flight tracker lifecycle cancelled during poll, shutting down");
                return;
            };
            for cmd in deferred_commands {
                if lifecycle.is_cancelled() {
                    return;
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
                return;
            }
            let now = clock.now_utc();
            // Roll the journal forward: once the date changes, drop stale files.
            let today = now.date_naive();
            if today != last_journal_date {
                prune_debug_journals(&data_dir, DEBUG_JOURNAL_KEEP_FILES).await;
                if lifecycle.is_cancelled() {
                    return;
                }
                last_journal_date = today;
            }
            let next_at = next_poll_at(&state.flights, now).unwrap_or(now);
            tracing::debug!(
                next_poll_at = %next_at,
                flights = state.flights.len(),
                "Sleeping until next flight poll"
            );

            let cmd = tokio::select! {
                biased;
                _ = lifecycle.wait_for_cancellation() => {
                    info!("Flight tracker lifecycle cancelled, shutting down");
                    return;
                }
                _ = clock.sleep_until(next_at) => None,
                cmd = cmd_rx.recv() => cmd,
            };
            let Some(cmd) = cmd else {
                continue;
            };
            if lifecycle.is_cancelled() {
                return;
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
    }
}
