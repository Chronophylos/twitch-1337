//! In-memory session table.
//!
//! Sessions sit behind a `RwLock<HashMap>` keyed by a 64-hex-char random
//! id. TTL is sliding: every successful `get_and_touch` bumps `last_seen`,
//! so an active user stays logged in indefinitely. The role-gate middleware
//! also stamps `last_role_check` so it knows when to re-verify the role.
//!
//! The TTL is **not** stored in the table — callers pass it on each
//! `get_and_touch` call. This allows the session TTL to be read live from
//! the settings handle so dashboard changes take effect on subsequent
//! requests without a restart.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use eyre::Result;
use jiff::{SignedDuration, Timestamp};
use parking_lot::RwLock;
use rand::Rng as _;
use serde::{Deserialize, Serialize};
use twitch_1337_core::util::persist::atomic_save_ron_private_async;

use crate::auth::role::Role;
use crate::clock::Clock;

pub type SessionId = String;

/// Inputs for [`SessionTable::insert`]. Named fields keep the call sites
/// readable instead of relying on positional `String, String, Role, Option<String>`.
pub struct NewSession {
    pub user_id: String,
    pub user_login: String,
    pub role: Role,
    pub avatar_url: Option<String>,
    pub is_broadcaster: bool,
    /// Twitch refresh token from the login. Only Mod sessions keep one: the
    /// role recheck uses it to ask Twitch, as the user, whether they still
    /// moderate the channel.
    pub refresh_token: Option<TwitchRefreshToken>,
}

/// A user's Twitch OAuth refresh token (scopes `user:read:email`,
/// `user:read:moderated_channels`). Persisted with the session in
/// `sessions.ron`; `Debug` is redacted so it never reaches a log line.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TwitchRefreshToken(String);

impl TwitchRefreshToken {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for TwitchRefreshToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TwitchRefreshToken(<redacted>)")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub user_id: String,
    pub user_login: String,
    pub role: Role,
    pub issued_at: Timestamp,
    pub last_seen: Timestamp,
    pub last_role_check: Timestamp,
    pub csrf_value: [u8; 32],
    /// Twitch helix `profile_image_url` captured at login. Static for the
    /// session lifetime; sidebar reads this without per-request helix calls.
    pub avatar_url: Option<String>,
    /// True when `user_id` matches the configured broadcaster. Captured at
    /// session creation so role badges and broadcaster-only UI bits don't
    /// need to re-read app state.
    pub is_broadcaster: bool,
    /// See [`NewSession::refresh_token`]. Absent in sessions persisted before
    /// it existed; such a Mod session fails its next recheck and must log in
    /// again.
    #[serde(default)]
    pub refresh_token: Option<TwitchRefreshToken>,
}

impl Session {
    pub fn is_mod(&self) -> bool {
        // Owner is a strict superset of Mod — anywhere we ask "is this user
        // at least a moderator?" Owner must answer true.
        self.role >= Role::Mod
    }
}

pub struct SessionTable {
    inner: RwLock<HashMap<SessionId, Session>>,
    clock: Arc<dyn Clock>,
    /// Backing file under `$DATA_DIR`. `None` keeps the table purely
    /// in-memory (tests, web-dev bin) so they never touch disk.
    path: Option<PathBuf>,
    /// Serializes [`persist`](Self::persist) calls. `persist` is invoked from
    /// three concurrent contexts (login, logout, the periodic snapshot task)
    /// and `atomic_save_ron_private_async` always writes the same `.ron.tmp`
    /// sibling — overlapping writes would tear that temp file. Holding this
    /// across the whole snapshot+write makes the last writer win cleanly.
    write_lock: tokio::sync::Mutex<()>,
    /// Serializes role rechecks. A Mod recheck spends the session's refresh
    /// token; two concurrent requests must not both spend it, or a rotated
    /// token would fail the second one and drop a valid session.
    recheck_lock: tokio::sync::Mutex<()>,
}

