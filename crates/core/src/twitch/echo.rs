//! Echo suppression for AI mouths: a pending outgoing-message-id set plus a
//! mouth-identity filter. Kept separate from `mouth.rs` (which only speaks)
//! since this is a distinct responsibility — recognizing our own echoes on
//! the way back in.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use tokio::sync::Mutex;

use crate::twitch::mouth::Agent;

const PENDING_TTL: Duration = Duration::from_secs(30);
const PENDING_CAP: usize = 512;

/// Known login/user-id identities of the bot's own AI mouths.
#[derive(Default)]
pub struct MouthIdentities {
    /// Lowercased logins.
    pub logins: HashSet<String>,
    pub user_ids: HashSet<String>,
    agent_by_login: HashMap<String, Agent>,
    agent_by_user_id: HashMap<String, Agent>,
}

impl MouthIdentities {
    /// Record a mouth under `agent` so replies to that identity can start a
    /// Follow-up Turn for the right speaker. Login is stored lowercased.
    /// `user_id` is optional: production still registers the login when Helix
    /// user-id resolve fails.
    pub fn register(&mut self, agent: Agent, login: &str, user_id: Option<&str>) {
        let login = login.to_ascii_lowercase();
        self.logins.insert(login.clone());
        self.agent_by_login.insert(login, agent);
        if let Some(id) = user_id {
            self.user_ids.insert(id.to_string());
            self.agent_by_user_id.insert(id.to_string(), agent);
        }
    }
}

/// Who an inbound chat message came from, as far as echo suppression is
/// concerned. Each consumer gets it from `Inbound::origin` at its point of
/// use ([`EchoGuard::classify`]).
///
/// The policy lives in the methods below, one per consumer:
///
/// | Consumer | Viewer | PendingEcho | Mouth |
/// |---|---|---|---|
/// | chat history, transcript | yes | no | no |
/// | command dispatch | yes | no | no |
/// | 1337 tracker, link correction | yes | yes | no |
/// | emote usage | recorded | recorded | recorded, flagged |
///
/// The two "no" kinds differ on purpose. History, transcript and commands
/// must never re-read our own output, so a pending Helix id also counts.
/// The tracker, link correction and emote flagging judge the *speaker*, so
/// they use identity only and a stale pending id can never filter a viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatOrigin {
    /// Anyone who is not one of the bot's AI mouths, including every
    /// non-PRIVMSG message.
    Viewer,
    /// One of the bot's AI mouth accounts, by login or user id.
    Mouth,
    /// Not a known mouth identity, but the message id matches a reply a
    /// mouth just sent (the identity backstop missed).
    PendingEcho,
}

impl ChatOrigin {
    fn is_own_output(self) -> bool {
        !matches!(self, Self::Viewer)
    }

    fn is_mouth(self) -> bool {
        matches!(self, Self::Mouth)
    }

    pub fn records_history(self) -> bool {
        !self.is_own_output()
    }

    pub fn records_transcript(self) -> bool {
        !self.is_own_output()
    }

    pub fn dispatches_commands(self) -> bool {
        !self.is_own_output()
    }

    pub fn counts_for_1337(self) -> bool {
        !self.is_mouth()
    }

    pub fn triggers_link_correction(self) -> bool {
        !self.is_mouth()
    }

    /// Mouth emote usage is stored but flagged, like other bots' usage.
    pub fn flags_emote_usage(self) -> bool {
        self.is_mouth()
    }
}

struct PendingIds {
    order: VecDeque<(Instant, String)>,
    set: HashSet<String>,
}

/// Recognizes messages the bot itself sent as it sees them come back over
/// the broadcast channel, so it doesn't treat its own speech as chat input.
pub struct EchoGuard {
    identities: MouthIdentities,
    pending: Mutex<PendingIds>,
}

impl EchoGuard {
    pub fn new(identities: MouthIdentities) -> Self {
        Self {
            identities,
            pending: Mutex::new(PendingIds {
                order: VecDeque::new(),
                set: HashSet::new(),
            }),
        }
    }

    /// No known mouth identities and nothing ever pending — used where echo
    /// suppression isn't wired up (e.g. no AI mouths configured).
    pub fn disabled() -> Self {
        Self::new(MouthIdentities::default())
    }

    /// Records an outgoing message id as pending so a later [`Self::classify`]
    /// recognizes it coming back over the broadcast channel. Prunes
    /// TTL-expired entries and enforces the hard cap before inserting.
    pub async fn register(&self, message_id: &str) {
        let mut pending = self.pending.lock().await;
        let now = Instant::now();
        prune_expired(&mut pending, now);
        while pending.order.len() >= PENDING_CAP {
            if let Some((_, id)) = pending.order.pop_front() {
                pending.set.remove(&id);
            }
        }
        if pending.set.insert(message_id.to_string()) {
            pending.order.push_back((now, message_id.to_string()));
        }
    }

