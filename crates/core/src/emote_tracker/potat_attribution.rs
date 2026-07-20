//! PotatBotat history attribution: soft `ago` parse, Step A gate, Step B match.

use std::time::Duration;

use eyre::{Result, WrapErr, eyre};
use serde::Deserialize;

use super::types::SetEventKind;

pub const SMALL_SLACK_SECS: u64 = 2;
pub const HEAD_SCAN_LIMIT: usize = 50;
pub const AMBIGUITY_N: usize = 2;
pub const HISTORY_RETRY_DELAY: Duration = Duration::from_millis(500);

const POTAT_HISTORY_BASE: &str = "https://api.potat.app/emotes/history";
const POTAT_ACTOR: &str = "potatbotat";
const STV_PROVIDER: &str = "STV";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PotatHistoryItem {
    pub user_login: String,
    pub actor: String,
    pub action: String,
    #[serde(rename = "type")]
    pub history_type: String,
    pub emote_id: String,
    #[serde(default)]
    pub emote_name: Option<String>,
    pub provider: String,
    #[serde(default)]
    pub ago: Option<String>,
}

/// Parse Potat `ago` strings like `"3m and 11s ago"` into total seconds.
///
/// Returns `None` when the string is missing the trailing `" ago"`, has an
/// unexpected shape, or contains unknown units. Callers treat unparsable ages
/// as eligible by list position only (soft cap does not apply).
pub fn parse_ago_secs(ago: &str) -> Option<u64> {
    let body = ago.strip_suffix(" ago")?;
    if body.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    for token in body.split(" and ") {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        let (digits, unit) = token.split_at(token.len().checked_sub(1)?);
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let n: u64 = digits.parse().ok()?;
        let secs = match unit {
            "s" => n,
            "m" => n.saturating_mul(60),
            "h" => n.saturating_mul(3600),
            "d" => n.saturating_mul(86400),
            _ => return None,
        };
        total = total.saturating_add(secs);
    }
    Some(total)
}

fn action_aligns(kind: SetEventKind, action: &str) -> bool {
    matches!(
        (kind, action),
        (SetEventKind::Add, "ADD")
            | (SetEventKind::Remove, "REMOVE")
            | (SetEventKind::Rename, "ALIAS")
    )
}

fn not_stale(item: &PotatHistoryItem, soft_cap_secs: u64) -> bool {
    match item.ago.as_deref().and_then(parse_ago_secs) {
        Some(age) => age <= soft_cap_secs,
        None => true,
    }
}

fn is_potat_stv_id_hit(item: &PotatHistoryItem, emote_id: &str, soft_cap_secs: u64) -> bool {
    item.emote_id == emote_id
        && item.actor == POTAT_ACTOR
        && item.provider == STV_PROVIDER
        && not_stale(item, soft_cap_secs)
}

/// Step A — performer-first mediation gate.
///
/// Known non-Potat `actor_7tv_id` always closes. Potat id always opens. Null
/// performer opens only when history has a qualifying STV+potatbotat+id hit
/// (action alignment not required for the gate).
pub fn mediation_gate_open(
    actor_7tv_id: Option<&str>,
    potat_7tv_user_id: &str,
    history: &[PotatHistoryItem],
    emote_id: &str,
    soft_cap_secs: u64,
) -> bool {
    match actor_7tv_id {
        Some(id) if id == potat_7tv_user_id => true,
        Some(_) => false,
        None => history
            .iter()
            .take(HEAD_SCAN_LIMIT)
            .any(|item| is_potat_stv_id_hit(item, emote_id, soft_cap_secs)),
    }
}

fn candidate_filter(
    item: &PotatHistoryItem,
    emote_id: &str,
    kind: SetEventKind,
    soft_cap_secs: u64,
) -> bool {
    is_potat_stv_id_hit(item, emote_id, soft_cap_secs) && action_aligns(kind, &item.action)
}

