use tokio::sync::{broadcast, mpsc::UnboundedReceiver};
use tracing::{Span, debug, info, info_span, trace};
use twitch_irc::message::ServerMessage;

use crate::lifecycle::Lifecycle;

/// One inbound IRC message on the handler broadcast, carrying the root span of
/// its trace.
///
/// The span ends when the last clone drops, i.e. once every handler is done
/// with the message (and any command it started has finished). Keep only
/// `msg` in anything that outlives one handler run, never the `Inbound`.
#[derive(Clone, Debug)]
pub struct Inbound {
    pub msg: ServerMessage,
    pub span: Span,
}

impl Inbound {
    /// Wrap `msg` in a new `irc.message` root span.
    pub fn new(msg: ServerMessage) -> Self {
        let (channel, message_id, user_id) = trace_identity(&msg);
        let span = info_span!(
            parent: None,
            "irc.message",
            twitch.channel = channel,
            twitch.message.id = message_id,
            twitch.user.id = user_id,
        );
        Self { msg, span }
    }

    /// The child span of a handler whose filter matched this message. Open it
    /// only after the match: an ignored message leaves no handler span.
    /// `handler` is a metric label; pass the handler's fixed snake_case name.
    pub fn handler_span(&self, handler: &'static str) -> Span {
        info_span!(parent: &self.span, "handler", bot.handler = handler)
    }
}

/// `(twitch.channel, twitch.message.id, twitch.user.id)` of a message; `None`
/// where the message kind lacks the field.
fn trace_identity(msg: &ServerMessage) -> (Option<&str>, Option<&str>, Option<&str>) {
    let (channel, message_id, user_id) = match msg {
        ServerMessage::Privmsg(m) => (
            Some(m.channel_login.as_str()),
            Some(m.message_id.as_str()),
            Some(m.sender.id.as_str()),
        ),
        ServerMessage::UserNotice(m) => (
            Some(m.channel_login.as_str()),
            Some(m.message_id.as_str()),
            Some(m.sender.id.as_str()),
        ),
        ServerMessage::ClearMsg(m) => (
            Some(m.channel_login.as_str()),
            Some(m.message_id.as_str()),
            None,
        ),
        ServerMessage::ClearChat(m) => (Some(m.channel_login.as_str()), None, None),
        ServerMessage::Join(m) => (Some(m.channel_login.as_str()), None, None),
        ServerMessage::Part(m) => (Some(m.channel_login.as_str()), None, None),
        ServerMessage::RoomState(m) => (Some(m.channel_login.as_str()), None, None),
        ServerMessage::UserState(m) => (Some(m.channel_login.as_str()), None, None),
        ServerMessage::Notice(m) => (m.channel_login.as_deref(), None, None),
        ServerMessage::Whisper(m) => (None, None, Some(m.sender.id.as_str())),
        _ => (None, None, None),
    };
    fn present(value: Option<&str>) -> Option<&str> {
        value.filter(|v| !v.is_empty())
    }
    (present(channel), present(message_id), present(user_id))
}

/// Message router task that broadcasts incoming IRC messages to all handlers.
///
/// Reads from the twitch-irc receiver and broadcasts to all subscribed handlers,
/// each message wrapped in its own [`Inbound`] trace.
/// Exits when the incoming_messages channel is closed.
pub async fn run_message_router(
    mut incoming_messages: UnboundedReceiver<ServerMessage>,
    broadcast_tx: broadcast::Sender<Inbound>,
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
        let inbound = Inbound::new(message);
        inbound
            .span
            .in_scope(|| trace!(message = ?inbound.msg, "Routing IRC message"));

        // Broadcast to all listeners (ignore errors if no receivers)
        broadcast_tx.send(inbound).ok();
    }
}
