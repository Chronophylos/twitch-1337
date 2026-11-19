use tokio::sync::{broadcast, mpsc::UnboundedReceiver};
use tracing::{debug, info, trace};
use twitch_irc::message::ServerMessage;

use crate::lifecycle::Lifecycle;

/// Message router task that broadcasts incoming IRC messages to all handlers.
///
/// Reads from the twitch-irc receiver and broadcasts to all subscribed handlers.
/// Exits when the incoming_messages channel is closed.
pub async fn run_message_router(
    mut incoming_messages: UnboundedReceiver<ServerMessage>,
    broadcast_tx: broadcast::Sender<ServerMessage>,
    lifecycle: Lifecycle,
) {
    info!("Message router started");

    loop {
        let message = tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => {
                debug!("Message router lifecycle cancelled, shutting down");
                return;
            }
            message = incoming_messages.recv() => match message {
                Some(message) => message,
                None => {
                    debug!("Message router exited (connection closed)");
                    return;
                }
            }
        };
        if lifecycle.is_cancelled() {
            return;
        }
        trace!(message = ?message, "Routing IRC message");

        // Broadcast to all listeners (ignore errors if no receivers)
        broadcast_tx.send(message).ok();
    }
}
