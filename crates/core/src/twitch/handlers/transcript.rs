//! Append every PRIVMSG (and the bot's own replies, see ai/command.rs) to the
//! line-buffered `transcripts/today.md`. Best-effort; errors only logged.

use std::sync::Arc;

use tokio::sync::broadcast;
use tracing::Instrument as _;
use twitch_irc::message::ServerMessage;

use super::Inbound;
use crate::ai::memory::transcript::TranscriptWriter;
use crate::lifecycle::Lifecycle;
use crate::twitch::echo::EchoGuard;

pub async fn run_transcript_tap(
    mut rx: broadcast::Receiver<Inbound>,
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
        let inbound = match message {
            Ok(inbound) => inbound,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return,
        };
        let ServerMessage::Privmsg(p) = &inbound.msg else {
            continue;
        };
        if p.channel_login != channel
            || echo
                .is_echo(&p.sender.id, &p.sender.login, &p.message_id)
                .await
        {
            continue;
        }
        async {
            if let Err(e) = writer
                .append_line(
                    p.server_timestamp,
                    &p.sender.login,
                    Some(&p.sender.id),
                    &p.message_text,
                )
                .await
            {
                crate::log_handled!(error, e, "transcript append failed");
            }
        }
        .instrument(inbound.handler_span("transcript"))
        .await;
    }
}
