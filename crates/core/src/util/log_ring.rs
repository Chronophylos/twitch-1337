//! In-memory tail of recent tracing events, read by the dashboard `/logs` page.
//!
//! [`LogRingLayer`] copies every INFO-and-above event that passes the global
//! `EnvFilter` into a bounded [`LogRing`]. Nothing is persisted: a restart
//! clears the ring, and stdout stays the record.
//!
//! The layer never blocks on I/O and never panics. Field values are formatted
//! before the lock is taken, so the critical section is an O(1) push/pop.
//! `SecretString` values only ever reach the ring through their redacting
//! `Debug` impl, and the message plus every field value pass through
//! [`redact_secrets`] first, so a credential in a logged URL (a `reqwest`
//! error's `?access_key=`) never lands on the `/logs` page.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::sync::Arc;

use jiff::Timestamp;
use parking_lot::{Mutex, MutexGuard};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use super::redact::redact_secrets;

/// Default number of events kept in the ring.
pub const LOG_RING_CAPACITY: usize = 2000;

/// Longest message or field value kept, in chars. LLM error bodies can be
/// arbitrarily large; the cap bounds the ring's memory to roughly
/// `capacity × (MAX_FIELDS + 1) × MAX_VALUE_CHARS`.
const MAX_VALUE_CHARS: usize = 2048;

/// Most fields kept per event; the rest are dropped.
const MAX_FIELDS: usize = 32;

/// One captured tracing event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// Monotonic sequence number, starting at 1. Contiguous within a ring.
    pub seq: u64,
    pub at: Timestamp,
    pub level: Level,
    pub target: String,
    pub message: String,
    pub fields: Vec<(String, String)>,
}

/// Bounded FIFO of the most recent [`LogEntry`]s. The oldest entry is
/// dropped once the ring is full.
#[derive(Debug)]
pub struct LogRing {
    capacity: usize,
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    entries: VecDeque<Arc<LogEntry>>,
    next_seq: u64,
}

impl Default for LogRing {
    fn default() -> Self {
        Self::new(LOG_RING_CAPACITY)
    }
}

impl LogRing {
    /// An empty ring holding at most `capacity` entries (at least 1).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            inner: Mutex::new(Inner {
                entries: VecDeque::with_capacity(capacity),
                next_seq: 1,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock()
    }

    /// Append an entry, evicting the oldest when full. Returns its `seq`.
    pub fn push(
        &self,
        at: Timestamp,
        level: Level,
        target: String,
        message: String,
        fields: Vec<(String, String)>,
    ) -> u64 {
        let mut inner = self.lock();
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.entries.len() >= self.capacity {
            inner.entries.pop_front();
        }
        inner.entries.push_back(Arc::new(LogEntry {
            seq,
            at,
            level,
            target,
            message,
            fields,
        }));
        seq
    }

    /// Entries with `seq > after`, oldest first. `after = 0` returns the whole
    /// ring.
    pub fn after(&self, after: u64) -> Vec<Arc<LogEntry>> {
        let inner = self.lock();
        let start = inner.entries.partition_point(|e| e.seq <= after);
        inner.entries.range(start..).cloned().collect()
    }

    /// Every entry currently held, oldest first.
    pub fn snapshot(&self) -> Vec<Arc<LogEntry>> {
        self.after(0)
    }

    /// `seq` of the newest entry ever pushed, or 0 when nothing was.
    pub fn last_seq(&self) -> u64 {
        self.lock().next_seq - 1
    }
}

/// `tracing_subscriber` layer feeding a [`LogRing`] with INFO+ events.
pub struct LogRingLayer {
    ring: Arc<LogRing>,
}

impl LogRingLayer {
    pub fn new(ring: Arc<LogRing>) -> Self {
        Self { ring }
    }
}

impl<S: Subscriber> Layer<S> for LogRingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        // `Level` orders verbose levels higher: TRACE > DEBUG > INFO.
        if *meta.level() > Level::INFO {
            return;
        }
        let mut visitor = EntryVisitor::default();
        event.record(&mut visitor);
        // Events bridged from the `log` crate arrive with target "log" and
        // carry the real one in a `log.target` field.
        let target = visitor
            .log_target
            .unwrap_or_else(|| meta.target().to_owned());
        self.ring.push(
            Timestamp::now(),
            *meta.level(),
            target,
            visitor.message,
            visitor.fields,
        );
    }
}

#[derive(Default)]
struct EntryVisitor {
    message: String,
    fields: Vec<(String, String)>,
    log_target: Option<String>,
}

impl EntryVisitor {
    fn store(&mut self, field: &Field, value: String) {
        let value = match redact_secrets(&value) {
            Cow::Owned(redacted) => redacted,
            Cow::Borrowed(_) => value,
        };
        let name = field.name();
        if name == "message" {
            self.message = clamp(value);
        } else if name == "log.target" {
            self.log_target = Some(clamp(value));
        } else if name.starts_with("log.") {
            // `log.module_path` / `log.file` / `log.line`: bridge noise.
        } else if self.fields.len() < MAX_FIELDS {
            self.fields.push((name.to_owned(), clamp(value)));
        }
    }
}

