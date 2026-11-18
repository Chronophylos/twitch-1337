//! On-demand `emote_stats` chat-turn tool over the emote tracker DB.

use std::sync::Arc;

use jiff::{Span, Timestamp};
use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use serde::Deserialize;

use crate::emote_tracker::{
    EmoteTrackerHandle, EmoteUsageCount, SetEventKind, SetEventListFilter, SetEventRow, SetTenure,
    format_berlin_millis, format_set_event_by, lifecycle_verb,
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
    LeastUsed,
    TopUsers,
    UsageByUser,
    UsageOfEmote,
    SetHistory,
}

/// Lifecycle kinds the model may filter `set_history` by. Bootstrap and gap
/// rows are tracker bookkeeping, never "changes" a viewer asks about.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChangeKind {
    Add,
    Remove,
    Rename,
}

impl From<ChangeKind> for SetEventKind {
    fn from(kind: ChangeKind) -> Self {
        match kind {
            ChangeKind::Add => Self::Add,
            ChangeKind::Remove => Self::Remove,
            ChangeKind::Rename => Self::Rename,
        }
    }
}

#[derive(Debug, Deserialize)]
struct EmoteStatsArgs {
    kind: StatsKind,
    /// Viewer login (or Twitch user id) for `usage_by_user`; requester or
    /// 7TV editor login filter for `set_history`.
    #[serde(default)]
    user: Option<String>,
    /// Emote code (or fragment/id) for `usage_of_emote` and `set_history`.
    #[serde(default)]
    emote: Option<String>,
    /// Change kind filter for `set_history`.
    #[serde(default)]
    change: Option<ChangeKind>,
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
            Emotes im Zeitraum. kind=least_used: am wenigsten oder nie genutzte Emotes im \
            aktuellen Set (gezählt erst ab Tracking-Start). kind=top_users: Viewer mit den \
            meisten Emote-Nutzungen. kind=usage_by_user mit user: was und wie oft ein bestimmter \
            Viewer nutzt. kind=usage_of_emote mit emote: wer und wie oft ein Emote nutzt. \
            kind=set_history: Set-Änderungen (hinzugefügt/entfernt/umbenannt) mit 7TV-Editor \
            und Attribution, optional gefiltert nach emote, change und user (Requester oder \
            Editor). Optional window_days (1..=30, Berlin-Tage, ohne Angabe: alles) und limit. \
            '(entfernt)' heißt, das Emote ist nicht mehr im Set. \
            Erfinde niemals Zahlen — nimm exakt die gelieferten Werte."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": [
                        "top",
                        "least_used",
                        "top_users",
                        "usage_by_user",
                        "usage_of_emote",
                        "set_history"
                    ],
                },
                "user": { "type": "string" },
                "emote": { "type": "string" },
                "change": { "type": "string", "enum": ["add", "remove", "rename"] },
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
    let today = crate::util::to_berlin(Timestamp::now());
    let cutoff_date = today
        .date()
        .checked_sub(Span::new().days(i64::from(days - 1)))?;
    let midnight = cutoff_date.at(0, 0, 0, 0);
    let cutoff = crate::util::BERLIN
        .to_ambiguous_timestamp(midnight)
        .unambiguous()
        .expect("Berlin midnight resolves unambiguously");
    Ok(Some(cutoff.as_millisecond()))
}

fn removed_marker(active: bool) -> &'static str {
    if active { "" } else { " (entfernt)" }
}

/// Ranked `name — Nx, zuletzt …` lines shared by `top` and `usage_by_user`.
fn push_emote_counts(out: &mut String, rows: &[EmoteUsageCount]) {
    for (i, r) in rows.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}{} — {}x, zuletzt {}\n",
            i + 1,
            r.name,
            removed_marker(r.active),
            r.count,
            format_berlin_millis(r.last_used_at),
        ));
    }
}

/// Marks a time the tracker only knows as "noticed at", not "happened at".
const APPROX_TIME: &str = "Zeitpunkt ungefähr, beim Abgleich erkannt";

