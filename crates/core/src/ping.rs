//! Ping groups: named sets of chat members a viewer can mention at once with
//! `!<name>`, persisted to `$DATA_DIR/pings.ron`.
//!
//! [`PingHandle`] is the one shared entry point (chat commands, the scheduler,
//! the dashboard). It serializes every read and write through one async mutex
//! around the store, so a check-then-write such as a trigger's cooldown check
//! and fire record is atomic. Cooldowns are in memory only (ADR-0017).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eyre::{Result, WrapErr};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use tracing::{debug, info, warn};

const PINGS_FILENAME: &str = "pings.ron";

/// Why a ping mutation failed. `Display` is neutral English for logs; each
/// Surface (chat, dashboard) words the variants itself (ADR-0012).
#[derive(Debug, thiserror::Error)]
pub enum PingError {
    #[error("ping {name:?} not found")]
    NotFound { name: String },
    #[error("ping {name:?} already exists")]
    AlreadyExists { name: String },
    #[error("ping name {name:?} is a built-in command")]
    ReservedCommand { name: String },
    #[error("ping name is empty")]
    EmptyName,
    #[error("ping name contains characters other than [a-z0-9_-]")]
    InvalidName,
    #[error("ping template contains control characters")]
    ControlChars,
    /// Not a Twitch login: members end up in chat mentions and in dashboard
    /// URL paths, so only the login charset is stored.
    #[error("member {user:?} is not a valid Twitch login")]
    InvalidMember { user: String },
    #[error("{user} is already a member of ping {ping:?}")]
    AlreadyMember { ping: String, user: String },
    #[error("{user} is not a member of ping {ping:?}")]
    NotMember { ping: String, user: String },
    #[error("failed to persist pings")]
    Persist(#[source] eyre::Report),
}

/// Twitch login: lowercase ASCII alphanumeric + underscore, 1–25 chars
/// (Twitch enforces that charset and length on signup).
pub fn is_valid_twitch_login(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 25
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ping {
    pub template: String,
    /// Sorted, so a fire that spans several messages splits in a stable order.
    pub members: BTreeSet<String>,
    /// Per-ping cooldown in seconds, overriding `pings.cooldown`. Read from
    /// `pings.ron` only; neither chat nor the dashboard sets it.
    pub cooldown: Option<u64>,
    pub created_by: String,
    #[serde(default)]
    pub last_fired_at: Option<Timestamp>,
    #[serde(default)]
    pub fire_count: u64,
}

#[derive(Debug, Clone)]
pub struct PingView {
    pub name: String,
    pub template: String,
    pub members: BTreeSet<String>,
    pub cooldown: Option<u64>,
    pub created_by: String,
    pub last_fired_at: Option<Timestamp>,
    pub fire_count: u64,
}

impl PingView {
    fn new(name: &str, p: &Ping) -> Self {
        Self {
            name: name.to_owned(),
            template: p.template.clone(),
            members: p.members.clone(),
            cooldown: p.cooldown,
            created_by: p.created_by.clone(),
            last_fired_at: p.last_fired_at,
            fire_count: p.fire_count,
        }
    }
}

#[derive(Debug)]
pub enum TriggerDecision {
    Skip,
    OnCooldown(Duration),
    /// One or more chat lines, each within the outbound limit: a large group
    /// spreads its mentions over several messages.
    Fire(Vec<String>),
}

/// Shared access to the ping store. Cheap to clone.
#[derive(Clone)]
pub struct PingHandle {
    manager: Arc<Mutex<PingManager>>,
    /// Ping names, republished after every create and delete, so
    /// `PingTriggerCommand::matches` (synchronous) can test a chat word
    /// without locking the store.
    names: Arc<watch::Sender<HashSet<String>>>,
}

impl PingHandle {
    /// Load `$DATA_DIR/pings.ron`. A missing file starts empty; an unreadable
    /// or unparseable one is an error, because starting empty would overwrite
    /// every ping with the next save.
    pub fn load(data_dir: &Path) -> Result<Self> {
        let manager = PingManager::load(data_dir)?;
        let (names, _) = watch::channel(manager.names());
        Ok(Self {
            manager: Arc::new(Mutex::new(manager)),
            names: Arc::new(names),
        })
    }

    /// Live set of ping names.
    pub fn names(&self) -> watch::Receiver<HashSet<String>> {
        self.names.subscribe()
    }

    pub async fn create_ping(
        &self,
        name: String,
        template: String,
        created_by: String,
    ) -> Result<(), PingError> {
        let mut manager = self.manager.lock().await;
        manager.create_ping(&name, template, created_by).await?;
        self.names.send_replace(manager.names());
        Ok(())
    }

    pub async fn delete_ping(&self, name: String) -> Result<(), PingError> {
        let mut manager = self.manager.lock().await;
        manager.delete_ping(&name).await?;
        self.names.send_replace(manager.names());
        Ok(())
    }

    pub async fn edit_template(&self, name: String, template: String) -> Result<(), PingError> {
        self.manager
            .lock()
            .await
            .edit_template(&name, template)
            .await
    }

    pub async fn add_member(&self, ping_name: String, username: String) -> Result<(), PingError> {
        self.manager
            .lock()
            .await
            .add_member(&ping_name, &username)
            .await
    }

    pub async fn remove_member(
        &self,
        ping_name: String,
        username: String,
    ) -> Result<(), PingError> {
        self.manager
            .lock()
            .await
            .remove_member(&ping_name, &username)
            .await
    }

    pub async fn try_record_trigger(
        &self,
        ping_name: String,
        sender: String,
        default_cooldown: Duration,
        public: bool,
    ) -> TriggerDecision {
        self.manager
            .lock()
            .await
            .try_record_trigger(&ping_name, &sender, default_cooldown, public)
            .await
    }

    pub async fn snapshot(&self) -> Vec<PingView> {
        let manager = self.manager.lock().await;
        manager
            .store
            .pings
            .iter()
            .map(|(name, ping)| PingView::new(name, ping))
            .collect()
    }

    pub async fn get_one(&self, name: String) -> Option<PingView> {
        let name = ping_key(&name);
        let manager = self.manager.lock().await;
        manager
            .store
            .pings
            .get(&name)
            .map(|ping| PingView::new(&name, ping))
    }

    pub async fn list_for_user(&self, username: String) -> Vec<String> {
        self.manager.lock().await.list_pings_for_user(&username)
    }

    pub async fn is_member(&self, ping_name: String, username: String) -> bool {
        self.manager
            .lock()
            .await
            .is_member(&ping_key(&ping_name), &username)
    }
}

/// The one normalization of a ping name: every lookup and create goes
/// through it, so `!Kaffee`, `!p join KAFFEE` and the dashboard all reach
/// `kaffee`.
fn ping_key(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn validate_template(template: &str) -> Result<(), PingError> {
    if template.chars().any(char::is_control) {
        return Err(PingError::ControlChars);
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), PingError> {
    if name.is_empty() {
        return Err(PingError::EmptyName);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(PingError::InvalidName);
    }
    Ok(())
}

fn not_found(name: &str) -> PingError {
    PingError::NotFound {
        name: name.to_owned(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PingStore {
    pings: HashMap<String, Ping>,
}

struct PingManager {
    store: PingStore,
    last_triggered: HashMap<String, Instant>,
    path: PathBuf,
}

impl PingManager {
    fn load(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(PINGS_FILENAME);
        let pings = match std::fs::read_to_string(&path) {
            Ok(data) => {
                ron::from_str::<PingStore>(&data)
                    .wrap_err("Failed to parse pings.ron")?
                    .pings
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                info!("No pings.ron found, starting with empty ping store");
                HashMap::new()
            }
            Err(error) => return Err(error).wrap_err("Failed to read pings.ron"),
        };
        let store = PingStore {
            pings: sanitize_loaded(pings),
        };
        info!(count = store.pings.len(), "Loaded pings");
        Ok(Self {
            store,
            last_triggered: HashMap::new(),
            path,
        })
    }

    fn names(&self) -> HashSet<String> {
        self.store.pings.keys().cloned().collect()
    }

    async fn save(&self) -> Result<()> {
        crate::util::persist::atomic_save_ron_async(&self.store, &self.path)
            .await
            .wrap_err("Failed to save pings.ron")?;
        debug!("Saved pings to disk");
        Ok(())
    }

    /// Save, or roll the in-memory store back to `before` so a failed write
    /// never leaves a ping that the command reported as failed live until
    /// restart.
    // ponytail: clones the whole store per edit; fine at chat-ping scale.
    async fn save_or_revert(&mut self, before: PingStore) -> Result<(), PingError> {
        let result = self.save().await;
        if result.is_err() {
            self.store = before;
        }
        result.map_err(PingError::Persist)
    }

    async fn create_ping(
        &mut self,
        name: &str,
        template: String,
        created_by: String,
    ) -> Result<(), PingError> {
        let before = self.store.clone();
        let name = ping_key(name);
        validate_name(&name)?;
        // A ping named after a built-in command never fires: the dispatcher
        // scans commands in order and the ping catch-all is last, so the
        // command always wins. Reject it here rather than letting it sit in
        // pings.ron looking registered but silently dead.
        if crate::commands::is_reserved_ping_name(&name) {
            return Err(PingError::ReservedCommand { name });
        }
        validate_template(&template)?;
        if self.store.pings.contains_key(&name) {
            return Err(PingError::AlreadyExists { name });
        }
        self.store.pings.insert(
            name,
            Ping {
                template,
                members: BTreeSet::new(),
                cooldown: None,
                created_by,
                last_fired_at: None,
                fire_count: 0,
            },
        );
        self.save_or_revert(before).await
    }

    async fn delete_ping(&mut self, name: &str) -> Result<(), PingError> {
        let before = self.store.clone();
        let name = ping_key(name);
        if self.store.pings.remove(&name).is_none() {
            return Err(not_found(&name));
        }
        self.last_triggered.remove(&name);
        self.save_or_revert(before).await
    }

    async fn edit_template(&mut self, name: &str, template: String) -> Result<(), PingError> {
        let before = self.store.clone();
        validate_template(&template)?;
        let name = ping_key(name);
        let ping = self
            .store
            .pings
            .get_mut(&name)
            .ok_or_else(|| not_found(&name))?;
        ping.template = template;
        self.save_or_revert(before).await
    }

    async fn add_member(&mut self, ping_name: &str, username: &str) -> Result<(), PingError> {
        let before = self.store.clone();
        let ping_name = ping_key(ping_name);
        let ping = self
            .store
            .pings
            .get_mut(&ping_name)
            .ok_or_else(|| not_found(&ping_name))?;
        let login = username.to_lowercase();
        if !is_valid_twitch_login(&login) {
            return Err(PingError::InvalidMember {
                user: username.to_owned(),
            });
        }
        if !ping.members.insert(login) {
            return Err(PingError::AlreadyMember {
                ping: ping_name,
                user: username.to_owned(),
            });
        }
        self.save_or_revert(before).await
    }

    async fn remove_member(&mut self, ping_name: &str, username: &str) -> Result<(), PingError> {
        let before = self.store.clone();
        let ping_name = ping_key(ping_name);
        let ping = self
            .store
            .pings
            .get_mut(&ping_name)
            .ok_or_else(|| not_found(&ping_name))?;
        if !ping.members.remove(&username.to_lowercase()) {
            return Err(PingError::NotMember {
                ping: ping_name,
                user: username.to_owned(),
            });
        }
        self.save_or_revert(before).await
    }

    fn is_member(&self, ping_name: &str, username: &str) -> bool {
        self.store
            .pings
            .get(ping_name)
            .is_some_and(|p| p.members.contains(username))
    }

    fn list_pings_for_user(&self, username: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .store
            .pings
            .iter()
            .filter(|(_, p)| p.members.contains(username))
            .map(|(name, _)| name.clone())
            .collect();
        names.sort_unstable();
        names
    }

    fn remaining_cooldown(&self, ping_name: &str, default_cooldown: Duration) -> Option<Duration> {
        let ping = self.store.pings.get(ping_name)?;
        let cooldown = ping.cooldown.map_or(default_cooldown, Duration::from_secs);
        let elapsed = self.last_triggered.get(ping_name)?.elapsed();
        (elapsed < cooldown).then(|| cooldown - elapsed)
    }

    async fn record_trigger(&mut self, ping_name: &str) -> Result<()> {
        let Some(ping) = self.store.pings.get_mut(ping_name) else {
            return Ok(());
        };
        self.last_triggered
            .insert(ping_name.to_owned(), Instant::now());
        ping.last_fired_at = Some(Timestamp::now());
        ping.fire_count = ping.fire_count.saturating_add(1);
        self.save().await
    }

    /// Membership check (unless `public`), cooldown, render and record as one
    /// step, so two concurrent triggers can never both fire.
    async fn try_record_trigger(
        &mut self,
        ping_name: &str,
        sender: &str,
        default_cooldown: Duration,
        public: bool,
    ) -> TriggerDecision {
        let ping_name = ping_key(ping_name);
        if !public && !self.is_member(&ping_name, sender) {
            return TriggerDecision::Skip;
        }
        if let Some(remaining) = self.remaining_cooldown(&ping_name, default_cooldown) {
            return TriggerDecision::OnCooldown(remaining);
        }
        let Some(lines) = self.render_template(&ping_name, sender) else {
            return TriggerDecision::Skip;
        };
        if let Err(e) = self.record_trigger(&ping_name).await {
            crate::log_handled!(warn, e, ping = %ping_name, "Failed to persist fire stats");
        }
        TriggerDecision::Fire(lines)
    }

    /// Chat lines for one fire: the template with as many mentions as fit one
    /// message, then the remaining mentions on as many further lines as
    /// needed. One line would be cut at the outbound limit, silently dropping
    /// every member past it.
    fn render_template(&self, ping_name: &str, sender: &str) -> Option<Vec<String>> {
        let ping = self.store.pings.get(ping_name)?;
        let sender_in_template = ping.template.contains("{sender}");
        let mentions: Vec<String> = ping
            .members
            .iter()
            .filter(|m| !sender_in_template || m.as_str() != sender)
            .map(|m| format!("@{m}"))
            .collect();
        if mentions.is_empty() {
            return None;
        }
        let sender = format!("@{sender}");
        let fill = |chunk: &str| {
            ping.template
                .replace("{mentions}", chunk)
                .replace("{sender}", &sender)
        };
        let (first, mut rest) = take_fitting(&mentions, |chunk| {
            crate::twitch::chat_sender::fits_outbound(&fill(chunk))
        });
        let mut lines = vec![fill(&first)];
        while !rest.is_empty() {
            let (chunk, tail) = take_fitting(rest, crate::twitch::chat_sender::fits_outbound);
            lines.push(chunk);
            rest = tail;
        }
        Some(lines)
    }
}

/// The longest prefix of `mentions`, joined by spaces, that `fits`, and the
/// rest. Takes at least one mention, so a caller looping on the rest always
/// makes progress even when a single mention does not fit.
fn take_fitting(mentions: &[String], fits: impl Fn(&str) -> bool) -> (String, &[String]) {
    let Some((head, tail)) = mentions.split_first() else {
        return (String::new(), mentions);
    };
    let mut chunk = head.clone();
    let mut taken = 1;
    for mention in tail {
        let candidate = format!("{chunk} {mention}");
        if !fits(&candidate) {
            break;
        }
        chunk = candidate;
        taken += 1;
    }
    (chunk, mentions.get(taken..).unwrap_or_default())
}

/// Apply the create/add rules to a file that may predate them: names are
/// lowercased (the trigger lowercases before lookup, so an uppercase key
/// never fired), and pings with an invalid name or template and members
/// that are not Twitch logins (they end up in mentions and dashboard URL
/// paths) are dropped with a warning. On a lowercase collision the key that
/// was already lowercase wins.
fn sanitize_loaded(pings: HashMap<String, Ping>) -> HashMap<String, Ping> {
    let mut entries: Vec<(String, Ping)> = pings.into_iter().collect();
    entries.sort_by_key(|(name, _)| *name != name.to_ascii_lowercase());
    let mut out = HashMap::with_capacity(entries.len());
    for (raw_name, mut ping) in entries {
        let name = raw_name.to_ascii_lowercase();
        if let Err(error) = validate_name(&name).and_then(|()| validate_template(&ping.template)) {
            crate::log_handled!(warn, error, ping = %raw_name, "Dropping invalid ping on load");
            continue;
        }
        if out.contains_key(&name) {
            warn!(ping = %raw_name, "Dropping ping whose lowercased name is taken on load");
            continue;
        }
        ping.members = std::mem::take(&mut ping.members)
            .into_iter()
            .filter_map(|member| {
                let login = member.to_ascii_lowercase();
                if is_valid_twitch_login(&login) {
                    Some(login)
                } else {
                    warn!(ping = %name, member = %member, "Dropping invalid member on load");
                    None
                }
            })
            .collect();
        out.insert(name, ping);
    }
    out
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "test fixtures seed files directly"
)]
mod tests {
    use super::*;

    fn empty_manager(dir: &Path) -> PingManager {
        PingManager {
            store: PingStore {
                pings: HashMap::new(),
            },
            last_triggered: HashMap::new(),
            path: dir.join(PINGS_FILENAME),
        }
    }

    async fn test_manager(dir: &Path) -> PingManager {
        let mut mgr = empty_manager(dir);
        mgr.create_ping("test", "Hey {mentions}!".into(), "admin".into())
            .await
            .unwrap();
        mgr
    }

    #[tokio::test]
    async fn create_ping_rejects_a_built_in_command_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = empty_manager(dir.path());

        // `!lb` is a registered command, so the dispatcher would always win
        // this word and the ping would never fire.
        let err = mgr
            .create_ping("lb", "Hey {mentions}!".into(), "admin".into())
            .await
            .expect_err("a built-in command name must be rejected");
        assert!(
            matches!(err, PingError::ReservedCommand { ref name } if name == "lb"),
            "{err}"
        );

        // The name is lowercased before the check, so casing is no escape.
        assert!(
            mgr.create_ping("LB", "Hey {mentions}!".into(), "admin".into())
                .await
                .is_err()
        );

        // A name that is not a command is still fine.
        mgr.create_ping("uwu", "Hey {mentions}!".into(), "admin".into())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn handle_normalizes_names_and_publishes_them() {
        let dir = tempfile::tempdir().unwrap();
        let handle = PingHandle::load(dir.path()).unwrap();
        let names = handle.names();
        handle
            .create_ping(" Kaffee ".into(), "{mentions}".into(), "admin".into())
            .await
            .unwrap();
        assert_eq!(*names.borrow(), HashSet::from(["kaffee".to_owned()]));
        handle
            .add_member("KAFFEE".into(), "Alice".into())
            .await
            .unwrap();
        assert!(matches!(
            handle
                .try_record_trigger("Kaffee".into(), "alice".into(), Duration::ZERO, false)
                .await,
            TriggerDecision::Fire(_)
        ));
        handle.delete_ping("kaffee".into()).await.unwrap();
        assert!(names.borrow().is_empty());
    }

    #[tokio::test]
    async fn edit_template_updates_template() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = test_manager(dir.path()).await;
        mgr.edit_template("test", "New template {mentions}".into())
            .await
            .unwrap();
        assert_eq!(
            mgr.store.pings.get("test").unwrap().template,
            "New template {mentions}"
        );
    }

    #[tokio::test]
    async fn edit_template_preserves_members() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = test_manager(dir.path()).await;
        mgr.add_member("test", "alice").await.unwrap();
        mgr.add_member("test", "bob").await.unwrap();
        mgr.edit_template("test", "Updated {mentions}".into())
            .await
            .unwrap();
        let ping = mgr.store.pings.get("test").unwrap();
        assert!(ping.members.contains("alice"));
        assert!(ping.members.contains("bob"));
        assert_eq!(ping.members.len(), 2);
    }

    /// Members are rendered into dashboard URL paths (`/pings/<p>/members/<m>/delete`);
    /// a path-shaped member once let a Mod's ✕ click POST somewhere else.
    #[tokio::test]
    async fn add_member_rejects_non_login_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = test_manager(dir.path()).await;
        for bad in ["../../victim/delete?", "a b", "@alice", "", &"a".repeat(26)] {
            assert!(
                matches!(
                    mgr.add_member("test", bad).await,
                    Err(PingError::InvalidMember { .. })
                ),
                "{bad:?}"
            );
        }
        mgr.add_member("test", "Alice_99").await.unwrap();
        let ping = mgr.store.pings.get("test").unwrap();
        assert_eq!(ping.members.len(), 1);
        assert!(ping.members.contains("alice_99"));
    }

    #[tokio::test]
    async fn edit_template_nonexistent_ping_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = test_manager(dir.path()).await;
        let result = mgr.edit_template("nope", "whatever".into()).await;
        assert!(matches!(result, Err(PingError::NotFound { .. })));
    }

    #[tokio::test]
    async fn create_ping_rejects_newline_in_template() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = empty_manager(dir.path());
        let result = mgr
            .create_ping(
                "bad",
                "Hey {mentions}\r\nPRIVMSG #other :pwned".into(),
                "admin".into(),
            )
            .await;
        assert!(matches!(result, Err(PingError::ControlChars)));
        assert!(!mgr.store.pings.contains_key("bad"));
    }

    #[tokio::test]
    async fn edit_template_rejects_control_chars() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = test_manager(dir.path()).await;
        let result = mgr.edit_template("test", "Hey\x00injection".into()).await;
        assert!(matches!(result, Err(PingError::ControlChars)));
        assert_eq!(
            mgr.store.pings.get("test").unwrap().template,
            "Hey {mentions}!"
        );
    }

