//! Record per-token emote usage from main-channel PRIVMSGs into SQLite.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::time::{MissedTickBehavior, interval};
use tracing::error;
use twitch_irc::message::ServerMessage;

use crate::emote_tracker::{EmoteRow, EmoteTrackerHandle, NewUsageEvent, match_usage_tokens};
use crate::settings::SettingsHandle;
use crate::twitch::clanker::should_flag_usage;
use crate::twitch::echo::EchoGuard;

pub const CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn active_name_index(rows: &[EmoteRow]) -> HashMap<String, String> {
    let mut sorted: Vec<_> = rows.iter().filter(|r| r.active).collect();
    sorted.sort_by_key(|r| &r.emote_id);
    let mut out = HashMap::new();
    for row in sorted {
        out.entry(row.name.clone())
            .or_insert_with(|| row.emote_id.clone());
    }
    out
}

async fn refresh_catalog(handle: &EmoteTrackerHandle, out: &mut HashMap<String, String>) {
    match handle.list_active_emotes().await {
        Ok(rows) => *out = active_name_index(&rows),
        Err(error) => error!(?error, "emote usage catalog refresh failed"),
    }
}

pub async fn run_emote_usage_handler(
    mut broadcast_rx: broadcast::Receiver<ServerMessage>,
    emote_tracker: Arc<EmoteTrackerHandle>,
    settings: SettingsHandle,
    channel: String,
    bot_username: String,
    echo: Arc<EchoGuard>,
) {
    let mut active_names = HashMap::new();
    refresh_catalog(&emote_tracker, &mut active_names).await;

    let mut refresh = interval(CATALOG_REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Mouth logins never change post-startup; clone once so the per-message
    // deny-list merge below is just a cheap chain, not a re-clone of the guard's set.
    let mouth_logins: Vec<String> = echo.mouth_logins().iter().cloned().collect();

    loop {
        tokio::select! {
            _ = refresh.tick() => {
                refresh_catalog(&emote_tracker, &mut active_names).await;
            }
            msg = broadcast_rx.recv() => {
                match msg {
                    Ok(ServerMessage::Privmsg(p)) if p.channel_login == channel => {
                        let user_id = p.sender.id.as_str();
                        if user_id.is_empty() {
                            continue;
                        }
                        let emote_ids = match_usage_tokens(&p.message_text, &active_names);
                        if emote_ids.is_empty() {
                            continue;
                        }
                        let snapshot = settings.load();
                        let deny_with_mouths: Vec<String> = snapshot
                            .twitch
                            .emote_usage_deny_list
                            .iter()
                            .cloned()
                            .chain(mouth_logins.iter().cloned())
                            .collect();
                        let flagged =
                            should_flag_usage(&p.sender.login, &bot_username, &deny_with_mouths);
                        let at = p.server_timestamp.timestamp_millis();
                        let login = Some(p.sender.login.clone());
                        let events: Vec<NewUsageEvent> = emote_ids
                            .into_iter()
                            .map(|emote_id| NewUsageEvent {
                                at,
                                emote_id,
                                twitch_user_id: user_id.to_owned(),
                                login: login.clone(),
                                flagged,
                            })
                            .collect();
                        let handle = emote_tracker.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle.insert_usage_events(events).await {
                                error!(?error, "emote usage insert failed");
                            }
                        });
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_name_index_lowest_id_wins_collision() {
        let rows = vec![
            EmoteRow {
                emote_id: "b".into(),
                name: "Same".into(),
                active: true,
                updated_at: 0,
            },
            EmoteRow {
                emote_id: "a".into(),
                name: "Same".into(),
                active: true,
                updated_at: 0,
            },
        ];
        let map = active_name_index(&rows);
        assert_eq!(map.get("Same").unwrap(), "a");
    }
}
