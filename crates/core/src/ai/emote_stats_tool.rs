//! On-demand `emote_stats` chat-turn tool over the emote tracker DB.

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use chrono_tz::Europe::Berlin;
use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use serde::Deserialize;

use crate::emote_tracker::{
    EmoteTrackerHandle, SetEventListFilter, format_berlin_millis, format_set_event_attribution,
};

pub const EMOTE_STATS_TOOL_NAME: &str = "emote_stats";

/// Hard cap for any result list this tool forwards to the model. The turn
/// already carries chat history and memory bodies; more would be tokens on
/// every call for detail nobody asks about in chat.
const MAX_RESULTS: usize = 20;
/// Sensible default ranking depth — a chat answer never wants 20 lines.
const DEFAULT_RESULTS: usize = 5;
/// Windows are calendar days on Europe/Berlin (`window_days = 1` means
/// "since Berlin midnight"). Bounded so the model cannot request decade-long
/// scans; full history stays on the dashboard.
const MAX_WINDOW_DAYS: u32 = 30;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StatsKind {
    Top,
    UsageByUser,
    UsageOfEmote,
    SetHistory,
}

#[derive(Debug, Deserialize)]
struct EmoteStatsArgs {
    kind: StatsKind,
    /// Viewer login (or Twitch user id) for `usage_by_user`.
    #[serde(default)]
    user: Option<String>,
    /// Emote code (or fragment/id) for `usage_of_emote`.
    #[serde(default)]
    emote: Option<String>,
    /// Calendar-day window ending now, counted on Europe/Berlin days.
    #[serde(default)]
    window_days: Option<u32>,
    #[serde(default)]
    limit: Option<usize>,
}

pub fn emote_stats_tool() -> ToolDefinition {
    ToolDefinition {
        name: EMOTE_STATS_TOOL_NAME.into(),
        description: "Frag die Emote-Nutzungsstatistik des Channels ab. kind=top: meistgenutzte \
            Emotes im Zeitraum. kind=usage_by_user mit user: was und wie oft ein bestimmter \
            Viewer nutzt. kind=usage_of_emote mit emote: wer und wie oft ein Emote nutzt. \
            kind=set_history: letzte Set-Aenderungen (Add/Remove/Rename mit Attribution). \
            Optional window_days (1..=30, Berlin-Tage, ohne Angabe: alles) und limit. \
            Erfinde niemals Zahlen — nimm exakt die gelieferten Werte."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["top", "usage_by_user", "usage_of_emote", "set_history"],
                },
                "user": { "type": "string" },
                "emote": { "type": "string" },
                "window_days": { "type": "integer", "minimum": 1, "maximum": 30 },
                "limit": { "type": "integer", "minimum": 1, "maximum": 20 }
            },
            "required": ["kind"]
        }),
    }
}

fn invalid(call: &ToolCall, details: &str) -> ToolResultMessage {
    ToolResultMessage::for_call(
        call,
        serde_json::json!({ "error": "invalid_arguments", "details": details }).to_string(),
    )
}

fn stats_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_RESULTS).clamp(1, MAX_RESULTS)
}

/// Cutoff timestamp (UTC unix ms) for a Berlin calendar-day window:
/// `window_days = 3` starts two Berlin midnights before today's.
fn window_cutoff(days: u32) -> eyre::Result<Option<i64>> {
    if days == 0 {
        return Ok(None);
    }
    if days > MAX_WINDOW_DAYS {
        eyre::bail!("window_days exceeds {}", MAX_WINDOW_DAYS);
    }
    // Midnights always exist in EU timezones — DST jumps happen after 01:00.
    let today = Utc::now().with_timezone(&Berlin);
    let cutoff_date = today.date_naive() - chrono::Duration::days(i64::from(days - 1));
    let midnight = cutoff_date.and_hms_opt(0, 0, 0).expect("midnight is valid");
    let cutoff = Berlin
        .from_local_datetime(&midnight)
        .single()
        .expect("Berlin midnight resolves unambiguously");
    Ok(Some(cutoff.with_timezone(&Utc).timestamp_millis()))
}

