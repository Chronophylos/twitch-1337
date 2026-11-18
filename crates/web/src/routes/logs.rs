//! `/logs` — owner-only tail of the in-memory tracing ring.
//!
//! A full page load renders the ring as a snapshot. The page then polls
//! `/logs?after=<seq>` (htmx, every 2s); that request answers with only the
//! newer rows plus an out-of-band poller carrying the advanced cursor, or 204
//! when nothing new arrived. Owner only: log lines carry chat text, user ids
//! and LLM error bodies.

use std::collections::BTreeSet;
use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use tracing::Level;
use twitch_1337_core::util::log_ring::LogEntry;
use twitch_1337_core::util::to_berlin;

use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::WebError;
use crate::nav;
use crate::routes::render;
use crate::state::WebState;

pub fn router() -> Router<WebState> {
    Router::new().route("/logs", get(logs))
}

#[derive(Debug, Default, Deserialize)]
pub struct LogsQuery {
    /// Poll cursor: only entries with a greater `seq` are returned.
    pub after: Option<u64>,
    /// Minimum level: `info` (default), `warn` or `error`.
    pub level: Option<String>,
    /// Target prefix, e.g. `twitch_1337_core::ai`.
    pub target: Option<String>,
    /// Case-insensitive substring over target, message and fields.
    pub q: Option<String>,
}

/// Parsed filters; also rebuilds the poll URL so polling keeps them.
struct Filter {
    min_level: Level,
    target: String,
    q: String,
    q_lower: String,
}

impl Filter {
    fn from_query(query: &LogsQuery) -> Self {
        let min_level = match query.level.as_deref() {
            Some("warn") => Level::WARN,
            Some("error") => Level::ERROR,
            _ => Level::INFO,
        };
        let target = query.target.as_deref().unwrap_or("").trim().to_owned();
        let q = query.q.as_deref().unwrap_or("").trim().to_owned();
        let q_lower = q.to_lowercase();
        Self {
            min_level,
            target,
            q,
            q_lower,
        }
    }

    fn level_param(&self) -> &'static str {
        match self.min_level {
            Level::ERROR => "error",
            Level::WARN => "warn",
            _ => "info",
        }
    }

    fn matches(&self, e: &LogEntry) -> bool {
        // `Level` orders more severe levels lower: ERROR < WARN < INFO.
        e.level <= self.min_level
            && e.target.starts_with(&self.target)
            && (self.q_lower.is_empty() || self.text_matches(e))
    }

    fn text_matches(&self, e: &LogEntry) -> bool {
        let hit = |s: &str| s.to_lowercase().contains(&self.q_lower);
        hit(&e.message) || hit(&e.target) || e.fields.iter().any(|(k, v)| hit(k) || hit(v))
    }

    fn poll_url(&self, after: u64) -> String {
        let mut url = format!("/logs?after={after}");
        if self.min_level != Level::INFO {
            url.push_str("&level=");
            url.push_str(self.level_param());
        }
        if !self.target.is_empty() {
            url.push_str("&target=");
            url.push_str(&urlencoding::encode(&self.target));
        }
        if !self.q.is_empty() {
            url.push_str("&q=");
            url.push_str(&urlencoding::encode(&self.q));
        }
        url
    }
}

/// Template view over one ring entry; formats on demand instead of copying.
struct Row(Arc<LogEntry>);

impl Row {
    fn entry(&self) -> &LogEntry {
        &self.0
    }

    /// `HH:MM:SS.mmm`, Berlin local — same clock as stdout.
    fn time(&self) -> String {
        to_berlin(self.0.at).strftime("%H:%M:%S%.3f").to_string()
    }

    fn full_time(&self) -> String {
        to_berlin(self.0.at)
            .strftime("%Y-%m-%d %H:%M:%S%.3f %Z")
            .to_string()
    }

    fn level(&self) -> &'static str {
        self.0.level.as_str()
    }

    fn level_class(&self) -> &'static str {
        match self.0.level {
            Level::ERROR => "error",
            Level::WARN => "warn",
            _ => "info",
        }
    }
}

#[derive(Template)]
#[template(path = "logs/index.html")]
struct LogsTpl {
    rows: Vec<Row>,
    poll_url: String,
    oob: bool,
    level: &'static str,
    target: String,
    q: String,
    targets: Vec<String>,
    csrf: String,
    user_login: String,
    user_avatar_url: Option<String>,
    current_page: &'static str,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
}