impl Visit for EntryVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.store(field, value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let mut s = String::new();
        // Writing into a String only fails if the Debug impl itself errors;
        // keep whatever was written.
        #[expect(
            clippy::let_underscore_must_use,
            reason = "fmt::Write into a String only fails if the Debug impl errors; keep the partial output"
        )]
        let _ = write!(s, "{value:?}");
        self.store(field, s);
    }
}

/// Cut `s` to [`MAX_VALUE_CHARS`] chars, marking the cut with `…`.
fn clamp(mut s: String) -> String {
    if let Some((idx, _)) = s.char_indices().nth(MAX_VALUE_CHARS) {
        s.truncate(idx);
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use tracing_subscriber::prelude::*;

    use super::*;

    fn push(ring: &LogRing, msg: &str) -> u64 {
        ring.push(
            Timestamp::now(),
            Level::INFO,
            "t".to_owned(),
            msg.to_owned(),
            Vec::new(),
        )
    }

    fn messages(entries: &[Arc<LogEntry>]) -> Vec<&str> {
        entries.iter().map(|e| e.message.as_str()).collect()
    }

    #[test]
    fn full_ring_drops_oldest() {
        let ring = LogRing::new(3);
        for msg in ["a", "b", "c", "d", "e"] {
            push(&ring, msg);
        }
        let snap = ring.snapshot();
        assert_eq!(messages(&snap), ["c", "d", "e"]);
        assert_eq!(
            snap.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [3, 4, 5],
            "seq keeps counting across evictions"
        );
        assert_eq!(ring.last_seq(), 5);
    }

    #[test]
    fn after_returns_only_newer_entries() {
        let ring = LogRing::new(10);
        assert_eq!(ring.last_seq(), 0);
        assert!(ring.after(0).is_empty());
        for msg in ["a", "b", "c", "d"] {
            push(&ring, msg);
        }
        assert_eq!(messages(&ring.after(2)), ["c", "d"]);
        assert!(ring.after(4).is_empty(), "nothing newer than the last seq");
        assert!(ring.after(99).is_empty(), "a future seq yields nothing");
        assert_eq!(messages(&ring.after(0)), ["a", "b", "c", "d"]);
    }

    #[test]
    fn after_an_evicted_seq_returns_what_is_left() {
        let ring = LogRing::new(2);
        for msg in ["a", "b", "c", "d"] {
            push(&ring, msg);
        }
        assert_eq!(messages(&ring.after(1)), ["c", "d"]);
    }

    #[test]
    fn clamp_cuts_long_values_on_a_char_boundary() {
        let long = "ä".repeat(MAX_VALUE_CHARS + 5);
        let out = clamp(long);
        assert_eq!(out.chars().count(), MAX_VALUE_CHARS + 1);
        assert!(out.ends_with('…'));
        assert_eq!(clamp("short".to_owned()), "short");
    }

    #[test]
    fn layer_captures_info_and_above_with_fields() {
        let ring = Arc::new(LogRing::new(10));
        let subscriber = tracing_subscriber::registry().with(LogRingLayer::new(ring.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "tst", "dropped");
            tracing::info!(target: "tst", user = "alice", n = 3, "hello");
            tracing::error!(target: "tst::sub", "boom");
        });
        let snap = ring.snapshot();
        assert_eq!(messages(&snap), ["hello", "boom"]);
        assert_eq!(snap[0].target, "tst");
        assert_eq!(snap[0].level, Level::INFO);
        assert_eq!(
            snap[0].fields,
            [
                ("user".to_owned(), "alice".to_owned()),
                ("n".to_owned(), "3".to_owned()),
            ]
        );
        assert_eq!(snap[1].level, Level::ERROR);
        assert_eq!(snap[1].target, "tst::sub");
    }

    #[test]
    fn layer_redacts_secret_query_values_in_message_and_fields() {
        const URL: &str = "https://id.twitch.tv/oauth2/token?access_key=SECRET1&client_secret=SECRET2&refresh_token=SECRET3&grant_type=refresh_token";
        let ring = Arc::new(LogRing::new(10));
        let subscriber = tracing_subscriber::registry().with(LogRingLayer::new(ring.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let error = format!("error sending request for url ({URL}): timed out");
            tracing::warn!(target: "tst", %error, "token refresh failed for {URL}");
        });
        let snap = ring.snapshot();
        let redacted = "https://id.twitch.tv/oauth2/token?access_key=<redacted>&client_secret=<redacted>&refresh_token=<redacted>&grant_type=refresh_token";
        assert_eq!(
            snap[0].message,
            format!("token refresh failed for {redacted}")
        );
        assert_eq!(
            snap[0].fields,
            [(
                "error".to_owned(),
                format!("error sending request for url ({redacted}): timed out"),
            )]
        );
        let stored = format!("{:?}", snap[0]);
        for secret in ["SECRET1", "SECRET2", "SECRET3"] {
            assert!(!stored.contains(secret), "{secret} leaked: {stored}");
        }
    }
}