async fn execute_top(
    tracker: &EmoteTrackerHandle,
    label: &str,
    cutoff: Option<i64>,
    limit: usize,
    out: String,
) -> String {
    match tracker.top_usage_events(cutoff, limit as u32).await {
        Ok(rows) => {
            let mut out = out;
            out.push_str(&format!("Top {} Emotes ({label}):\n", rows.len()));
            for (i, r) in rows.iter().enumerate() {
                out.push_str(&format!(
                    "{}. {} — {}x, zuletzt {}\n",
                    i + 1,
                    r.name,
                    r.count,
                    format_berlin_millis(r.last_used_at),
                ));
            }
            out
        }
        Err(error) => {
            tracing::warn!(?error, "emote_stats top query failed");
            "{\"error\":\"tracker unavailable\"}".to_string()
        }
    }
}

async fn execute_by_user(
    tracker: &EmoteTrackerHandle,
    user: &str,
    label: &str,
    cutoff: Option<i64>,
    limit: usize,
) -> String {
    // Logins are the model-facing identifier; resolve to the stable Twitch id.
    // An unknown login falls through as-is so numeric ids keep working — an
    // unseen id simply answers zero instead of claiming the viewer is wrong.
    let uid = match tracker.resolve_viewer(user).await {
        Ok(Some(id)) => id,
        Ok(None) => user.to_owned(),
        Err(error) => {
            tracing::warn!(?error, "emote_stats viewer resolution failed");
            return "{\"error\":\"tracker unavailable\"}".to_string();
        }
    };
    match tracker
        .usage_count_by_user(&uid, cutoff, limit as u32)
        .await
    {
        Ok((0, _per_emote)) => {
            format!("Keine Nutzung von {user} erfasst ({label}).")
        }
        Ok((total, per_emote)) => {
            let mut out = format!("{user}: {total} Emote-Nutzungen ({label}):\n");
            for (i, r) in per_emote.iter().enumerate() {
                out.push_str(&format!(
                    "{}. {} — {}x, zuletzt {}\n",
                    i + 1,
                    r.name,
                    r.count,
                    format_berlin_millis(r.last_used_at),
                ));
            }
            out
        }
        Err(error) => {
            tracing::warn!(?error, "emote_stats per-user query failed");
            "{\"error\":\"tracker unavailable\"}".to_string()
        }
    }
}

async fn execute_of_emote(
    tracker: &EmoteTrackerHandle,
    emote_ref: &str,
    label: &str,
    cutoff: Option<i64>,
    limit: usize,
) -> String {
    let row = match tracker.resolve_emote_query(emote_ref).await {
        Ok(Some(row)) => row,
        Ok(None) => return "Unbekanntes Emote — pruefe den Code.".to_string(),
        Err(error) => {
            tracing::warn!(?error, "emote_stats emote resolution failed");
            return "{\"error\":\"tracker unavailable\"}".to_string();
        }
    };
    match tracker
        .usage_count_by_emote(&row.emote_id, cutoff, limit as u32)
        .await
    {
        Ok((0, _users)) => {
            format!("\"{}\" wurde nicht genutzt ({label}).", row.name)
        }
        Ok((total, users)) => {
            let mut out = format!(
                "\"{}\": {total} Nutzung(en) ({label}), Top {} Viewer:\n",
                row.name,
                users.len()
            );
            for (i, u) in users.iter().enumerate() {
                out.push_str(&format!(
                    "{}. {} — {}x, zuletzt {}\n",
                    i + 1,
                    u.login.as_deref().unwrap_or(&u.twitch_user_id),
                    u.count,
                    format_berlin_millis(u.last_used_at),
                ));
            }
            out
        }
        Err(error) => {
            tracing::warn!(?error, "emote_stats per-emote query failed");
            "{\"error\":\"tracker unavailable\"}".to_string()
        }
    }
}