/// One `set_history` line: when, what changed and who did it. `None` for
/// bootstrap/gap bookkeeping rows.
fn format_set_event(ev: &SetEventRow) -> Option<String> {
    let verb = lifecycle_verb(ev.kind)?;
    let what = if ev.kind == SetEventKind::Rename {
        format!(
            "{} → {}",
            ev.old_name.as_deref().unwrap_or("?"),
            ev.new_name.as_deref().unwrap_or("?"),
        )
    } else {
        ev.new_name
            .as_deref()
            .or(ev.old_name.as_deref())
            .or(ev.emote_id.as_deref())
            .unwrap_or("?")
            .to_string()
    };
    let mut line = format!(
        "{} — {what} {verb} — von {}",
        format_berlin_millis(ev.at),
        format_set_event_by(ev),
    );
    if ev.is_snapshot_diff() {
        line.push_str(&format!(" ({APPROX_TIME})"));
    }
    Some(line)
}

async fn execute_top(
    tracker: &EmoteTrackerHandle,
    label: &str,
    cutoff: Option<i64>,
    limit: usize,
) -> String {
    match tracker.top_usage_events(cutoff, limit as u32).await {
        Ok(rows) => {
            let mut out = format!("Top {} Emotes ({label}):\n", rows.len());
            push_emote_counts(&mut out, &rows);
            out
        }
        Err(error) => {
            tracing::warn!(?error, "emote_stats top query failed");
            "{\"error\":\"tracker unavailable\"}".to_string()
        }
    }
}

async fn execute_least_used(
    tracker: &EmoteTrackerHandle,
    label: &str,
    cutoff: Option<i64>,
    limit: usize,
) -> String {
    match tracker.least_used_emotes(cutoff, limit as u32).await {
        Ok((_, rows)) if rows.is_empty() => "Keine Emotes im Set erfasst.".to_string(),
        Ok((unused, rows)) => {
            let mut out = format!(
                "{unused} Emotes im Set ohne Nutzung ({label}, gezählt ab Tracking-Start). \
                 Am wenigsten genutzt:\n"
            );
            for (i, r) in rows.iter().enumerate() {
                // Inside a window "no use" says nothing about older history.
                let last = match (r.last_used_at, cutoff) {
                    (Some(at), _) => format_berlin_millis(at),
                    (None, Some(_)) => "nicht im Zeitraum".to_string(),
                    (None, None) => "nie".to_string(),
                };
                let since = match r.tenure {
                    Some(SetTenure::TrackingStart) => "seit Tracking-Start".to_string(),
                    Some(SetTenure::Added { at, approximate }) => {
                        let at = format_berlin_millis(at);
                        if approximate {
                            format!("seit ca. {at} ({APPROX_TIME})")
                        } else {
                            format!("seit {at}")
                        }
                    }
                    None => "seit unbekannt".to_string(),
                };
                out.push_str(&format!(
                    "{}. {} — {}x, zuletzt {last}, im Set {since}\n",
                    i + 1,
                    r.name,
                    r.count,
                ));
            }
            out
        }
        Err(error) => {
            tracing::warn!(?error, "emote_stats least-used query failed");
            "{\"error\":\"tracker unavailable\"}".to_string()
        }
    }
}