impl SessionTable {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            clock,
            path: None,
            write_lock: tokio::sync::Mutex::new(()),
            recheck_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Construct a disk-backed table, loading any previously persisted
    /// sessions so a bot restart (every rolling deploy) does not log everyone
    /// out — the signed sid cookie still matches a live table entry.
    ///
    /// A missing file starts empty; a corrupt/unparseable file is logged and
    /// also starts empty, so a bad file costs at most one forced re-login
    /// instead of wedging startup.
    pub fn load(clock: Arc<dyn Clock>, path: PathBuf) -> Self {
        let mut inner = match std::fs::read_to_string(&path) {
            Ok(data) => match ron::from_str::<HashMap<SessionId, Session>>(&data) {
                Ok(map) => map,
                Err(error) => {
                    tracing::warn!(?error, path = %path.display(), "Failed to parse sessions; starting empty");
                    HashMap::new()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => {
                tracing::warn!(?error, path = %path.display(), "Failed to read sessions; starting empty");
                HashMap::new()
            }
        };
        // Force a fresh helix role re-check on the first request of every
        // restored session. Without this a moderator demoted while the bot was
        // down would keep dashboard access until `mod_check_refresh_secs`
        // elapsed — the pre-persistence restart wiped sessions and re-evaluated
        // roles at the forced re-login, and we must not regress that.
        for s in inner.values_mut() {
            s.last_role_check = Timestamp::MIN;
        }
        Self {
            inner: RwLock::new(inner),
            clock,
            path: Some(path),
            write_lock: tokio::sync::Mutex::new(()),
            recheck_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Snapshot the table and atomically write it to disk. No-op when the
    /// table has no backing path. The file carries Mod refresh tokens, so it
    /// is written owner-only (`0600`).
    ///
    /// The whole snapshot+write runs under `write_lock` so concurrent callers
    /// (login, logout, the periodic task) can't tear the shared `.ron.tmp`.
    /// The snapshot is a quick clone under the `inner` read lock, which is
    /// released before the disk `await`, so only the (already serialized)
    /// writers wait — readers never block on IO.
    pub async fn persist(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let _write = self.write_lock.lock().await;
        let snapshot = self.inner.read().clone();
        if let Err(error) = atomic_save_ron_private_async(&snapshot, &path).await {
            tracing::warn!(?error, "Failed to persist sessions");
        }
    }

    /// Returns the new session id together with the freshly-generated csrf
    /// value so the OAuth callback can set both cookies without a second
    /// lookup against the table.
    pub fn insert(&self, new: NewSession) -> Result<(SessionId, [u8; 32])> {
        let mut rng = rand::rng();
        let mut id_bytes = [0u8; 32];
        rng.fill_bytes(&mut id_bytes);
        let mut csrf = [0u8; 32];
        rng.fill_bytes(&mut csrf);
        let id = hex::encode(id_bytes);
        self.insert_at(&id, csrf, new);
        Ok((id, csrf))
    }

    /// Insert a session at a caller-chosen id with a caller-chosen csrf.
    /// Dev-only: the web-dev bin and `/_dev/login` use this to seed a
    /// deterministic session that survives server restarts.
    #[cfg(feature = "dev-login")]
    pub fn insert_with_id(&self, id: &str, csrf: [u8; 32], new: NewSession) {
        self.insert_at(id, csrf, new);
    }

    fn insert_at(&self, id: &str, csrf: [u8; 32], new: NewSession) {
        let now = self.clock.now();
        self.inner.write().insert(
            id.to_owned(),
            Session {
                user_id: new.user_id,
                user_login: new.user_login,
                role: new.role,
                issued_at: now,
                last_seen: now,
                last_role_check: now,
                csrf_value: csrf,
                avatar_url: new.avatar_url,
                is_broadcaster: new.is_broadcaster,
                refresh_token: new.refresh_token,
            },
        );
    }

    /// Check whether the session with `id` is still live under `ttl` and, if
    /// so, slide its `last_seen` timestamp and return a clone.
    ///
    /// `ttl` is passed by the caller on every call so the value is always the
    /// **current** effective setting — dashboard changes to `session_ttl_secs`
    /// take effect on the next request without a restart.
    pub fn get_and_touch(&self, id: &str, ttl: Duration) -> Option<Session> {
        let now = self.clock.now();
        let ttl = SignedDuration::try_from(ttl).ok()?;
        let mut g = self.inner.write();
        let session = g.get_mut(id)?;
        if now.duration_since(session.last_seen) > ttl {
            g.remove(id);
            return None;
        }
        session.last_seen = now;
        Some(session.clone())
    }

    /// Clone of the session with `id`, without sliding its TTL.
    pub fn get(&self, id: &str) -> Option<Session> {
        self.inner.read().get(id).cloned()
    }

    pub fn drop_session(&self, id: &str) {
        self.inner.write().remove(id);
    }

    /// Stamp a passed role recheck. `rotated` replaces the stored refresh
    /// token when Twitch issued a new one during the recheck.
    pub fn record_role_check(&self, id: &str, rotated: Option<TwitchRefreshToken>) {
        let now = self.clock.now();
        if let Some(s) = self.inner.write().get_mut(id) {
            s.last_role_check = now;
            if let Some(token) = rotated {
                s.refresh_token = Some(token);
            }
        }
    }

    /// Held across one role recheck; see the `recheck_lock` field.
    pub async fn lock_recheck(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.recheck_lock.lock().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Manually-advanced clock so TTL behaviour is deterministic.
    struct TestClock {
        now: Mutex<Timestamp>,
    }

    impl TestClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                now: Mutex::new(Timestamp::UNIX_EPOCH),
            })
        }
        fn advance(&self, by: Duration) {
            let mut g = self.now.lock().unwrap();
            *g += SignedDuration::try_from(by).unwrap();
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> Timestamp {
            *self.now.lock().unwrap()
        }
    }

    fn sample_session() -> NewSession {
        NewSession {
            user_id: "u1".to_owned(),
            user_login: "login1".to_owned(),
            role: Role::Mod,
            avatar_url: None,
            is_broadcaster: false,
            refresh_token: None,
        }
    }

    const TTL: Duration = Duration::from_secs(3600);

    #[test]
    fn get_and_touch_returns_live_session() {
        let clock = TestClock::new();
        let table = SessionTable::new(clock);
        table.insert_at("sid", [0u8; 32], sample_session());

        let got = table.get_and_touch("sid", TTL).expect("session is live");
        assert_eq!(got.user_id, "u1");
        assert_eq!(got.role, Role::Mod);
    }

    #[test]
    fn get_and_touch_unknown_id_is_none() {
        let table = SessionTable::new(TestClock::new());
        assert!(table.get_and_touch("missing", TTL).is_none());
    }

    #[test]
    fn expired_session_is_evicted() {
        let clock = TestClock::new();
        let table = SessionTable::new(clock.clone());
        table.insert_at("sid", [0u8; 32], sample_session());

        clock.advance(TTL + Duration::from_secs(1));
        assert!(
            table.get_and_touch("sid", TTL).is_none(),
            "session past ttl must not validate"
        );
        // Prove the expired lookup *evicted* the entry rather than merely
        // rejecting it: a still-present entry (last_seen at epoch) would
        // validate against a 10-year ttl, but an evicted one stays gone.
        let huge_ttl = Duration::from_secs(10 * 365 * 24 * 3600);
        assert!(
            table.get_and_touch("sid", huge_ttl).is_none(),
            "expired session must be removed from the table, not just rejected"
        );
    }

    #[test]
    fn activity_slides_the_window() {
        let clock = TestClock::new();
        let table = SessionTable::new(clock.clone());
        table.insert_at("sid", [0u8; 32], sample_session());

        // Repeatedly touch just under the ttl; total elapsed exceeds one ttl
        // but the session stays live because last_seen keeps moving forward.
        for _ in 0..5 {
            clock.advance(TTL - Duration::from_secs(1));
            assert!(
                table.get_and_touch("sid", TTL).is_some(),
                "sliding window should keep an active session alive"
            );
        }
    }

    #[test]
    fn drop_session_removes_entry() {
        let table = SessionTable::new(TestClock::new());
        table.insert_at("sid", [0u8; 32], sample_session());
        table.drop_session("sid");
        assert!(table.get_and_touch("sid", TTL).is_none());
    }

    #[test]
    fn record_role_check_stamps_current_time() {
        let clock = TestClock::new();
        let table = SessionTable::new(clock.clone());
        table.insert_at("sid", [0u8; 32], sample_session());

        clock.advance(Duration::from_secs(120));
        table.record_role_check("sid", None);

        let session = table.get_and_touch("sid", TTL).unwrap();
        assert_eq!(
            session.last_role_check,
            Timestamp::UNIX_EPOCH + SignedDuration::from_secs(120)
        );
    }

    #[test]
    fn is_mod_tracks_role_tier() {
        let mk = |role| Session {
            user_id: "u".into(),
            user_login: "l".into(),
            role,
            issued_at: Timestamp::UNIX_EPOCH,
            last_seen: Timestamp::UNIX_EPOCH,
            last_role_check: Timestamp::UNIX_EPOCH,
            csrf_value: [0u8; 32],
            avatar_url: None,
            is_broadcaster: false,
            refresh_token: None,
        };
        assert!(!mk(Role::Viewer).is_mod());
        assert!(mk(Role::Mod).is_mod());
        assert!(mk(Role::Owner).is_mod(), "owner is a superset of mod");
    }

    /// `sessions.ron` carries Mod refresh tokens, so it must be owner-only,
    /// also when an older build left it world-readable.
    #[cfg(unix)]
    #[tokio::test]
    async fn persist_writes_an_owner_only_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.ron");
        #[expect(clippy::disallowed_methods, reason = "test seeds a 0644 sessions file")]
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let table = SessionTable::load(TestClock::new(), path.clone());
        table.insert_at("sid", [0u8; 32], sample_session());
        table.persist().await;

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
        let reloaded = SessionTable::load(TestClock::new(), path);
        assert_eq!(reloaded.get("sid").unwrap().user_id, "u1");
    }

    /// `sessions.ron` written by the pre-jiff (chrono) build must keep loading.
    #[test]
    fn persisted_session_timestamps_load() {
        let csrf = vec!["7"; 32].join(",");
        let fixture = format!(
            r#"(user_id: "1", user_login: "alice", role: Mod,
                issued_at: "2026-07-19T12:00:00Z",
                last_seen: "2026-07-19T12:05:00.250Z",
                last_role_check: "2026-07-19T12:01:00.123456789Z",
                csrf_value: ({csrf}), avatar_url: None, is_broadcaster: false)"#
        );
        let s: Session = ron::from_str(&fixture).expect("old session must load");
        let out = ron::to_string(&s).expect("serialize");
        assert!(out.contains(r#"issued_at:"2026-07-19T12:00:00Z""#), "{out}");
        assert!(
            out.contains(r#"last_role_check:"2026-07-19T12:01:00.123456789Z""#),
            "{out}"
        );
    }
}