/// Step B — newest-first requester resolve. Returns lowercase login or `None`
/// on empty / ambiguous head (up to [`AMBIGUITY_N`] distinct logins).
pub fn resolve_requester_login(
    history: &[PotatHistoryItem],
    emote_id: &str,
    kind: SetEventKind,
    soft_cap_secs: u64,
) -> Option<String> {
    let q: Vec<&PotatHistoryItem> = history
        .iter()
        .take(HEAD_SCAN_LIMIT)
        .filter(|item| candidate_filter(item, emote_id, kind, soft_cap_secs))
        .collect();
    if q.is_empty() {
        return None;
    }
    let head_len = q.len().min(AMBIGUITY_N);
    let head = &q[..head_len];
    let first = head[0].user_login.to_ascii_lowercase();
    if head
        .iter()
        .all(|item| item.user_login.eq_ignore_ascii_case(&first))
    {
        Some(first)
    } else {
        tracing::debug!(
            emote_id,
            ?kind,
            "potat attribution ambiguous head logins; skipping"
        );
        None
    }
}

#[derive(Debug, Deserialize)]
struct PotatHistoryEnvelope {
    #[serde(default)]
    data: Vec<PotatHistoryData>,
    #[serde(default)]
    history: Option<Vec<PotatHistoryItem>>,
}

#[derive(Debug, Deserialize)]
struct PotatHistoryData {
    #[serde(default)]
    history: Vec<PotatHistoryItem>,
}

fn history_from_envelope(env: PotatHistoryEnvelope) -> Vec<PotatHistoryItem> {
    if let Some(top) = env.history {
        return top.into_iter().take(HEAD_SCAN_LIMIT).collect();
    }
    env.data
        .into_iter()
        .next()
        .map(|d| d.history)
        .unwrap_or_default()
        .into_iter()
        .take(HEAD_SCAN_LIMIT)
        .collect()
}

/// `GET https://api.potat.app/emotes/history/{channel_login}` — first page only.
pub async fn fetch_potat_history(
    http: &reqwest::Client,
    channel_login: &str,
) -> Result<Vec<PotatHistoryItem>> {
    let url = format!("{POTAT_HISTORY_BASE}/{channel_login}");
    let resp = http
        .get(&url)
        .send()
        .await
        .wrap_err("potat history request")?;
    if !resp.status().is_success() {
        return Err(eyre!("potat history HTTP {}", resp.status()));
    }
    let env: PotatHistoryEnvelope = resp.json().await.wrap_err("potat history JSON")?;
    Ok(history_from_envelope(env))
}

/// Parse a fixture / mock JSON body into history items (test + ingest helpers).
pub fn parse_potat_history_json(raw: &str) -> Result<Vec<PotatHistoryItem>> {
    let env: PotatHistoryEnvelope = serde_json::from_str(raw).wrap_err("potat history JSON")?;
    Ok(history_from_envelope(env))
}

#[cfg(test)]
mod tests {
    use super::*;

    const POTAT_ID: &str = "01GNRG2MDG0000DM7N933JH49Y";

    fn load_fixture(name: &str) -> Vec<PotatHistoryItem> {
        let raw = match name {
            "standard" => include_str!("fixtures/potat_history_standard.json"),
            "undo" => include_str!("fixtures/potat_history_undo.json"),
            "ambiguous" => include_str!("fixtures/potat_history_ambiguous.json"),
            "non_stv" => include_str!("fixtures/potat_history_non_stv.json"),
            _ => panic!("unknown fixture {name}"),
        };
        parse_potat_history_json(raw).expect("fixture parses")
    }

    fn history_with_potat_hit(emote_id: &str) -> Vec<PotatHistoryItem> {
        vec![PotatHistoryItem {
            user_login: "ringomar".into(),
            actor: POTAT_ACTOR.into(),
            action: "ADD".into(),
            history_type: "STANDARD".into(),
            emote_id: emote_id.into(),
            emote_name: Some("x".into()),
            provider: STV_PROVIDER.into(),
            ago: Some("5s ago".into()),
        }]
    }

    #[test]
    fn parse_ago_secs_sums_units() {
        assert_eq!(parse_ago_secs("3m and 11s ago"), Some(3 * 60 + 11));
        assert_eq!(parse_ago_secs("1d and 16h ago"), Some(86400 + 16 * 3600));
        assert_eq!(parse_ago_secs("20m and 38s ago"), Some(20 * 60 + 38));
        assert_eq!(parse_ago_secs("5s ago"), Some(5));
        assert_eq!(parse_ago_secs("weird"), None);
        assert_eq!(parse_ago_secs("3m ago"), Some(180));
        assert_eq!(parse_ago_secs("3x ago"), None);
    }