async fn execute_top_users(
    tracker: &EmoteTrackerHandle,
    label: &str,
    cutoff: Option<i64>,
    limit: usize,
) -> String {
    match tracker.top_users(cutoff, limit as u32).await {
        Ok(users) if users.is_empty() => format!("Keine Emote-Nutzung erfasst ({label})."),
        Ok(users) => {
            let mut out = format!("Top {} Viewer nach Emote-Nutzung ({label}):\n", users.len());
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
            tracing::warn!(?error, "emote_stats top-users query failed");
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
            push_emote_counts(&mut out, &per_emote);
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
        Ok(None) => return "Unbekanntes Emote — prüfe den Code.".to_string(),
        Err(error) => {
            tracing::warn!(?error, "emote_stats emote resolution failed");
            return "{\"error\":\"tracker unavailable\"}".to_string();
        }
    };
    let marker = removed_marker(row.active);
    match tracker
        .usage_count_by_emote(&row.emote_id, cutoff, limit as u32)
        .await
    {
        Ok((0, _users)) => {
            format!("\"{}\"{marker} wurde nicht genutzt ({label}).", row.name)
        }
        Ok((total, users)) => {
            let mut out = format!(
                "\"{}\"{marker}: {total} Nutzung(en) ({label}), Top {} Viewer:\n",
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

struct SetHistoryQuery<'a> {
    emote: Option<&'a str>,
    user: Option<&'a str>,
    change: Option<ChangeKind>,
    cutoff: Option<i64>,
    label: &'a str,
    limit: usize,
}

async fn execute_set_history(tracker: &EmoteTrackerHandle, q: SetHistoryQuery<'_>) -> String {
    // Resolving through the catalog (not a name substring) follows renames
    // and finds removed emotes by their last name; the exact-name match adds
    // other 7TV ids that carried the same code (version swaps).
    let emote = match q.emote {
        None => None,
        Some(emote_ref) => match tracker.resolve_emote_query(emote_ref).await {
            Ok(Some(row)) => Some(row),
            Ok(None) => return "Unbekanntes Emote — prüfe den Code.".to_string(),
            Err(error) => {
                tracing::warn!(?error, "emote_stats emote resolution failed");
                return "{\"error\":\"tracker unavailable\"}".to_string();
            }
        },
    };
    let filter = SetEventListFilter {
        kind: q.change.map(SetEventKind::from),
        emote_name: emote.as_ref().map(|row| row.name.clone()),
        emote_id: emote.map(|row| row.emote_id),
        since_ms: q.cutoff,
        lifecycle_only: true,
        user: q.user.map(str::to_owned),
        limit: q.limit as u32,
        ..Default::default()
    };
    let label = q.label;
    match tracker.list_set_events(filter).await {
        Ok(events) if events.is_empty() => format!("Keine Set-Änderungen erfasst ({label})."),
        Ok(events) => {
            let mut out = format!("Letzte Set-Änderungen ({label}):\n");
            for line in events.iter().filter_map(format_set_event) {
                out.push_str(&line);
                out.push('\n');
            }
            out
        }
        Err(error) => {
            tracing::warn!(?error, "emote_stats set-history query failed");
            "{\"error\":\"tracker unavailable\"}".to_string()
        }
    }
}

fn non_blank(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
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
        Err(error) => {
            // serde names the offending field and its allowed values.
            return invalid(
                call,
                &format!("erwartet kind (+ je nach kind user/emote/change): {error}"),
            );
        }
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
    // Chat habit: viewers are named as `@login`.
    let user = non_blank(
        args.user
            .as_deref()
            .map(|u| u.trim().trim_start_matches('@')),
    );
    let emote = non_blank(args.emote.as_deref());

    let body = match args.kind {
        StatsKind::Top => execute_top(&tracker, &label, cutoff, limit).await,
        StatsKind::LeastUsed => execute_least_used(&tracker, &label, cutoff, limit).await,
        StatsKind::TopUsers => execute_top_users(&tracker, &label, cutoff, limit).await,
        StatsKind::UsageByUser => match user {
            None => return invalid(call, "kind=usage_by_user braucht user"),
            Some(user) => execute_by_user(&tracker, user, &label, cutoff, limit).await,
        },
        StatsKind::UsageOfEmote => match emote {
            None => return invalid(call, "kind=usage_of_emote braucht emote"),
            Some(emote) => execute_of_emote(&tracker, emote, &label, cutoff, limit).await,
        },
        StatsKind::SetHistory => {
            let query = SetHistoryQuery {
                emote,
                user,
                change: args.change,
                cutoff,
                label: &label,
                limit,
            };
            execute_set_history(&tracker, query).await
        }
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
            let now = Timestamp::now().as_millisecond();
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
            at: Timestamp::now().as_millisecond(),
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
        // Same "von" rule as !emote: the chat requester wins over the 7TV editor.
        assert!(
            out.contains("KEKW hinzugefügt — von alice via PotatBotat"),
            "{out}"
        );
        assert!(
            !out.contains("ungefähr"),
            "attributed rows are exact: {out}"
        );
    }

    fn lifecycle(at: i64, kind: SetEventKind, old: Option<&str>, new: Option<&str>) -> NewSetEvent {
        NewSetEvent {
            at,
            kind,
            emote_id: Some("e1".into()),
            old_name: old.map(Into::into),
            new_name: new.map(Into::into),
            actor_7tv_id: None,
            actor_username: None,
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
            requester_login: None,
            requester_twitch_id: None,
            via: None,
            attribution_method: None,
        }
    }

    #[tokio::test]
    async fn set_history_hides_bootstrap_and_names_changes() {
        let (h, _dir) = tracker().await;
        let now = Timestamp::now().as_millisecond();
        for ev in [
            lifecycle(now - 3, SetEventKind::Bootstrap, None, Some("KEKW")),
            lifecycle(now - 2, SetEventKind::Rename, Some("KEKW"), Some("KEKL")),
            lifecycle(now - 1, SetEventKind::Remove, Some("KEKL"), None),
        ] {
            h.insert_set_event(ev).await.unwrap();
        }
        let out = content(
            execute_emote_stats(h, &call(serde_json::json!({"kind": "set_history"}))).await,
        );
        assert!(out.contains("KEKW → KEKL umbenannt"), "{out}");
        assert!(out.contains("KEKL entfernt"), "{out}");
        assert!(!out.contains("Tracking-Start"), "bootstrap leaked: {out}");
        // Snapshot-diff rows: unknown editor, detection-time stamp.
        assert!(
            out.contains("KEKL entfernt — von unbekannt (Zeitpunkt ungefähr"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn set_history_filters_by_change_and_emote() {
        let (h, _dir) = tracker().await;
        let now = Timestamp::now().as_millisecond();
        h.insert_set_event(lifecycle(now - 2, SetEventKind::Add, None, Some("KEKW")))
            .await
            .unwrap();
        h.insert_set_event(lifecycle(now - 1, SetEventKind::Remove, Some("KEKW"), None))
            .await
            .unwrap();
        let out = content(
            execute_emote_stats(
                h.clone(),
                &call(serde_json::json!({
                    "kind": "set_history",
                    "emote": "KEKW",
                    "change": "add",
                    "window_days": 1
                })),
            )
            .await,
        );
        assert!(out.contains("hinzugefügt"), "{out}");
        assert!(!out.contains("entfernt"), "{out}");
        let unknown = content(
            execute_emote_stats(
                h,
                &call(serde_json::json!({"kind": "set_history", "emote": "NopeGibtsNicht"})),
            )
            .await,
        );
        assert!(unknown.contains("Unbekanntes Emote"), "{unknown}");
    }

    #[tokio::test]
    async fn set_history_user_accepts_at_prefix() {
        let (h, _dir) = tracker().await;
        let mut ev = lifecycle(
            Timestamp::now().as_millisecond(),
            SetEventKind::Add,
            None,
            Some("KEKW"),
        );
        ev.requester_login = Some("alice".into());
        ev.via = Some("potatbotat".into());
        h.insert_set_event(ev).await.unwrap();
        let out = content(
            execute_emote_stats(
                h,
                &call(serde_json::json!({"kind": "set_history", "user": "@Alice"})),
            )
            .await,
        );
        assert!(out.contains("KEKW hinzugefügt"), "{out}");
    }

    #[tokio::test]
    async fn invalid_change_names_allowed_values() {
        let (h, _dir) = tracker().await;
        let out = content(
            execute_emote_stats(
                h,
                &call(serde_json::json!({"kind": "set_history", "change": "added"})),
            )
            .await,
        );
        assert!(out.contains("invalid_arguments"), "{out}");
        assert!(out.contains("rename"), "allowed values missing: {out}");
    }

    #[tokio::test]
    async fn least_used_in_window_does_not_claim_never() {
        let (h, _dir) = tracker().await;
        let out = content(
            execute_emote_stats(
                h,
                &call(serde_json::json!({"kind": "least_used", "window_days": 1})),
            )
            .await,
        );
        assert!(
            out.contains("Pepe — 0x, zuletzt nicht im Zeitraum"),
            "{out}"
        );
        assert!(!out.contains("zuletzt nie"), "{out}");
    }

    #[tokio::test]
    async fn top_marks_removed_emotes() {
        let (h, _dir) = tracker().await;
        h.deactivate_emote("e1", 20).await.unwrap();
        let out = content(execute_emote_stats(h, &call(serde_json::json!({"kind": "top"}))).await);
        assert!(out.contains("KEKW (entfernt) — 4x"), "{out}");
    }

    #[tokio::test]
    async fn least_used_lists_never_used_first() {
        let (h, _dir) = tracker().await;
        let out =
            content(execute_emote_stats(h, &call(serde_json::json!({"kind": "least_used"}))).await);
        assert!(out.starts_with("1 Emotes im Set ohne Nutzung"), "{out}");
        assert!(out.contains("1. Pepe — 0x, zuletzt nie"), "{out}");
    }

    #[tokio::test]
    async fn top_users_ranks_viewers() {
        let (h, _dir) = tracker().await;
        let out =
            content(execute_emote_stats(h, &call(serde_json::json!({"kind": "top_users"}))).await);
        // Seeded events share one timestamp, so either login casing may win.
        assert!(out.to_lowercase().contains("1. alice — 3x"), "{out}");
        assert!(!out.contains("spam-bot"), "{out}");
    }
}
