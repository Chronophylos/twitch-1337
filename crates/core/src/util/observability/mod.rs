//! Tracing setup and the signal pipeline (docs/observability.md, ADR-0013).
//!
//! [`install_tracing`] installs one global subscriber with a filter per sink:
//! stdout follows `RUST_LOG` (default `info`), the `/logs` ring is fixed at
//! `info`, and the OTel logs and traces layers each sit behind a reloadable
//! filter that starts `off`. [`Observability::enable_otel`] turns export on
//! once config.toml and the settings store are loaded.

pub mod attr;
mod otel;
mod report;

use std::io;
use std::sync::Arc;

use jiff::Timestamp;
use tracing::{Metadata, Subscriber};
use tracing_error::ErrorLayer;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::filter::{EnvFilter, LevelFilter, ParseError};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Registry, fmt};

use super::log_ring::{LogRing, LogRingLayer};
use super::redact::redact_secrets;
use crate::config::OtelBootstrap;
use crate::settings::SettingsHandle;

pub use otel::OtelExport;
pub use report::mark_span_error;

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

/// Handle returned by [`install_tracing`]: the `/logs` ring and the switch
/// that turns OTLP export on.
pub struct Observability {
    log_ring: Arc<LogRing>,
    otel: otel::Pipeline,
}

impl Observability {
    /// The ring backing the dashboard `/logs` page.
    pub fn log_ring(&self) -> Arc<LogRing> {
        Arc::clone(&self.log_ring)
    }

    /// Start OTLP export when `[otel]` is configured, with the OTel filters
    /// from `settings` applied now and on every settings change.
    ///
    /// `None` means nothing is exported: `[otel]` is absent, or setup failed
    /// (logged once as `warn`). The bot runs the same either way. Call once,
    /// inside the tokio runtime; flush the returned handle on shutdown.
    pub fn enable_otel(
        &self,
        config: Option<&OtelBootstrap>,
        settings: &SettingsHandle,
    ) -> Option<OtelExport> {
        let export = self.otel.enable(config?, settings)?;
        export.set_global_meter_provider();
        Some(export)
    }
}

/// Install the global tracing subscriber. Sinks, each with its own filter:
///
/// - stdout: `RUST_LOG`, default `info`; color-eyre's span traces use the same
///   filter.
/// - the dashboard `/logs` ring: `info`, fixed.
/// - OTel logs and traces: `off` until [`Observability::enable_otel`], then
///   `observability.otel.logs_filter` / `traces_filter` from settings.
///
/// stdout and the ring scrub secret query values via [`redact_secrets`].
/// Call once at program startup, before any span is created.
pub fn install_tracing() -> Observability {
    let (subscriber, observability) =
        build_subscriber(io::stdout, || EnvFilter::try_from_default_env().ok());
    subscriber.init();
    observability
}

/// The subscriber [`install_tracing`] installs, with the stdout writer and
/// `RUST_LOG` lookup injectable for tests. `stdout_directives` returning
/// `None` means `info`.
fn build_subscriber<W>(
    stdout: W,
    stdout_directives: fn() -> Option<EnvFilter>,
) -> (impl Subscriber + Send + Sync + 'static, Observability)
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let stdout_filter = || stdout_directives().unwrap_or_else(|| EnvFilter::new("info"));
    // `with_target(true)` surfaces the emitting module (e.g. `twitch_1337`
    // vs `twitch_irc` vs `reqwest`) so an unexpected line can be traced
    // back to a crate without grepping. Pair with `ShortTimer` for the
    // line-budget we just freed by dropping the date prefix.
    let fmt_layer = fmt::layer()
        .with_target(true)
        .with_timer(ShortTimer)
        .with_writer(RedactingMakeWriter(stdout))
        .with_filter(stdout_filter());
    let ring = Arc::new(LogRing::default());
    let ring_layer = LogRingLayer::new(Arc::clone(&ring)).with_filter(LevelFilter::INFO);
    // Every layer carries a filter: an unfiltered one would raise the global
    // max level to TRACE and enable every callsite in every dependency.
    let error_layer = ErrorLayer::default().with_filter(stdout_filter());

    let (traces_filter, traces_reload) = otel::off_filter();
    let (logs_filter, logs_reload) = otel::off_filter();
    let otel = otel::Pipeline::new(traces_reload, logs_reload);
    let traces_layer = OpenTelemetryLayer::new(otel.tracer()).with_filter(traces_filter);
    let logs_layer = otel.logs_layer().with_filter(logs_filter);

    let subscriber = Registry::default()
        .with(fmt_layer)
        .with(ring_layer)
        .with(error_layer)
        .with(traces_layer)
        .with(logs_layer);
    (
        subscriber,
        Observability {
            log_ring: ring,
            otel,
        },
    )
}

/// Targets on the export path itself: their events, exported by the very
/// request that carries them, would emit more on the way out. The SDK
/// suppresses its own telemetry but not the HTTP stack's, so these are forced
/// `off` after whatever the settings say.
const EXPORT_PATH_OFF: &str = "hyper=off,reqwest=off,h2=off,opentelemetry=off";

/// Why an OTel filter string was rejected.
#[derive(Debug, thiserror::Error)]
pub enum OtelFilterError {
    #[error("must not be blank; use `off` to export nothing")]
    Blank,
    #[error(transparent)]
    Invalid(#[from] ParseError),
}

/// Parse an `observability.otel.*_filter` setting into the filter the OTel
/// layer runs with. Settings validation calls this too, so a saved string
/// always applies. Every directive must parse; `EnvFilter`'s lossy parsing
/// would silently drop a typo.
pub fn parse_otel_filter(directives: &str) -> Result<EnvFilter, OtelFilterError> {
    if directives.trim().is_empty() {
        return Err(OtelFilterError::Blank);
    }
    Ok(EnvFilter::builder().parse(format!("{directives},{EXPORT_PATH_OFF}"))?)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Arc;

    use jiff::{SignedDuration, Timestamp};
    use parking_lot::Mutex;
    use tracing::Level;
    use tracing_subscriber::fmt;
    use tracing_subscriber::prelude::*;

    use super::{RedactingMakeWriter, build_subscriber, short_time};

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

    /// Without `[otel]` nothing is enabled beyond stdout and the ring: our
    /// crates' `debug` events (on in the default OTel logs filter) stay off
    /// for every layer, and the `info` lines reach stdout as before.
    #[test]
    fn absent_otel_section_exports_nothing_and_keeps_stdout() {
        let buf = SharedBuf::default();
        let sink = buf.clone();
        let (subscriber, obs) = build_subscriber(move || sink.clone(), || None);
        let settings = crate::settings::test_handle();
        assert!(obs.enable_otel(None, &settings).is_none());

        tracing::subscriber::with_default(subscriber, || {
            assert!(!tracing::enabled!(target: "twitch_1337_core::x", Level::DEBUG));
            let span = tracing::info_span!(target: "twitch_1337_core::x", "unit");
            let _enter = span.enter();
            tracing::info!(target: "twitch_1337_core::x", "still on stdout");
        });
        let out = String::from_utf8(buf.0.lock().clone()).unwrap();
        assert!(out.contains("still on stdout"), "{out}");
        assert_eq!(obs.log_ring().snapshot().len(), 1);
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
