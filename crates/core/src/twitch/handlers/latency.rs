use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

use tokio::{
    sync::broadcast,
    time::{Duration, sleep},
};
use tracing::{Span, debug, info, warn};
use twitch_irc::{
    TwitchIRCClient, irc,
    login::LoginCredentials,
    message::{PongMessage, ServerMessage},
    transport::Transport,
};

use super::Inbound;
use crate::lifecycle::Lifecycle;
use crate::settings::SettingsHandle;

/// Interval between PING measurements
pub(crate) const LATENCY_PING_INTERVAL: Duration = Duration::from_secs(300);

/// Timeout waiting for PONG response
pub(crate) const LATENCY_PING_TIMEOUT: Duration = Duration::from_secs(10);

/// EMA smoothing factor (0.2 = moderate responsiveness)
pub(crate) const LATENCY_EMA_ALPHA: f64 = 0.2;

/// Only log EMA changes at info level when delta exceeds this threshold
pub(crate) const LATENCY_LOG_THRESHOLD: u32 = 10;

/// Periodically measures IRC latency via PING/PONG and updates a shared EMA estimate.
///
/// Sends a PING with a unique nonce every 5 minutes, measures the round-trip time
/// from the matching PONG response, and updates an exponential moving average (EMA)
/// of the one-way latency. The EMA is stored in a shared `AtomicU32` that other
/// handlers (e.g., the 1337 handler) read for timing adjustments.
///
/// The EMA is seeded from `twitch.expected_latency` and re-based whenever that
/// setting's value actually changes via the dashboard: the edit is
/// authoritative, so the EMA and the shared cell jump straight to the new value
/// instead of converging over multiple measurements. Unrelated settings
/// commits (no-op writes, other knobs) leave the EMA untouched.
///
/// The handler is fully independent — PING failures or PONG timeouts are logged
/// but never crash the handler or affect the EMA.
pub async fn run_latency_handler<T, L>(
    client: Arc<TwitchIRCClient<T, L>>,
    broadcast_tx: broadcast::Sender<Inbound>,
    settings: SettingsHandle,
    latency: Arc<AtomicU32>,
    irc_connected: Arc<AtomicBool>,
    lifecycle: Lifecycle,
) where
    T: Transport,
    L: LoginCredentials,
{
    let initial = settings.load().twitch.expected_latency;
    info!(initial_latency_ms = initial, "Latency handler started");

    // Spawned only after the IRC client authenticated and joined channels, so the
    // connection is healthy at this point. Mark healthy immediately — without
    // this, /healthz returns 503 for the full 5-min ping interval after start.
    irc_connected.store(true, Ordering::Relaxed);

    // Armed at construction: a settings commit fired between here and the
    // first `sub.next()` await cannot be lost.
    let mut sub = settings.subscribe();
    let mut last_seen: u32 = initial;
    let mut ema: f64 = f64::from(initial);
    let mut consecutive_misses: u32 = 0;
    let mut last_logged_ema: u32 = initial;

    loop {
        tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => return,
            () = sleep(LATENCY_PING_INTERVAL) => {
                let nonce = jiff::Timestamp::now().as_nanosecond().to_string();

                // Subscribe before sending so we don't miss the PONG on fast connections
                let mut broadcast_rx = broadcast_tx.subscribe();

                let send_time = tokio::time::Instant::now();
                if let Err(e) = client.send_message(irc!["PING", nonce.clone()]).await {
                    crate::log_handled!(warn, e, "Failed to send PING");
                    continue;
                }
                let pong_result = tokio::time::timeout(LATENCY_PING_TIMEOUT, async {
                    loop {
                        match broadcast_rx.recv().await {
                            // Pongs with mismatched nonces (library keepalives) fall
                            // through the guard to the wildcard arm and keep waiting.
                            Ok(inbound) if is_our_pong(&inbound.msg, &nonce) => {
                                return (send_time.elapsed(), inbound.handler_span("latency"));
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                warn!("Broadcast channel closed during PONG wait");
                                return (send_time.elapsed(), Span::none());
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                warn!(skipped = n, "Latency handler lagged during PONG wait");
                                crate::util::observability::metrics::broadcast_lagged("latency", n);
                            }
                            _ => {}
                        }
                    }
                })
                .await;

                let (rtt, span) = match pong_result {
                    Ok(measured) => {
                        consecutive_misses = 0;
                        irc_connected.store(true, Ordering::Relaxed);
                        measured
                    }
                    Err(_) => {
                        warn!("PONG timeout after {:?}", LATENCY_PING_TIMEOUT);
                        consecutive_misses += 1;
                        if consecutive_misses >= 3 {
                            irc_connected.store(false, Ordering::Relaxed);
                            warn!("3 consecutive PONG timeouts; marking IRC disconnected");
                        }
                        continue;
                    }
                };

                span.in_scope(|| {
                    let one_way_ms = rtt.as_millis() as f64 / 2.0;
                    ema = LATENCY_EMA_ALPHA * one_way_ms + (1.0 - LATENCY_EMA_ALPHA) * ema;
                    let ema_rounded = ema.round() as u32;

                    latency.store(ema_rounded, Ordering::Relaxed);

                    debug!(
                        rtt_ms = rtt.as_millis() as u64,
                        one_way_ms = one_way_ms as u64,
                        ema_ms = ema_rounded,
                        "Latency measurement"
                    );

                    // Log at info level only when EMA shifts significantly
                    if ema_rounded.abs_diff(last_logged_ema) >= LATENCY_LOG_THRESHOLD {
                        info!(
                            previous_ms = last_logged_ema,
                            current_ms = ema_rounded,
                            "Latency EMA changed"
                        );
                        last_logged_ema = ema_rounded;
                    }
                });
            }
            () = sub.next() => {
                if lifecycle.is_cancelled() {
                    return;
                }
                // Every commit notifies, including no-op writes and unrelated
                // edits (cooldowns, toggles). Only a genuine change to
                // `twitch.expected_latency` re-bases the EMA; the new value is
                // authoritative and applies immediately, no PONG required.
                let latest = settings.load().twitch.expected_latency;
                let changed = latest != last_seen;
                last_seen = latest;
                if !changed {
                    continue;
                }
                // The edit is authoritative even when the new value is
                // numerically unchanged from the current EMA. Reset
                // `last_logged_ema` to the new value so the next *significant*
                // EMA shift still logs.
                ema = f64::from(latest);
                latency.store(latest, Ordering::Relaxed);
                last_logged_ema = latest;
                info!(rebased_ms = latest, "expected_latency edited; EMA re-based");
            }
        }
    }
}

/// The PONG answering our PING carries the nonce as its second parameter.
fn is_our_pong(msg: &ServerMessage, nonce: &str) -> bool {
    matches!(
        msg,
        ServerMessage::Pong(PongMessage { source, .. })
            if source.params.get(1).map(String::as_str) == Some(nonce)
    )
}