    #[test]
    fn gate_closed_for_known_non_potat_performer() {
        assert!(!mediation_gate_open(
            Some("01HUMAN"),
            POTAT_ID,
            &history_with_potat_hit("e1"),
            "e1",
            17,
        ));
    }

    #[test]
    fn gate_open_for_potat_performer() {
        assert!(mediation_gate_open(Some(POTAT_ID), POTAT_ID, &[], "e1", 17,));
    }

    #[test]
    fn gate_open_for_null_performer_with_history_hit() {
        assert!(mediation_gate_open(
            None,
            POTAT_ID,
            &history_with_potat_hit("e1"),
            "e1",
            17,
        ));
    }

    #[test]
    fn gate_closed_for_null_performer_without_hit() {
        assert!(!mediation_gate_open(None, POTAT_ID, &[], "e1", 17));
    }

    #[test]
    fn step_b_unique_head_login_wins() {
        let history = load_fixture("standard");
        assert_eq!(
            resolve_requester_login(&history, "e1", SetEventKind::Add, 17).as_deref(),
            Some("ringomar")
        );
    }

    #[test]
    fn step_b_undo_remove_aligns() {
        let history = load_fixture("undo");
        assert_eq!(
            resolve_requester_login(&history, "e1", SetEventKind::Remove, 17).as_deref(),
            Some("chronophylos")
        );
    }

    #[test]
    fn step_b_two_distinct_head_logins_null() {
        let history = load_fixture("ambiguous");
        assert_eq!(
            resolve_requester_login(&history, "e1", SetEventKind::Add, 17),
            None
        );
    }

    #[test]
    fn step_b_rejects_non_stv_and_external_actor() {
        let history = load_fixture("non_stv");
        assert_eq!(
            resolve_requester_login(&history, "e1", SetEventKind::Add, 17),
            None
        );
    }

    #[test]
    fn step_b_stale_ago_skipped_unparsable_kept() {
        let mut history = history_with_potat_hit("e1");
        history[0].ago = Some("1d and 16h ago".into());
        assert_eq!(
            resolve_requester_login(&history, "e1", SetEventKind::Add, 17),
            None
        );

        history[0].ago = Some("garbled".into());
        assert_eq!(
            resolve_requester_login(&history, "e1", SetEventKind::Add, 17).as_deref(),
            Some("ringomar")
        );

        history[0].ago = None;
        assert_eq!(
            resolve_requester_login(&history, "e1", SetEventKind::Add, 17).as_deref(),
            Some("ringomar")
        );
    }

    #[test]
    fn step_b_same_login_twice_in_head_wins() {
        let history = vec![
            PotatHistoryItem {
                user_login: "Alice".into(),
                actor: POTAT_ACTOR.into(),
                action: "ADD".into(),
                history_type: "STANDARD".into(),
                emote_id: "e1".into(),
                emote_name: None,
                provider: STV_PROVIDER.into(),
                ago: Some("5s ago".into()),
            },
            PotatHistoryItem {
                user_login: "alice".into(),
                actor: POTAT_ACTOR.into(),
                action: "ADD".into(),
                history_type: "REDO".into(),
                emote_id: "e1".into(),
                emote_name: None,
                provider: STV_PROVIDER.into(),
                ago: Some("8s ago".into()),
            },
        ];
        assert_eq!(
            resolve_requester_login(&history, "e1", SetEventKind::Add, 17).as_deref(),
            Some("alice")
        );
    }

    #[test]
    fn step_b_ignores_bootstrap_and_wrong_action() {
        let history = history_with_potat_hit("e1");
        assert_eq!(
            resolve_requester_login(&history, "e1", SetEventKind::Remove, 17),
            None
        );
        assert_eq!(
            resolve_requester_login(&history, "e1", SetEventKind::Bootstrap, 17),
            None
        );
    }
}
