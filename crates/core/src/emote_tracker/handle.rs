use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use eyre::{Result, WrapErr};
use rusqlite::Connection;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot};

use super::db::{
    apply_set_changes, catalog_is_empty, count_usage_events, deactivate_emote, get_emote,
    get_emote_meaning, insert_set_event, insert_usage_events, least_used_emotes,
    list_active_emotes, list_emote_meanings_for_ids, list_set_events, list_usage_events,
    resolve_active_name, resolve_viewer_twitch_id, top_usage_counts, top_users_by_usage,
    upsert_emote, upsert_emote_meaning, usage_counts_for_user, usage_counts_of_emote,
};
use super::migrate;
use super::types::{
    EmoteMeaningRow, EmoteRow, EmoteUsageCount, LeastUsedEmote, NewSetEvent, NewUsageEvent,
    SetChangeBatch, SetEventListFilter, SetEventRow, UsageEventListFilter, UsageEventRow,
    UserUsageCount,
};

enum StoreCmd {
    UpsertEmote {
        emote_id: String,
        name: String,
        active: bool,
        updated_at: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    DeactivateEmote {
        emote_id: String,
        updated_at: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    InsertSetEvent {
        event: NewSetEvent,
        reply: oneshot::Sender<Result<i64>>,
    },
    InsertUsageEvents {
        events: Vec<NewUsageEvent>,
        reply: oneshot::Sender<Result<()>>,
    },
    ListActiveEmotes {
        reply: oneshot::Sender<Result<Vec<EmoteRow>>>,
    },
    GetEmote {
        emote_id: String,
        reply: oneshot::Sender<Result<Option<EmoteRow>>>,
    },
    ResolveActiveName {
        name: String,
        reply: oneshot::Sender<Result<Option<EmoteRow>>>,
    },
    ApplySetChanges {
        batch: SetChangeBatch,
        reply: oneshot::Sender<Result<()>>,
    },
    CatalogIsEmpty {
        reply: oneshot::Sender<Result<bool>>,
    },
    ResolveEmoteQuery {
        query: String,
        reply: oneshot::Sender<Result<Option<EmoteRow>>>,
    },
    LatestLifecycleEvent {
        emote_id: String,
        reply: oneshot::Sender<Result<Option<SetEventRow>>>,
    },
    ListSetEvents {
        filter: SetEventListFilter,
        reply: oneshot::Sender<Result<Vec<SetEventRow>>>,
    },
    ListUsageEvents {
        filter: UsageEventListFilter,
        reply: oneshot::Sender<Result<Vec<UsageEventRow>>>,
    },
    CountUsageEvents {
        filter: UsageEventListFilter,
        reply: oneshot::Sender<Result<u64>>,
    },
    TopUsageCounts {
        since_ms: Option<i64>,
        limit: u32,
        reply: oneshot::Sender<Result<Vec<EmoteUsageCount>>>,
    },
    TopUsers {
        since_ms: Option<i64>,
        limit: u32,
        reply: oneshot::Sender<Result<Vec<UserUsageCount>>>,
    },
    LeastUsedEmotes {
        since_ms: Option<i64>,
        limit: u32,
        reply: oneshot::Sender<Result<(i64, Vec<LeastUsedEmote>)>>,
    },
    ResolveViewerTwitchId {
        login: String,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    UsageCountsForUser {
        twitch_user_id: String,
        since_ms: Option<i64>,
        limit: u32,
        reply: oneshot::Sender<Result<(i64, Vec<EmoteUsageCount>)>>,
    },
    UsageCountsOfEmote {
        emote_id: String,
        since_ms: Option<i64>,
        limit: u32,
        reply: oneshot::Sender<Result<(i64, Vec<UserUsageCount>)>>,
    },
    UpsertEmoteMeaning {
        emote_id: String,
        appearance: String,
        meaning: Option<String>,
        usage: Option<String>,
        avoid: Option<String>,
        updated_at: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    GetEmoteMeaning {
        emote_id: String,
        reply: oneshot::Sender<Result<Option<EmoteMeaningRow>>>,
    },
    ListEmoteMeaningsForIds {
        ids: Vec<String>,
        reply: oneshot::Sender<Result<Vec<EmoteMeaningRow>>>,
    },
    Close(oneshot::Sender<()>),
}

#[derive(Debug, Error)]
pub enum EmoteTrackerFinalizeError {
    #[error("emote tracker SQLite writer closed before the close command was accepted")]
    CloseCommandFailed,

    #[error("emote tracker SQLite writer closed before close acknowledgement")]
    CloseReplyFailed,

    #[error("failed to schedule emote tracker SQLite writer join")]
    JoinTaskFailed(#[source] tokio::task::JoinError),

    #[error("emote tracker SQLite writer thread panicked")]
    WriterPanicked,
}

struct WriterOwner {
    handle: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Drop for WriterOwner {
    fn drop(&mut self) {
        // Normal runtime ownership takes the handle in `close_and_join`. If
        // composition unwinds before finalization, the last handle can still
        // synchronously confirm the named thread has terminated.
        if let Ok(mut handle) = self.handle.try_lock()
            && let Some(handle) = handle.take()
        {
            let _ = handle.join();
        }
    }
}

#[derive(Clone)]
pub struct EmoteTrackerHandle {
    tx: Arc<mpsc::UnboundedSender<StoreCmd>>,
    writer: Arc<WriterOwner>,
}

impl EmoteTrackerHandle {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).wrap_err_with(|| {
                format!("create emote tracker DB parent dir {}", parent.display())
            })?;
        }

        let conn = Connection::open(path)
            .wrap_err_with(|| format!("open emote tracker DB at {}", path.display()))?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
     PRAGMA foreign_keys = ON;",
        )?;
        migrate(&conn)?;

        let (tx, mut rx) = mpsc::unbounded_channel();

        let writer = thread::Builder::new()
            .name("emote-tracker-sqlite".into())
            .spawn(move || run_writer(conn, &mut rx))
            .wrap_err("spawn emote-tracker-sqlite thread")?;

        Ok(Self {
            tx: Arc::new(tx),
            writer: Arc::new(WriterOwner {
                handle: Mutex::new(Some(writer)),
            }),
        })
    }

    async fn send<R>(
        &self,
        make_cmd: impl FnOnce(oneshot::Sender<Result<R>>) -> StoreCmd,
    ) -> Result<R> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(make_cmd(reply_tx))
            .map_err(|_| eyre::eyre!("emote tracker writer thread exited"))?;
        reply_rx
            .await
            .map_err(|_| eyre::eyre!("emote tracker writer dropped reply"))?
    }

    pub async fn upsert_emote(
        &self,
        emote_id: &str,
        name: &str,
        active: bool,
        updated_at: i64,
    ) -> Result<()> {
        let emote_id = emote_id.to_owned();
        let name = name.to_owned();
        self.send(|reply| StoreCmd::UpsertEmote {
            emote_id,
            name,
            active,
            updated_at,
            reply,
        })
        .await
    }

    pub async fn deactivate_emote(&self, emote_id: &str, updated_at: i64) -> Result<()> {
        let emote_id = emote_id.to_owned();
        self.send(|reply| StoreCmd::DeactivateEmote {
            emote_id,
            updated_at,
            reply,
        })
        .await
    }

    pub async fn insert_set_event(&self, event: NewSetEvent) -> Result<i64> {
        self.send(|reply| StoreCmd::InsertSetEvent { event, reply })
            .await
    }

    pub async fn insert_usage_events(&self, events: Vec<NewUsageEvent>) -> Result<()> {
        self.send(|reply| StoreCmd::InsertUsageEvents { events, reply })
            .await
    }

    pub async fn list_active_emotes(&self) -> Result<Vec<EmoteRow>> {
        self.send(|reply| StoreCmd::ListActiveEmotes { reply })
            .await
    }

    pub async fn get_emote(&self, emote_id: &str) -> Result<Option<EmoteRow>> {
        let emote_id = emote_id.to_owned();
        self.send(|reply| StoreCmd::GetEmote { emote_id, reply })
            .await
    }

    pub async fn resolve_active_name(&self, name: &str) -> Result<Option<EmoteRow>> {
        let name = name.to_owned();
        self.send(|reply| StoreCmd::ResolveActiveName { name, reply })
            .await
    }

    pub async fn apply_set_changes(&self, batch: SetChangeBatch) -> Result<()> {
        self.send(|reply| StoreCmd::ApplySetChanges { batch, reply })
            .await
    }

    pub async fn catalog_is_empty(&self) -> Result<bool> {
        self.send(|reply| StoreCmd::CatalogIsEmpty { reply }).await
    }

    pub async fn resolve_emote_query(&self, query: &str) -> Result<Option<EmoteRow>> {
        let query = query.to_owned();
        self.send(|reply| StoreCmd::ResolveEmoteQuery { query, reply })
            .await
    }

    pub async fn latest_lifecycle_event(&self, emote_id: &str) -> Result<Option<SetEventRow>> {
        let emote_id = emote_id.to_owned();
        self.send(|reply| StoreCmd::LatestLifecycleEvent { emote_id, reply })
            .await
    }

    pub async fn list_set_events(&self, filter: SetEventListFilter) -> Result<Vec<SetEventRow>> {
        self.send(|reply| StoreCmd::ListSetEvents { filter, reply })
            .await
    }

    pub async fn list_usage_events(
        &self,
        filter: UsageEventListFilter,
    ) -> Result<Vec<UsageEventRow>> {
        self.send(|reply| StoreCmd::ListUsageEvents { filter, reply })
            .await
    }

    /// Events matching `filter`, ignoring its cursor and limit.
    pub async fn count_usage_events(&self, filter: UsageEventListFilter) -> Result<u64> {
        self.send(|reply| StoreCmd::CountUsageEvents { filter, reply })
            .await
    }

    /// Top emotes by unflagged use count within a window.
    pub async fn top_usage_events(
        &self,
        since_ms: Option<i64>,
        limit: u32,
    ) -> Result<Vec<EmoteUsageCount>> {
        self.send(|reply| StoreCmd::TopUsageCounts {
            since_ms,
            limit,
            reply,
        })
        .await
    }

    /// Viewers ranked by unflagged emote uses across all emotes.
    pub async fn top_users(
        &self,
        since_ms: Option<i64>,
        limit: u32,
    ) -> Result<Vec<UserUsageCount>> {
        self.send(|reply| StoreCmd::TopUsers {
            since_ms,
            limit,
            reply,
        })
        .await
    }

    /// Active emotes least-used first, plus the count of never-used ones.
    pub async fn least_used_emotes(
        &self,
        since_ms: Option<i64>,
        limit: u32,
    ) -> Result<(i64, Vec<LeastUsedEmote>)> {
        self.send(|reply| StoreCmd::LeastUsedEmotes {
            since_ms,
            limit,
            reply,
        })
        .await
    }

    /// Resolve a viewer login to their stable Twitch user id (`None` if never seen).
    pub async fn resolve_viewer(&self, login: &str) -> Result<Option<String>> {
        let login = login.to_owned();
        self.send(|reply| StoreCmd::ResolveViewerTwitchId { login, reply })
            .await
    }

    /// Per-emote unflagged counts for one viewer plus total across all emotes.
    pub async fn usage_count_by_user(
        &self,
        twitch_user_id: &str,
        since_ms: Option<i64>,
        limit: u32,
    ) -> Result<(i64, Vec<EmoteUsageCount>)> {
        let twitch_user_id = twitch_user_id.to_owned();
        self.send(|reply| StoreCmd::UsageCountsForUser {
            twitch_user_id,
            since_ms,
            limit,
            reply,
        })
        .await
    }

    /// Total uses of one emote plus per-viewer ranking.
    pub async fn usage_count_by_emote(
        &self,
        emote_id: &str,
        since_ms: Option<i64>,
        limit: u32,
    ) -> Result<(i64, Vec<UserUsageCount>)> {
        let emote_id = emote_id.to_owned();
        self.send(|reply| StoreCmd::UsageCountsOfEmote {
            emote_id,
            since_ms,
            limit,
            reply,
        })
        .await
    }

    pub async fn upsert_emote_meaning(
        &self,
        emote_id: &str,
        appearance: &str,
        meaning: Option<&str>,
        usage: Option<&str>,
        avoid: Option<&str>,
        updated_at: i64,
    ) -> Result<()> {
        let emote_id = emote_id.to_owned();
        let appearance = appearance.to_owned();
        let meaning = meaning.map(str::to_owned);
        let usage = usage.map(str::to_owned);
        let avoid = avoid.map(str::to_owned);
        self.send(|reply| StoreCmd::UpsertEmoteMeaning {
            emote_id,
            appearance,
            meaning,
            usage,
            avoid,
            updated_at,
            reply,
        })
        .await
    }

    pub async fn get_emote_meaning(&self, emote_id: &str) -> Result<Option<EmoteMeaningRow>> {
        let emote_id = emote_id.to_owned();
        self.send(|reply| StoreCmd::GetEmoteMeaning { emote_id, reply })
            .await
    }

    pub async fn list_emote_meanings_for_ids(
        &self,
        ids: &[String],
    ) -> Result<Vec<EmoteMeaningRow>> {
        let ids = ids.to_vec();
        self.send(|reply| StoreCmd::ListEmoteMeaningsForIds { ids, reply })
            .await
    }

    /// Drain queued commands, stop the named SQLite writer, and synchronously
    /// join its thread. This operation is idempotent across cloned handles.
    pub async fn close_and_join(&self) -> Result<(), EmoteTrackerFinalizeError> {
        let mut writer = self.writer.handle.lock().await;
        if writer.is_none() {
            return Ok(());
        }

        let (close_tx, close_rx) = oneshot::channel();
        let command_result = self.tx.send(StoreCmd::Close(close_tx));
        let close_result = match command_result {
            Ok(()) => close_rx
                .await
                .map_err(|_| EmoteTrackerFinalizeError::CloseReplyFailed),
            Err(_) => Err(EmoteTrackerFinalizeError::CloseCommandFailed),
        };

        // Even when command acknowledgement fails, take and join the retained
        // thread handle. Returning must not leave an application-owned writer
        // detached.
        let handle = writer
            .take()
            .expect("writer handle was present before close began");
        let join_result = tokio::task::spawn_blocking(move || handle.join())
            .await
            .map_err(EmoteTrackerFinalizeError::JoinTaskFailed)?;
        let join_result = join_result.map_err(|_| EmoteTrackerFinalizeError::WriterPanicked);

        close_result.and(join_result)
    }
}

fn run_writer(conn: Connection, rx: &mut mpsc::UnboundedReceiver<StoreCmd>) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            StoreCmd::UpsertEmote {
                emote_id,
                name,
                active,
                updated_at,
                reply,
            } => {
                let _ = reply.send(upsert_emote(&conn, &emote_id, &name, active, updated_at));
            }
            StoreCmd::DeactivateEmote {
                emote_id,
                updated_at,
                reply,
            } => {
                let _ = reply.send(deactivate_emote(&conn, &emote_id, updated_at));
            }
            StoreCmd::InsertSetEvent { event, reply } => {
                let _ = reply.send(insert_set_event(&conn, &event));
            }
            StoreCmd::InsertUsageEvents { events, reply } => {
                let _ = reply.send(insert_usage_events(&conn, &events));
            }
            StoreCmd::ListActiveEmotes { reply } => {
                let _ = reply.send(list_active_emotes(&conn));
            }
            StoreCmd::GetEmote { emote_id, reply } => {
                let _ = reply.send(get_emote(&conn, &emote_id));
            }
            StoreCmd::ResolveActiveName { name, reply } => {
                let _ = reply.send(resolve_active_name(&conn, &name));
            }
            StoreCmd::ApplySetChanges { batch, reply } => {
                let _ = reply.send(apply_set_changes(&conn, &batch));
            }
            StoreCmd::CatalogIsEmpty { reply } => {
                let _ = reply.send(catalog_is_empty(&conn));
            }
            StoreCmd::ResolveEmoteQuery { query, reply } => {
                let _ = reply.send(super::lookup::resolve_emote_query(&conn, &query));
            }
            StoreCmd::LatestLifecycleEvent { emote_id, reply } => {
                let _ = reply.send(super::db::latest_lifecycle_event(&conn, &emote_id));
            }
            StoreCmd::ListSetEvents { filter, reply } => {
                let _ = reply.send(list_set_events(&conn, &filter));
            }
            StoreCmd::ListUsageEvents { filter, reply } => {
                let _ = reply.send(list_usage_events(&conn, &filter));
            }
            StoreCmd::CountUsageEvents { filter, reply } => {
                let _ = reply.send(count_usage_events(&conn, &filter));
            }
            StoreCmd::TopUsageCounts {
                since_ms,
                limit,
                reply,
            } => {
                let _ = reply.send(top_usage_counts(&conn, since_ms, limit));
            }
            StoreCmd::TopUsers {
                since_ms,
                limit,
                reply,
            } => {
                let _ = reply.send(top_users_by_usage(&conn, since_ms, limit));
            }
            StoreCmd::LeastUsedEmotes {
                since_ms,
                limit,
                reply,
            } => {
                let _ = reply.send(least_used_emotes(&conn, since_ms, limit));
            }
            StoreCmd::ResolveViewerTwitchId { login, reply } => {
                let _ = reply.send(resolve_viewer_twitch_id(&conn, &login));
            }
            StoreCmd::UsageCountsForUser {
                twitch_user_id,
                since_ms,
                limit,
                reply,
            } => {
                let _ = reply.send(usage_counts_for_user(
                    &conn,
                    &twitch_user_id,
                    since_ms,
                    limit,
                ));
            }
            StoreCmd::UsageCountsOfEmote {
                emote_id,
                since_ms,
                limit,
                reply,
            } => {
                let _ = reply.send(usage_counts_of_emote(&conn, &emote_id, since_ms, limit));
            }
            StoreCmd::UpsertEmoteMeaning {
                emote_id,
                appearance,
                meaning,
                usage,
                avoid,
                updated_at,
                reply,
            } => {
                let _ = reply.send(upsert_emote_meaning(
                    &conn,
                    &emote_id,
                    &appearance,
                    meaning.as_deref(),
                    usage.as_deref(),
                    avoid.as_deref(),
                    updated_at,
                ));
            }
            StoreCmd::GetEmoteMeaning { emote_id, reply } => {
                let _ = reply.send(get_emote_meaning(&conn, &emote_id));
            }
            StoreCmd::ListEmoteMeaningsForIds { ids, reply } => {
                let _ = reply.send(list_emote_meanings_for_ids(&conn, &ids));
            }
            StoreCmd::Close(reply) => {
                let _ = reply.send(());
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote_tracker::{DB_FILENAME, SetEventKind, SetTenure, try_open};

    #[tokio::test]
    async fn close_and_join_retains_named_writer_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILENAME);
        let handle = EmoteTrackerHandle::open(&path).unwrap();

