//! Append every PRIVMSG (and the bot's own replies, see ai/command.rs) to the
//! line-buffered `transcripts/today.md`. Best-effort; errors only logged.

use std::sync::Arc;

use tokio::sync::broadcast;
use tracing::error;
use twitch_irc::message::ServerMessage;

use crate::ai::memory::transcript::TranscriptWriter;
use crate::lifecycle::Lifecycle;
use crate::twitch::echo::EchoGuard;

pub async fn run_transcript_tap(
    mut rx: broadcast::Receiver<ServerMessage>,
    writer: Arc<TranscriptWriter>,
    channel: String,
    echo: Arc<EchoGuard>,
    lifecycle: Lifecycle,
) {
    loop {
        let message = tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => return,
            result = rx.recv() => result,
        };
        if lifecycle.is_cancelled() {
            return;
        }
        match message {
            Ok(ServerMessage::Privmsg(p)) if p.channel_login == channel => {
                if echo
                    .is_echo(&p.sender.id, &p.sender.login, &p.message_id)
                    .await
                {
                    continue;
                }
                if let Err(e) = writer
                    .append_line(
                        crate::util::from_chrono(p.server_timestamp),
                        &p.sender.login,
                        Some(&p.sender.id),
                        &p.message_text,
                    )
                    .await
                {
                    error!(error = ?e, "transcript append failed");
                }
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => return,
        }
    }
}