    #[tokio::test]
    async fn failed_save_reverts_in_memory_change() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = test_manager(dir.path()).await;
        // Unwritable target: the parent directory does not exist.
        mgr.path = dir.path().join("gone").join(PINGS_FILENAME);

        assert!(matches!(
            mgr.create_ping("new", "Hi {mentions}".into(), "admin".into())
                .await,
            Err(PingError::Persist(_))
        ));
        assert!(!mgr.store.pings.contains_key("new"));

        assert!(matches!(
            mgr.add_member("test", "alice").await,
            Err(PingError::Persist(_))
        ));
        assert!(!mgr.is_member("test", "alice"));

        assert!(matches!(
            mgr.delete_ping("test").await,
            Err(PingError::Persist(_))
        ));
        assert!(mgr.store.pings.contains_key("test"));
    }

    #[test]
    fn load_drops_pings_with_invalid_template_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PINGS_FILENAME);
        let raw = "(pings: {\n  \"good\": (template: \"Hey {mentions}!\", members: [], cooldown: None, created_by: \"admin\"),\n  \"bad\": (template: \"Hey {mentions}\\r\\nPRIVMSG #other :pwned\", members: [], cooldown: None, created_by: \"admin\"),\n})\n";
        std::fs::write(&path, raw).unwrap();
        let mgr = PingManager::load(dir.path()).unwrap();
        assert!(mgr.store.pings.contains_key("good"));
        assert!(
            !mgr.store.pings.contains_key("bad"),
            "ping with control chars must be dropped on load"
        );
    }

    #[test]
    fn load_lowercases_names_and_drops_invalid_members() {
        let dir = tempfile::tempdir().unwrap();
        let raw = r#"(pings: {
            "Kaffee": (template: "{mentions}", members: ["Alice", "../../victim/delete?"], cooldown: None, created_by: "admin"),
            "kaffee": (template: "{mentions}", members: ["bob"], cooldown: None, created_by: "admin"),
            "bad name": (template: "{mentions}", members: ["carol"], cooldown: None, created_by: "admin"),
            "Tee": (template: "{mentions}", members: ["Dora", "../x"], cooldown: None, created_by: "admin"),
        })"#;
        std::fs::write(dir.path().join(PINGS_FILENAME), raw).unwrap();
        let mgr = PingManager::load(dir.path()).unwrap();
        let mut names: Vec<&str> = mgr.store.pings.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["kaffee", "tee"]);
        assert!(
            mgr.is_member("kaffee", "bob"),
            "the key that was already lowercase wins a collision"
        );
        let tee: Vec<&str> = mgr.store.pings["tee"]
            .members
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(tee, ["dora"]);
    }

    #[test]
    fn unparseable_file_is_an_error_not_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PINGS_FILENAME), "not ron").unwrap();
        assert!(
            PingManager::load(dir.path()).is_err(),
            "starting empty would overwrite every ping on the next save"
        );
    }

    #[tokio::test]
    async fn edit_template_persists_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = test_manager(dir.path()).await;
        mgr.edit_template("test", "Persisted {mentions}".into())
            .await
            .unwrap();
        let mgr2 = PingManager::load(dir.path()).unwrap();
        assert_eq!(
            mgr2.store.pings.get("test").unwrap().template,
            "Persisted {mentions}"
        );
    }

    #[tokio::test]
    async fn remaining_cooldown_returns_none_when_never_triggered() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = test_manager(dir.path()).await;
        mgr.add_member("test", "alice").await.unwrap();
        assert!(
            mgr.remaining_cooldown("test", Duration::from_secs(300))
                .is_none()
        );
    }

    #[tokio::test]
    async fn remaining_cooldown_returns_some_when_on_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = test_manager(dir.path()).await;
        mgr.add_member("test", "alice").await.unwrap();
        mgr.record_trigger("test").await.unwrap();
        let remaining = mgr.remaining_cooldown("test", Duration::from_secs(300));
        assert!(remaining.is_some());
        let secs = remaining.unwrap().as_secs();
        assert!(secs > 0 && secs <= 300, "expected 1..=300, got {secs}");
    }

    #[tokio::test]
    async fn remaining_cooldown_returns_none_for_nonexistent_ping() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = empty_manager(dir.path());
        assert!(
            mgr.remaining_cooldown("nope", Duration::from_secs(300))
                .is_none()
        );
    }

    #[tokio::test]
    async fn try_record_trigger_is_atomic_across_consecutive_calls() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = test_manager(dir.path()).await;
        mgr.add_member("test", "alice").await.unwrap();
        mgr.add_member("test", "bob").await.unwrap();
        let first = mgr
            .try_record_trigger("test", "bob", Duration::from_secs(300), false)
            .await;
        match first {
            TriggerDecision::Fire(lines) => {
                assert_eq!(lines, ["Hey @alice @bob!"]);
            }
            other => panic!("expected Fire on first call, got {other:?}"),
        }
        let second = mgr
            .try_record_trigger("test", "bob", Duration::from_secs(300), false)
            .await;
        match second {
            TriggerDecision::OnCooldown(remaining) => {
                assert!(remaining.as_secs() <= 300);
            }
            other => panic!("expected OnCooldown on second call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn try_record_trigger_respects_membership() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = test_manager(dir.path()).await;
        mgr.add_member("test", "alice").await.unwrap();
        let decision = mgr
            .try_record_trigger("test", "stranger", Duration::from_secs(300), false)
            .await;
        assert!(matches!(decision, TriggerDecision::Skip));
        let decision = mgr
            .try_record_trigger("test", "alice", Duration::from_secs(300), false)
            .await;
        assert!(
            matches!(decision, TriggerDecision::Fire(_)),
            "sole member with no {{sender}} in template should fire, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn try_record_trigger_public_allows_non_members() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = test_manager(dir.path()).await;
        mgr.add_member("test", "alice").await.unwrap();
        let decision = mgr
            .try_record_trigger("test", "stranger", Duration::from_secs(300), true)
            .await;
        assert!(
            matches!(decision, TriggerDecision::Fire(_)),
            "public=true should allow non-members to fire, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn render_template_includes_sender_when_no_sender_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = empty_manager(dir.path());
        mgr.create_ping("grp", "{mentions}".into(), "admin".into())
            .await
            .unwrap();
        mgr.add_member("grp", "alice").await.unwrap();
        mgr.add_member("grp", "bob").await.unwrap();
        let result = mgr.render_template("grp", "alice").unwrap();
        assert_eq!(result, ["@alice @bob"]);
    }

    #[tokio::test]
    async fn render_template_excludes_sender_when_template_has_sender_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = empty_manager(dir.path());
        mgr.create_ping("grp", "{sender} pinged {mentions}".into(), "admin".into())
            .await
            .unwrap();
        mgr.add_member("grp", "alice").await.unwrap();
        mgr.add_member("grp", "bob").await.unwrap();
        let result = mgr.render_template("grp", "alice").unwrap();
        assert_eq!(result, ["@alice pinged @bob"]);
    }

    #[tokio::test]
    async fn render_template_fires_when_sender_is_sole_member_and_no_sender_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = empty_manager(dir.path());
        mgr.create_ping("grp", "{mentions}".into(), "admin".into())
            .await
            .unwrap();
        mgr.add_member("grp", "alice").await.unwrap();
        let result = mgr.render_template("grp", "alice");
        assert_eq!(result.unwrap(), ["@alice"]);
    }

    #[tokio::test]
    async fn render_template_skips_when_sender_is_sole_member_and_template_has_sender_placeholder()
    {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = empty_manager(dir.path());
        mgr.create_ping("grp", "{sender} pinged {mentions}".into(), "admin".into())
            .await
            .unwrap();
        mgr.add_member("grp", "alice").await.unwrap();
        let result = mgr.render_template("grp", "alice");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn large_group_spreads_over_messages_without_losing_anyone() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = empty_manager(dir.path());
        mgr.create_ping("grp", "Raid {mentions}!".into(), "admin".into())
            .await
            .unwrap();
        // 25-char logins: about 18 fit one 480-byte line.
        let members: Vec<String> = (0..60)
            .map(|i| format!("viewer_{i:02}_{}", "x".repeat(15)))
            .collect();
        for m in &members {
            mgr.add_member("grp", m).await.unwrap();
        }
        let lines = mgr.render_template("grp", "someone").unwrap();
        assert!(lines.len() > 1, "{lines:?}");
        assert!(lines[0].starts_with("Raid @viewer_00") && lines[0].ends_with('!'));
        assert!(
            lines
                .iter()
                .all(|l| crate::twitch::chat_sender::fits_outbound(l)),
            "every line passes the outbound clamp untouched"
        );
        let mentioned: Vec<&str> = lines
            .iter()
            .flat_map(|l| l.split(' '))
            .filter_map(|w| w.strip_prefix('@'))
            .map(|w| w.trim_end_matches('!'))
            .collect();
        assert_eq!(mentioned, members, "every member once, in sorted order");
    }
}