#[derive(Template)]
#[template(path = "logs/_rows.html")]
struct RowsTpl {
    rows: Vec<Row>,
    poll_url: String,
    oob: bool,
}

async fn logs(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    headers: HeaderMap,
    Query(query): Query<LogsQuery>,
) -> Result<Response, WebError> {
    let filter = Filter::from_query(&query);
    let is_htmx = headers
        .get("HX-Request")
        .is_some_and(|v| v.as_bytes() == b"true");

    if let (true, Some(after)) = (is_htmx, query.after) {
        // A cursor past the newest seq means the process restarted and the
        // ring's seq began again at 1; resend the new ring from the start.
        let after = if after > state.log_ring.last_seq() {
            0
        } else {
            after
        };
        let entries = state.log_ring.after(after);
        let Some(last) = entries.last().map(|e| e.seq) else {
            // htmx swaps nothing on 204, so the poller and its cursor stay.
            return Ok(StatusCode::NO_CONTENT.into_response());
        };
        // The cursor advances past filtered-out rows too, so they are not
        // re-scanned on every tick.
        return render(&RowsTpl {
            rows: entries
                .into_iter()
                .filter(|e| filter.matches(e))
                .map(Row)
                .collect(),
            poll_url: filter.poll_url(last),
            oob: true,
        });
    }

    let entries = state.log_ring.snapshot();
    let cursor = entries.last().map_or(0, |e| e.seq);
    let targets: Vec<String> = entries
        .iter()
        .map(|e| e.target.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let rows = entries
        .into_iter()
        .filter(|e| filter.matches(e))
        .map(Row)
        .collect();
    render(&LogsTpl {
        rows,
        poll_url: filter.poll_url(cursor),
        oob: false,
        level: filter.level_param(),
        target: filter.target,
        q: filter.q,
        targets,
        csrf: csrf::encode(&session.csrf_value),
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, crate::auth::Role::Owner),
        user_avatar_url: session.avatar_url,
        user_login: session.user_login,
        current_page: nav::LOGS,
    })
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;

    use super::*;

    fn entry(level: Level, target: &str, message: &str, fields: &[(&str, &str)]) -> LogEntry {
        LogEntry {
            seq: 1,
            at: Timestamp::now(),
            level,
            target: target.to_owned(),
            message: message.to_owned(),
            fields: fields
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    fn filter(level: Option<&str>, target: Option<&str>, q: Option<&str>) -> Filter {
        Filter::from_query(&LogsQuery {
            after: None,
            level: level.map(str::to_owned),
            target: target.map(str::to_owned),
            q: q.map(str::to_owned),
        })
    }

    #[test]
    fn level_filter_keeps_at_least_as_severe() {
        let f = filter(Some("warn"), None, None);
        assert!(!f.matches(&entry(Level::INFO, "a", "m", &[])));
        assert!(f.matches(&entry(Level::WARN, "a", "m", &[])));
        assert!(f.matches(&entry(Level::ERROR, "a", "m", &[])));
    }

    #[test]
    fn target_filter_is_a_prefix_match() {
        let f = filter(None, Some("core::ai"), None);
        assert!(f.matches(&entry(Level::INFO, "core::ai::memory", "m", &[])));
        assert!(!f.matches(&entry(Level::INFO, "core::ping", "m", &[])));
    }

    #[test]
    fn text_filter_searches_message_and_fields_case_insensitively() {
        let f = filter(None, None, Some("ALICE"));
        assert!(f.matches(&entry(Level::INFO, "a", "hello alice", &[])));
        assert!(f.matches(&entry(Level::INFO, "a", "m", &[("user", "Alice")])));
        assert!(!f.matches(&entry(Level::INFO, "a", "m", &[("user", "bob")])));
    }

    #[test]
    fn poll_url_carries_cursor_and_non_default_filters() {
        assert_eq!(filter(None, None, None).poll_url(7), "/logs?after=7");
        assert_eq!(
            filter(Some("error"), Some("a::b"), Some("x y")).poll_url(9),
            "/logs?after=9&level=error&target=a%3A%3Ab&q=x%20y"
        );
    }
}
