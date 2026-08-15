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

    /// Records an outgoing message id as pending so a later `is_echo` call
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

    /// True if `sender_id`/`sender_login` is a known mouth identity, or
    /// `message_id` is a pending outgoing message. Peeks the pending set —
    /// does NOT remove on a hit, since two independent broadcast consumers
    /// (the transcript tap and the command dispatcher) both check the same
    /// message id and both need to see it. TTL expiry is the only removal
    /// path.
    pub async fn is_echo(&self, sender_id: &str, sender_login: &str, message_id: &str) -> bool {
        if self.is_mouth(sender_id, sender_login) {
            return true;
        }
        let mut pending = self.pending.lock().await;
        prune_expired(&mut pending, Instant::now());
        pending.set.contains(message_id)
    }

    /// Identity-only check (no pending-id lookup), for filters that must
    /// only ever match known mouths — commands, the 1337 tracker, emotes —
    /// as opposed to `is_echo`'s broader "don't reprocess our own text"
    /// backstop.
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
    async fn pending_id_is_echo_but_not_removed_on_peek() {
        let guard = EchoGuard::disabled();
        guard.register("mid1").await;
        // Two independent consumers both see it (peek, not take):
        assert!(guard.is_echo("999", "someone", "mid1").await);
        assert!(guard.is_echo("999", "someone", "mid1").await);
    }

    #[tokio::test]
    async fn mouth_identity_is_backstop_for_history_and_only_filter_for_commands() {
        let mut ids = MouthIdentities::default();
        ids.register(
            crate::twitch::mouth::Agent::Aurora,
            "aurora_bot",
            Some("42"),
        );
        let guard = EchoGuard::new(ids);
        assert!(guard.is_echo("42", "aurora_bot", "unknown-id").await); // backstop
        assert!(guard.is_mouth("42", "whatever"));
        assert!(!guard.is_mouth("7", "viewer"));
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
        assert!(guard.is_echo("999", "someone", "mid1").await);
        assert_eq!(guard.agent_for("999", "someone"), None);
    }
}