        handle.upsert_emote("e1", "Kappa", true, 1).await.unwrap();
        handle
            .insert_usage_events(vec![NewUsageEvent {
                at: 1,
                emote_id: "e1".into(),
                twitch_user_id: "u1".into(),
                login: Some("alice".into()),
                flagged: false,
            }])
            .await
            .unwrap();
        handle.close_and_join().await.unwrap();

        assert!(handle.writer.handle.lock().await.is_none());
        assert!(
            handle
                .catalog_is_empty()
                .await
                .expect_err("closed writer must reject later work")
                .to_string()
                .contains("writer thread exited")
        );
        handle.close_and_join().await.unwrap();
    }

    #[test]
    fn try_open_returns_none_when_db_path_is_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(DB_FILENAME)).unwrap();
        assert!(try_open(dir.path()).is_none());
    }

    /// Seeds: emotes KEKW/e1 and Pepe/e2; unflagged use by alice(u1) x3 +
    /// bob(u2) x1 on e1 inside the window, an extra e1 use before the window,
    /// a flagged use (must never count), and one e2 use by alice.
    async fn seed_usage_fixture() -> (EmoteTrackerHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let h = EmoteTrackerHandle::open(&dir.path().join(DB_FILENAME)).unwrap();
        h.upsert_emote("e1", "KEKW", true, 10).await.unwrap();
        h.upsert_emote("e2", "Pepe", true, 10).await.unwrap();
        let ev = |at: i64, emote: &str, uid: &str, login: &str, flagged: bool| NewUsageEvent {
            at,
            emote_id: emote.into(),
            twitch_user_id: uid.into(),
            login: Some(login.into()),
            flagged,
        };
        h.insert_usage_events(vec![
            ev(100, "e1", "u1", "Alice", false),
            ev(200, "e1", "u1", "alice", false),
            ev(300, "e1", "u1", "alice", false),
            ev(400, "e1", "u2", "bob", false),
            // Flagged bot spam — excluded from every count.
            ev(500, "e1", "u3", "spam-bot", true),
            e1_before_window(60),
            ev(150, "e2", "u1", "alice", false),
        ])
        .await
        .unwrap();
        (h, dir)
    }

    fn e1_before_window(at: i64) -> NewUsageEvent {
        NewUsageEvent {
            at,
            emote_id: "e1".into(),
            twitch_user_id: "u9".into(),
            login: Some("early".into()),
            flagged: false,
        }
    }

    #[tokio::test]
    async fn top_usage_events_ranks_counts_and_respects_window() {
        let (h, _dir) = seed_usage_fixture().await;
        // Window at 100 excludes the pre-window event but keeps all else.
        let top = h.top_usage_events(Some(100), 20).await.unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].name, "KEKW");
        assert_eq!(top[0].count, 4); // 3x alice + bob, not flagged, not early
        assert_eq!(top[0].last_used_at, 400);
        assert_eq!(top[1].name, "Pepe");
        // Unbounded window must also pick up the early event.
        let all = h.top_usage_events(None, 20).await.unwrap();
        assert_eq!(all[0].count, 5);
        // Cap truncates the ranking.
        let capped = h.top_usage_events(None, 1).await.unwrap();
        assert_eq!(capped.len(), 1);
    }

    #[tokio::test]
    async fn usage_count_by_user_filters_and_sums() {
        let (h, _dir) = seed_usage_fixture().await;
        // Login resolution is case-insensitive.
        let uid = h.resolve_viewer("ALICE").await.unwrap().unwrap();
        assert_eq!(uid, "u1");
        assert!(h.resolve_viewer("never-seen").await.unwrap().is_none());

        let (total, per_emote) = h.usage_count_by_user(&uid, Some(100), 20).await.unwrap();
        assert_eq!(total, 4); // 3x KEKW + Pepe; flags excluded
        assert_eq!(per_emote.len(), 2);
        assert_eq!(per_emote[0].name, "KEKW");
        assert_eq!(per_emote[0].count, 3);
    }

    #[tokio::test]
    async fn usage_count_by_emote_ranks_viewers_within_window() {
        let (h, _dir) = seed_usage_fixture().await;
        let (total, users) = h.usage_count_by_emote("e1", Some(100), 20).await.unwrap();
        assert_eq!(total, 4);
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].twitch_user_id, "u1");
        assert_eq!(users[0].login.as_deref(), Some("alice"));
        assert_eq!(users[0].count, 3);
        assert_eq!(users[1].twitch_user_id, "u2");
        assert!(!users.iter().any(|u| u.twitch_user_id == "u3"));
    }

    /// Lifecycle row with only the fields these tests vary.
    fn set_ev(
        at: i64,
        kind: SetEventKind,
        emote_id: &str,
        old_name: Option<&str>,
        new_name: Option<&str>,
        actor: Option<&str>,
        requester: Option<&str>,
    ) -> NewSetEvent {
        NewSetEvent {
            at,
            kind,
            emote_id: Some(emote_id.into()),
            old_name: old_name.map(Into::into),
            new_name: new_name.map(Into::into),
            actor_7tv_id: None,
            actor_username: actor.map(Into::into),
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
            requester_login: requester.map(Into::into),
            requester_twitch_id: None,
            via: requester.map(|_| "potatbotat".into()),
            attribution_method: None,
        }
    }

    #[tokio::test]
    async fn top_users_sums_across_emotes_and_skips_flagged() {
        let (h, _dir) = seed_usage_fixture().await;
        let users = h.top_users(None, 20).await.unwrap();
        // alice: 3x KEKW + 1x Pepe; label is her newest login, not "Alice".
        assert_eq!(users[0].twitch_user_id, "u1");
        assert_eq!(users[0].count, 4);
        assert_eq!(users[0].login.as_deref(), Some("alice"));
        assert!(!users.iter().any(|u| u.twitch_user_id == "u3"));
        // Window drops the pre-window viewer entirely.
        let windowed = h.top_users(Some(100), 20).await.unwrap();
        assert!(!windowed.iter().any(|u| u.twitch_user_id == "u9"));
    }

    #[tokio::test]
    async fn least_used_ranks_unused_first_and_reports_tenure() {
        let (h, _dir) = seed_usage_fixture().await;
        h.upsert_emote("e3", "Dead", true, 10).await.unwrap();
        h.upsert_emote("e4", "Gone", false, 10).await.unwrap();
        for ev in [
            set_ev(
                5,
                SetEventKind::Bootstrap,
                "e1",
                None,
                Some("KEKW"),
                None,
                None,
            ),
            set_ev(
                5,
                SetEventKind::Bootstrap,
                "e2",
                None,
                Some("Pepe"),
                None,
                None,
            ),
            set_ev(50, SetEventKind::Add, "e3", None, Some("Dead"), None, None),
        ] {
            h.insert_set_event(ev).await.unwrap();
        }
        let (unused, rows) = h.least_used_emotes(None, 20).await.unwrap();
        assert_eq!(unused, 1);
        // Inactive emotes are not "unused", they are gone.
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["Dead", "Pepe", "KEKW"]
        );
        assert_eq!(rows[0].count, 0);
        assert_eq!(rows[0].last_used_at, None);
        // No actor or requester: a snapshot-diff add, so its time is approximate.
        assert_eq!(
            rows[0].tenure,
            Some(SetTenure::Added {
                at: 50,
                approximate: true
            })
        );
        assert_eq!(rows[1].tenure, Some(SetTenure::TrackingStart));
        // Windowed: KEKW's pre-window use no longer counts, Pepe's still does.
        let (_, windowed) = h.least_used_emotes(Some(100), 20).await.unwrap();
        assert_eq!(windowed[2].count, 4);
    }

    #[tokio::test]
    async fn least_used_breaks_zero_use_ties_by_tenure() {
        let dir = tempfile::tempdir().unwrap();
        let h = EmoteTrackerHandle::open(&dir.path().join(DB_FILENAME)).unwrap();
        // Alphabetical order would put the brand-new "AAAA" first.
        for (id, name) in [("n1", "AAAA"), ("o1", "Old"), ("b1", "Zzz")] {
            h.upsert_emote(id, name, true, 10).await.unwrap();
        }
        for ev in [
            set_ev(
                900,
                SetEventKind::Add,
                "n1",
                None,
                Some("AAAA"),
                Some("chef"),
                None,
            ),
            set_ev(
                100,
                SetEventKind::Add,
                "o1",
                None,
                Some("Old"),
                Some("chef"),
                None,
            ),
            set_ev(
                5,
                SetEventKind::Bootstrap,
                "b1",
                None,
                Some("Zzz"),
                None,
                None,
            ),
        ] {
            h.insert_set_event(ev).await.unwrap();
        }
        let (unused, rows) = h.least_used_emotes(None, 20).await.unwrap();
        assert_eq!(unused, 3);
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["Zzz", "Old", "AAAA"]
        );
        assert_eq!(
            rows[1].tenure,
            Some(SetTenure::Added {
                at: 100,
                approximate: false
            })
        );
    }

    #[tokio::test]
    async fn usage_counts_mark_removed_emotes_inactive() {
        let (h, _dir) = seed_usage_fixture().await;
        h.deactivate_emote("e2", 20).await.unwrap();
        let top = h.top_usage_events(None, 20).await.unwrap();
        assert!(top.iter().find(|r| r.name == "KEKW").unwrap().active);
        assert!(!top.iter().find(|r| r.name == "Pepe").unwrap().active);
        let (_, per_emote) = h.usage_count_by_user("u1", None, 20).await.unwrap();
        assert!(!per_emote.iter().find(|r| r.name == "Pepe").unwrap().active);
    }

    #[tokio::test]
    async fn set_events_filter_lifecycle_and_user() {
        let (h, _dir) = seed_usage_fixture().await;
        for ev in [
            set_ev(
                5,
                SetEventKind::Bootstrap,
                "e1",
                None,
                Some("KEKW"),
                None,
                None,
            ),
            set_ev(
                60,
                SetEventKind::Add,
                "e2",
                None,
                Some("Pepe"),
                Some("potat"),
                Some("alice"),
            ),
            set_ev(
                70,
                SetEventKind::Remove,
                "e1",
                Some("KEKW"),
                None,
                Some("chef"),
                None,
            ),
        ] {
            h.insert_set_event(ev).await.unwrap();
        }
        let list = |user: Option<&str>| SetEventListFilter {
            lifecycle_only: true,
            user: user.map(Into::into),
            limit: 20,
            ..Default::default()
        };
        let all = h.list_set_events(list(None)).await.unwrap();
        assert_eq!(all.len(), 2, "bootstrap must be dropped");
        let by_requester = h.list_set_events(list(Some("ALICE"))).await.unwrap();
        assert_eq!(by_requester.len(), 1);
        assert_eq!(by_requester[0].kind, SetEventKind::Add);
        let by_actor = h.list_set_events(list(Some("Chef"))).await.unwrap();
        assert_eq!(by_actor.len(), 1);
        assert_eq!(by_actor[0].kind, SetEventKind::Remove);
    }

    #[tokio::test]
    async fn set_events_match_requester_twitch_id_and_swapped_versions() {
        let (h, _dir) = seed_usage_fixture().await;
        let mut add = set_ev(
            60,
            SetEventKind::Add,
            "e2",
            None,
            Some("KEKW"),
            None,
            Some("alice"),
        );
        add.requester_twitch_id = Some("u1".into());
        // Version swap: the old KEKW lived under id e1.
        let remove = set_ev(
            50,
            SetEventKind::Remove,
            "e1",
            Some("KEKW"),
            None,
            None,
            None,
        );
        h.insert_set_event(add).await.unwrap();
        h.insert_set_event(remove).await.unwrap();

        let by_id = h
            .list_set_events(SetEventListFilter {
                user: Some("u1".into()),
                limit: 20,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(by_id.len(), 1);

        let filter = |name: Option<&str>| SetEventListFilter {
            emote_id: Some("e2".into()),
            emote_name: name.map(Into::into),
            limit: 20,
            ..Default::default()
        };
        assert_eq!(h.list_set_events(filter(None)).await.unwrap().len(), 1);
        assert_eq!(
            h.list_set_events(filter(Some("KEKW"))).await.unwrap().len(),
            2
        );
    }
}