async fn execute_set_history(tracker: &EmoteTrackerHandle, limit: usize) -> String {
    let filter = SetEventListFilter {
        limit: limit as u32,
        ..Default::default()
    };
    match tracker.list_set_events(filter).await {
        Ok(events) if events.is_empty() => "Keine Set-Aenderungen erfasst.".to_string(),
        Ok(events) => {
            let mut out = String::from("Letzte Set-Aenderungen:\n");
            for ev in events {
                out.push_str(&format!(
                    "{} — {:?} — {}\n",
                    format_berlin_millis(ev.at),
                    ev.kind,
                    format_set_event_attribution(ev.requester_login.as_deref(), ev.via.as_deref(),)
                        .unwrap_or_else(|| "ohne Attribution".to_string()),
                ));
            }
            out
        }
        Err(error) => {
            tracing::warn!(?error, "emote_stats set-history query failed");
            "{\"error\":\"tracker unavailable\"}".to_string()
        }
    }
}

/// Execute an `emote_stats` call against the tracker handle. Always returns a
/// `ToolResultMessage`; malformed args degrade to `invalid_arguments` without
/// touching the DB.
pub async fn execute_emote_stats(
    tracker: Arc<EmoteTrackerHandle>,
    call: &ToolCall,
) -> ToolResultMessage {
    let args = match call.parse_args::<EmoteStatsArgs>() {
        Ok(a) => a,
        Err(_) => return invalid(call, "erwartet kind (+ je nach kind user/emote)"),
    };

    let cutoff = match args.window_days.map(window_cutoff) {
        None => None,
        Some(result) => match result {
            Ok(cutoff) => cutoff,
            Err(error) => return invalid(call, &error.to_string()),
        },
    };
    let label = args.window_days.map_or_else(
        || "gesamte Historie".to_string(),
        |days| format!("letzte {days} Tage"),
    );
    let limit = stats_limit(args.limit);

    let body = match args.kind {
        StatsKind::Top => execute_top(&tracker, &label, cutoff, limit, String::new()).await,
        StatsKind::UsageByUser => match args.user.as_deref().map(str::trim) {
            Some("") | None => return invalid(call, "kind=usage_by_user braucht user"),
            Some(user) => execute_by_user(&tracker, user, &label, cutoff, limit).await,
        },
        StatsKind::UsageOfEmote => match args.emote.as_deref().map(str::trim) {
            Some("") | None => return invalid(call, "kind=usage_of_emote braucht emote"),
            Some(emote) => execute_of_emote(&tracker, emote, &label, cutoff, limit).await,
        },
        StatsKind::SetHistory => execute_set_history(&tracker, limit).await,
    };

    ToolResultMessage::for_call(call, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote_tracker::{DB_FILENAME, NewSetEvent, NewUsageEvent, SetEventKind};

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: EMOTE_STATS_TOOL_NAME.into(),
            arguments: args,
            arguments_parse_error: None,
        }
    }

    fn content(msg: ToolResultMessage) -> String {
        msg.content
    }

    /// Seeds emotes e1/KEKW, e2/Pepe plus current-era usage so windowed
    /// queries see them: alice(u1) uses KEKW 3x, bob(u2) once, spam-bot is
    /// flagged and never counts.
    async fn tracker() -> (Arc<EmoteTrackerHandle>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let h = Arc::new(EmoteTrackerHandle::open(&dir.path().join(DB_FILENAME)).unwrap());
        h.upsert_emote("e1", "KEKW", true, 10).await.unwrap();
        h.upsert_emote("e2", "Pepe", true, 10).await.unwrap();
        let ev = |emote: &str, uid: &str, login: &str, flagged: bool| {
            let now = Utc::now().timestamp_millis();
            NewUsageEvent {
                at: now,
                emote_id: emote.into(),
                twitch_user_id: uid.into(),
                login: Some(login.into()),
                flagged,
            }
        };
        h.insert_usage_events(vec![
            ev("e1", "u1", "Alice", false),
            ev("e1", "u1", "alice", false),
            ev("e1", "u1", "alice", false),
            ev("e1", "u2", "bob", false),
            ev("e1", "u3", "spam-bot", true),
        ])
        .await
        .unwrap();
        (h, dir)
    }

    #[test]
    fn tool_def_requires_only_kind() {
        let t = emote_stats_tool();
        assert_eq!(t.name, EMOTE_STATS_TOOL_NAME);
        assert_eq!(t.parameters["required"][0], "kind");
    }

    #[tokio::test]
    async fn usage_by_user_without_user_is_invalid() {
        let (h, _dir) = tracker().await;
        let result =
            execute_emote_stats(h, &call(serde_json::json!({"kind": "usage_by_user"}))).await;
        assert!(content(result).contains("invalid_arguments"));
    }

    #[tokio::test]
    async fn window_above_cap_is_invalid() {
        let (h, _dir) = tracker().await;
        let result = execute_emote_stats(
            h,
            &call(serde_json::json!({"kind": "top", "window_days": 31})),
        )
        .await;
        assert!(content(result).contains("invalid_arguments"));
    }

    #[tokio::test]
    async fn window_zero_is_all_time() {
        assert_eq!(window_cutoff(0).unwrap(), None);
    }

    #[tokio::test]
    async fn top_ranks_counts_excludes_flagged() {
        let (h, _dir) = tracker().await;
        // One day always covers the freshly seeded events (Berlin midnight).
        let out = content(
            execute_emote_stats(
                h,
                &call(serde_json::json!({"kind": "top", "window_days": 1})),
            )
            .await,
        );
        let kekw_line = out.lines().find(|l| l.contains("KEKW")).unwrap();
        // 3x alice + bob; the flagged bot event must not inflate the count.
        assert!(kekw_line.contains("4x"), "unflagged count expected: {out}");
        assert!(
            !out.lines().any(|l| l.contains("5x")),
            "flagged spam must not inflate: {out}"
        );
    }

    #[tokio::test]
    async fn top_limit_caps_lines() {
        let (h, _dir) = tracker().await;
        let out = content(
            execute_emote_stats(h, &call(serde_json::json!({"kind": "top", "limit": 1}))).await,
        );
        assert_eq!(out.lines().count(), 2); // header + one ranked line
    }

    #[tokio::test]
    async fn by_user_matches_login_case_insensitively() {
        let (h, _dir) = tracker().await;
        let out = content(
            execute_emote_stats(
                h,
                &call(serde_json::json!({
                    "kind": "usage_by_user",
                    "user": "ALICE"
                })),
            )
            .await,
        );
        assert!(out.contains("ALICE: 3 Emote-Nutzungen"), "{out}");
    }

    #[tokio::test]
    async fn of_emote_names_unknown_emotes_instead_of_inventing() {
        let (h, _dir) = tracker().await;
        let out = content(
            execute_emote_stats(
                h,
                &call(serde_json::json!({
                    "kind": "usage_of_emote",
                    "emote": "NopeGibtsNicht"
                })),
            )
            .await,
        );
        assert!(out.contains("Unbekanntes Emote"), "{out}");
    }

    #[tokio::test]
    async fn set_history_surfaces_attribution() {
        let (h, _dir) = tracker().await;
        h.insert_set_event(NewSetEvent {
            at: Utc::now().timestamp_millis(),
            kind: SetEventKind::Add,
            emote_id: Some("e1".into()),
            old_name: None,
            new_name: Some("KEKW".into()),
            actor_7tv_id: Some("a7".into()),
            actor_username: Some("chef".into()),
            actor_display_name: Some("Chef".into()),
            gap_from: None,
            gap_to: None,
            requester_login: Some("alice".into()),
            requester_twitch_id: Some("u1".into()),
            via: Some("potatbotat".into()),
            attribution_method: Some("potat".into()),
        })
        .await
        .unwrap();
        let out = content(
            execute_emote_stats(h, &call(serde_json::json!({"kind": "set_history"}))).await,
        );
        assert!(out.contains("Add"), "{out}");
        assert!(out.contains("alice via PotatBotat"), "{out}");
    }
}
