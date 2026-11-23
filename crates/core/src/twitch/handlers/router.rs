use std::sync::Arc;

use tokio::sync::{broadcast, mpsc::UnboundedReceiver};
use tracing::{Span, debug, info, info_span, trace};
use twitch_irc::message::ServerMessage;

use crate::lifecycle::Lifecycle;
use crate::twitch::echo::{ChatOrigin, EchoGuard};

/// One inbound IRC message on the handler broadcast, carrying the root span of
/// its trace.
///
/// The span ends when the last clone drops, i.e. once every handler is done
/// with the message (and any command it started has finished). Keep only
/// `msg` in anything that outlives one handler run, never the `Inbound`.
#[derive(Clone)]
pub struct Inbound {
    pub msg: ServerMessage,
    pub span: Span,
    echo: Arc<EchoGuard>,
}

impl std::fmt::Debug for Inbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inbound")
            .field("msg", &self.msg)
            .field("span", &self.span)
            .finish_non_exhaustive()
    }
}

impl Inbound {
    /// Wrap `msg` in a new `irc.message` root span.
    pub fn new(msg: ServerMessage, echo: Arc<EchoGuard>) -> Self {
        let (channel, message_id, user_id) = trace_identity(&msg);
        let span = info_span!(
            parent: None,
            "irc.message",
            twitch.channel = channel,
            twitch.message.id = message_id,
            twitch.user.id = user_id,
        );
        Self { msg, span, echo }
    }

    /// Who sent this message. Consumers ask the result's named eligibilities
    /// ([`ChatOrigin::records_history`] and friends) instead of querying the
    /// [`EchoGuard`] themselves. Every non-PRIVMSG is [`ChatOrigin::Viewer`].
    ///
    /// Call it at the point of use, not earlier: a Mouth's Helix id is
    /// registered only after its send returns, so a later classification can
    /// still recognize an echo that arrived while the send was in flight.
    pub async fn origin(&self) -> ChatOrigin {
        match &self.msg {
            ServerMessage::Privmsg(p) => {
                self.echo
                    .classify(&p.sender.id, &p.sender.login, &p.message_id)
                    .await
            }
            _ => ChatOrigin::Viewer,
        }
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
    echo: Arc<EchoGuard>,
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
        if matches!(message, ServerMessage::GlobalUserState(_)) {
            info!("IRC connection (re)authenticated");
        }
        let inbound = Inbound::new(message, echo.clone());
        inbound
            .span
            .in_scope(|| trace!(message = ?inbound.msg, "Routing IRC message"));

        // Broadcast to all listeners (ignore errors if no receivers)
        broadcast_tx.send(inbound).ok();
    }
}

#[cfg(test)]
mod tests {
    use twitch_irc::message::IRCMessage;

    use super::*;

    fn privmsg(id: &str) -> ServerMessage {
        let raw = format!(
            "@badge-info=;badges=;color=;display-name=someone;emotes=;id={id};mod=0;room-id=1;subscriber=0;tmi-sent-ts=1700000000000;user-id=999;user-type= :someone!someone@someone.tmi.twitch.tv PRIVMSG #test :hi"
        );
        ServerMessage::try_from(IRCMessage::parse(&raw).expect("valid IRC line"))
            .expect("valid PRIVMSG")
    }

    /// A Mouth's Helix id is registered only after its send returns, so the
    /// echo can be routed first. Classification happens when a consumer asks,
    /// so an id registered after routing is still recognized.
    #[tokio::test]
    async fn origin_sees_an_id_registered_after_routing() {
        let echo = Arc::new(EchoGuard::disabled());
        let inbound = Inbound::new(privmsg("mid1"), echo.clone());
        assert_eq!(inbound.origin().await, ChatOrigin::Viewer);

        echo.register("mid1").await;
        assert_eq!(inbound.origin().await, ChatOrigin::PendingEcho);
    }
}
