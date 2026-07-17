use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use eyre::{Result, WrapErr};
use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use super::db::{
    apply_set_changes, catalog_is_empty, deactivate_emote, get_emote, insert_set_event,
    insert_usage_events, list_active_emotes, resolve_active_name, upsert_emote,
};
use super::migrate;
use super::types::{EmoteRow, NewSetEvent, NewUsageEvent, SetChangeBatch};

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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote_tracker::{DB_FILENAME, try_open};

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
