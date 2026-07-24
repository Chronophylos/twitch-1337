use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use eyre::{Result, WrapErr};
use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use super::db::{
    apply_set_changes, catalog_is_empty, deactivate_emote, get_emote, get_emote_meaning,
    insert_set_event, insert_usage_events, list_active_emotes, list_emote_meanings_for_ids,
    list_set_events, list_usage_events, resolve_active_name, upsert_emote, upsert_emote_meaning,
};
use super::migrate;
use super::types::{
    EmoteMeaningRow, EmoteRow, NewSetEvent, NewUsageEvent, SetChangeBatch, SetEventListFilter,
    SetEventRow, UsageEventListFilter, UsageEventRow,
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
}

#[derive(Clone)]
pub struct EmoteTrackerHandle {
    tx: Arc<mpsc::UnboundedSender<StoreCmd>>,
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

        thread::Builder::new()
            .name("emote-tracker-sqlite".into())
            .spawn(move || run_writer(conn, &mut rx))
            .wrap_err("spawn emote-tracker-sqlite thread")?;

        Ok(Self { tx: Arc::new(tx) })
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote_tracker::{DB_FILENAME, SetEventKind, UsageEventListFilter, try_open};

    const TEST_EMOTE_ID: &str = "0123456789abcdef01234567";

    #[tokio::test]
    async fn handle_resolves_and_fetches_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILENAME);
        let h = EmoteTrackerHandle::open(&path).unwrap();
        h.upsert_emote(TEST_EMOTE_ID, "Kappa", true, 1_736_000_000_000)
            .await
            .unwrap();
        h.insert_set_event(NewSetEvent {
            at: 1_736_000_000_000,
            kind: SetEventKind::Bootstrap,
            emote_id: Some(TEST_EMOTE_ID.into()),
            old_name: None,
            new_name: Some("Kappa".into()),
            actor_7tv_id: None,
            actor_username: None,
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
            requester_login: None,
            requester_twitch_id: None,
            via: None,
            attribution_method: None,
        })
        .await
        .unwrap();
        let row = h.resolve_emote_query("Kappa").await.unwrap().unwrap();
        assert_eq!(row.emote_id, TEST_EMOTE_ID);
        let life = h
            .latest_lifecycle_event(TEST_EMOTE_ID)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(life.kind, SetEventKind::Bootstrap);
    }

    #[tokio::test]
    async fn handle_lists_usage_with_flag_filter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILENAME);
        let h = EmoteTrackerHandle::open(&path).unwrap();
        h.upsert_emote("aaaaaaaaaaaaaaaaaaaaaaaa", "Kappa", true, 1)
            .await
            .unwrap();
        h.insert_usage_events(vec![
            NewUsageEvent {
                at: 1,
                emote_id: "aaaaaaaaaaaaaaaaaaaaaaaa".into(),
                twitch_user_id: "1".into(),
                login: Some("a".into()),
                flagged: false,
            },
            NewUsageEvent {
                at: 2,
                emote_id: "aaaaaaaaaaaaaaaaaaaaaaaa".into(),
                twitch_user_id: "2".into(),
                login: Some("b".into()),
                flagged: true,
            },
        ])
        .await
        .unwrap();
        let rows = h
            .list_usage_events(UsageEventListFilter {
                include_flagged: false,
                limit: 200,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn handle_round_trips_emote_meanings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILENAME);
        let h = EmoteTrackerHandle::open(&path).unwrap();
        h.upsert_emote_meaning(
            "e1",
            "pepega",
            None,
            Some("when chat is being dense"),
            None,
            42,
        )
        .await
        .unwrap();
        let row = h.get_emote_meaning("e1").await.unwrap().unwrap();
        assert_eq!(row.appearance, "pepega");
        assert_eq!(row.usage.as_deref(), Some("when chat is being dense"));
        let listed = h.list_emote_meanings_for_ids(&["e1".into()]).await.unwrap();
        assert_eq!(listed.len(), 1);
    }

    #[tokio::test]
    async fn handle_round_trips_emote_and_usage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILENAME);
        let h = EmoteTrackerHandle::open(&path).unwrap();
        h.upsert_emote("e1", "Kappa", true, 100).await.unwrap();
        h.insert_usage_events(vec![NewUsageEvent {
            at: 101,
            emote_id: "e1".into(),
            twitch_user_id: "1".into(),
            login: Some("bob".into()),
            flagged: false,
        }])
        .await
        .unwrap();
        let active = h.list_active_emotes().await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "Kappa");
    }

    #[test]
    fn try_open_succeeds_on_writable_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let handle = try_open(dir.path());
        assert!(handle.is_some());
    }

    #[test]
    fn try_open_returns_none_when_db_path_is_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(DB_FILENAME)).unwrap();
        assert!(try_open(dir.path()).is_none());
    }
}
