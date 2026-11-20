//! Tracing / observability initialisation.

use std::io;
use std::sync::Arc;

use jiff::Timestamp;
use tracing::Metadata;
use tracing_error::ErrorLayer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

use super::log_ring::{LogRing, LogRingLayer};
use super::redact::redact_secrets;

/// `HH:MM:SS.mmm` in Europe/Berlin. The default `SystemTime` formatter emits
/// the full RFC3339 timestamp on every line (~30 chars including date + tz +
/// microseconds), which is overkill for a single-process bot whose logs
/// always belong to today. The short form preserves millisecond precision —
/// enough to order events within a handler — while leaving room for the
/// actual message content. Berlin local time keeps log timestamps on the
/// same clock as the bot's domain logic (1337 tracker, schedules, dreamer),
/// so the 13:37 event logs as 13:37.
struct ShortTimer;

fn short_time(t: Timestamp) -> String {
    super::to_berlin(t).strftime("%H:%M:%S%.3f").to_string()
}

impl FormatTime for ShortTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", short_time(Timestamp::now()))
    }
}

/// [`MakeWriter`] that runs every formatted log line through
/// [`redact_secrets`] before it reaches the wrapped writer, so a credential in
/// a logged URL never reaches stdout (`docker logs`).
///
/// The fmt layer formats each event into one buffer and hands it over in a
/// single `write_all`; `RedactingWriter`'s `write` consumes the whole buffer,
/// so every `write` it sees is one complete line and a `key=value` pair is
/// never split across calls.
struct RedactingMakeWriter<M>(M);

struct RedactingWriter<W>(W);

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for RedactingMakeWriter<M> {
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter(self.0.make_writer())
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        RedactingWriter(self.0.make_writer_for(meta))
    }
}

impl<W: io::Write> io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // The fmt layer always writes UTF-8; pass anything else through.
        match std::str::from_utf8(buf) {
            Ok(line) => self.0.write_all(redact_secrets(line).as_bytes())?,
            Err(_) => self.0.write_all(buf)?,
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Install the global tracing subscriber (format + env-filter + error layer +
/// the dashboard's in-memory [`LogRing`]). Both sinks, stdout and the ring,
/// scrub secret query values via [`redact_secrets`].
///
/// Call once at program startup before any spans are created. The returned
/// ring backs the `/logs` page via `Services`.
pub fn install_tracing() -> Arc<LogRing> {
    // `with_target(true)` surfaces the emitting module (e.g. `twitch_1337`
    // vs `twitch_irc` vs `reqwest`) so an unexpected line can be traced
    // back to a crate without grepping. Pair with `ShortTimer` for the
    // line-budget we just freed by dropping the date prefix.
    let fmt_layer = fmt::layer()
        .with_target(true)
        .with_timer(ShortTimer)
        .with_writer(RedactingMakeWriter(io::stdout));
    let filter_layer = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let ring = Arc::new(LogRing::default());

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .with(LogRingLayer::new(ring.clone()))
        .with(ErrorLayer::default())
        .init();
    ring
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Arc;

    use jiff::{SignedDuration, Timestamp};
    use parking_lot::Mutex;
    use tracing_subscriber::fmt;
    use tracing_subscriber::prelude::*;

    use super::{RedactingMakeWriter, short_time};

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn fmt_writer_redacts_secret_query_values() {
        let buf = SharedBuf::default();
        let sink = buf.clone();
        let layer = fmt::layer()
            .with_target(true)
            .with_writer(RedactingMakeWriter(move || sink.clone()));
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            // Shaped like tower_http's request span around `/auth/callback`.
            let span = tracing::info_span!("request", uri = "/auth/callback?code=SECRET4&state=s");
            let _enter = span.enter();
            let error = "error sending request for url (https://id.twitch.tv/oauth2/token?access_key=SECRET1&client_secret=SECRET2&refresh_token=SECRET3&grant_type=refresh_token)";
            tracing::warn!(target: "tst", %error, "token refresh failed");
        });
        let out = String::from_utf8(buf.0.lock().clone()).unwrap();
        for secret in ["SECRET1", "SECRET2", "SECRET3", "SECRET4"] {
            assert!(!out.contains(secret), "{secret} leaked: {out}");
        }
        assert!(
            out.contains("/auth/callback?code=<redacted>&state=s"),
            "{out}"
        );
        assert!(
            out.contains("token?access_key=<redacted>&client_secret=<redacted>&refresh_token=<redacted>&grant_type=refresh_token)"),
            "{out}"
        );
        assert!(out.contains("token refresh failed"), "{out}");
    }

    #[test]
    fn short_time_formats_in_berlin_summer_time() {
        // 11:37 UTC in June is 13:37 CEST (UTC+2) — the 1337 event must
        // read as 13:37 in the logs.
        let t =
            "2026-06-10T11:37:00Z".parse::<Timestamp>().unwrap() + SignedDuration::from_millis(123);
        assert_eq!(short_time(t), "13:37:00.123");
    }

    #[test]
    fn short_time_formats_in_berlin_winter_time() {
        // 12:37 UTC in January is 13:37 CET (UTC+1).
        let t = "2026-01-15T12:37:00Z".parse::<Timestamp>().unwrap();
        assert_eq!(short_time(t), "13:37:00.000");
    }
}