    /// Classify one inbound chat message. Consumers reach this through
    /// `Inbound::origin` and ask the result's named eligibilities instead of
    /// combining identity and pending-id checks themselves.
    ///
    /// Identity wins over the pending id. The pending set is peeked, not
    /// drained: TTL expiry is the only removal path.
    pub async fn classify(
        &self,
        sender_id: &str,
        sender_login: &str,
        message_id: &str,
    ) -> ChatOrigin {
        if self.is_mouth(sender_id, sender_login) {
            return ChatOrigin::Mouth;
        }
        let mut pending = self.pending.lock().await;
        prune_expired(&mut pending, Instant::now());
        if pending.set.contains(message_id) {
            ChatOrigin::PendingEcho
        } else {
            ChatOrigin::Viewer
        }
    }

    /// Identity-only check (no pending-id lookup): is this sender one of the
    /// bot's AI mouth accounts?
    pub fn is_mouth(&self, sender_id: &str, sender_login: &str) -> bool {
        self.identities.user_ids.contains(sender_id)
            || self
                .identities
                .logins
                .contains(&sender_login.to_ascii_lowercase())
    }

    /// Which agent owns this mouth identity, if any. Login/user-id only —
    /// never the pending outgoing-id set, so a Follow-up still resolves after
    /// the 30s echo TTL and a stale pending id can never pick an agent.
    pub fn agent_for(&self, sender_id: &str, sender_login: &str) -> Option<Agent> {
        self.identities
            .agent_by_user_id
            .get(sender_id)
            .copied()
            .or_else(|| {
                self.identities
                    .agent_by_login
                    .get(&sender_login.to_ascii_lowercase())
                    .copied()
            })
    }

    /// Lowercased mouth logins, for the emote deny-list.
    pub fn mouth_logins(&self) -> &HashSet<String> {
        &self.identities.logins
    }
}

fn prune_expired(pending: &mut PendingIds, now: Instant) {
    while pending
        .order
        .front()
        .is_some_and(|(t, _)| now.duration_since(*t) > PENDING_TTL)
    {
        if let Some((_, id)) = pending.order.pop_front() {
            pending.set.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pending_id_is_classified_as_echo_and_not_removed_on_peek() {
        let guard = EchoGuard::disabled();
        guard.register("mid1").await;
        // Peek, not take: a second classification still sees it.
        assert_eq!(
            guard.classify("999", "someone", "mid1").await,
            ChatOrigin::PendingEcho
        );
        assert_eq!(
            guard.classify("999", "someone", "mid1").await,
            ChatOrigin::PendingEcho
        );
        assert_eq!(
            guard.classify("999", "someone", "other").await,
            ChatOrigin::Viewer
        );
    }

    #[tokio::test]
    async fn mouth_identity_wins_over_pending_id() {
        let mut ids = MouthIdentities::default();
        ids.register(
            crate::twitch::mouth::Agent::Aurora,
            "aurora_bot",
            Some("42"),
        );
        let guard = EchoGuard::new(ids);
        guard.register("mid1").await;
        assert_eq!(
            guard.classify("42", "whatever", "mid1").await,
            ChatOrigin::Mouth
        );
        assert_eq!(
            guard.classify("0", "AURORA_BOT", "unknown-id").await,
            ChatOrigin::Mouth
        );
        assert_eq!(
            guard.classify("7", "viewer", "unknown-id").await,
            ChatOrigin::Viewer
        );
    }

    /// Pins the policy table on [`ChatOrigin`]: own output never reaches
    /// history, transcript or commands; only a Mouth identity is filtered
    /// from the 1337 tracker and link correction, and flagged in emote usage.
    #[test]
    fn origin_policy_table() {
        use ChatOrigin::{Mouth, PendingEcho, Viewer};
        type Row = (ChatOrigin, [bool; 6]);
        let rows: [Row; 3] = [
            (Viewer, [true, true, true, true, true, false]),
            (PendingEcho, [false, false, false, true, true, false]),
            (Mouth, [false, false, false, false, false, true]),
        ];
        for (origin, expected) in rows {
            let got = [
                origin.records_history(),
                origin.records_transcript(),
                origin.dispatches_commands(),
                origin.counts_for_1337(),
                origin.triggers_link_correction(),
                origin.flags_emote_usage(),
            ];
            assert_eq!(got, expected, "{origin:?}");
        }
    }

    #[test]
    fn agent_for_maps_aurora_and_grok_by_id_or_login() {
        let mut ids = MouthIdentities::default();
        ids.register(
            crate::twitch::mouth::Agent::Aurora,
            "aurora_bot",
            Some("42"),
        );
        ids.register(crate::twitch::mouth::Agent::Grok, "grok_bot", Some("77"));
        let guard = EchoGuard::new(ids);

        assert_eq!(
            guard.agent_for("42", "whatever"),
            Some(crate::twitch::mouth::Agent::Aurora)
        );
        assert_eq!(
            guard.agent_for("999", "GROK_BOT"),
            Some(crate::twitch::mouth::Agent::Grok)
        );
        assert_eq!(guard.agent_for("7", "viewer"), None);
    }

    #[tokio::test]
    async fn agent_for_does_not_use_pending_message_ids() {
        let guard = EchoGuard::disabled();
        guard.register("mid1").await;
        assert_eq!(
            guard.classify("999", "someone", "mid1").await,
            ChatOrigin::PendingEcho
        );
        assert_eq!(guard.agent_for("999", "someone"), None);
    }
}
