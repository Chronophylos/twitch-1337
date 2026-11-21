//! Handled-error reporting: ADR-0012 rule 5 (log once, where handled) plus
//! docs/observability.md § Spans rule 6 (the same call marks the span).

use std::fmt::Display;

use opentelemetry::trace::Status;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::util::redact::redact_secrets;

/// Set the current span's OTel status to `Error`, described by `error`'s
/// chain (`{:#}`) with secret query values redacted. A no-op outside a span
/// or when the span is not exported.
///
/// Prefer [`log_handled!`](crate::log_handled), which also writes the log
/// line; call this directly only where the line is logged elsewhere.
pub fn mark_span_error<E: Display + ?Sized>(error: &E) {
    let span = tracing::Span::current();
    if span.is_none() {
        return;
    }
    let description = redact_secrets(&format!("{error:#}")).into_owned();
    span.set_status(Status::error(description));
}

/// Log a handled operational failure once, as `error = ?e` (ADR-0012 rule 5),
/// and mark the current span `Error` (docs/observability.md § Spans rule 6).
///
/// The first token picks the level: `error` for unexpected failures, `warn`
/// for expected flakes. The rest is the event's fields and message, exactly
/// as for `tracing::error!`:
///
/// ```ignore
/// log_handled!(warn, error, "weather lookup failed, replying fallback");
/// log_handled!(error, e, user = %login, "could not persist ping {name}");
/// ```
#[macro_export]
macro_rules! log_handled {
    (error, $err:expr, $($rest:tt)+) => {{
        let error = &$err;
        $crate::util::observability::mark_span_error(error);
        ::tracing::error!(error = ?error, $($rest)+);
    }};
    (warn, $err:expr, $($rest:tt)+) => {{
        let error = &$err;
        $crate::util::observability::mark_span_error(error);
        ::tracing::warn!(error = ?error, $($rest)+);
    }};
}

#[cfg(test)]
mod tests {
    use opentelemetry::trace::{Status, TracerProvider as _};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use tracing_subscriber::prelude::*;

    #[derive(Debug, thiserror::Error)]
    #[error("upstream said no for https://api.test/x?access_key=SECRET")]
    struct Upstream;

    #[test]
    fn log_handled_marks_span_error_with_redacted_chain() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));

        tracing::subscriber::with_default(subscriber, || {
            drop(tracing::info_span!("handled.ok").entered());
            let failed = tracing::info_span!("handled.failed");
            let _enter = failed.enter();
            let e = eyre::Report::new(Upstream).wrap_err("weather lookup");
            crate::log_handled!(
                warn,
                e,
                city = "Berlin",
                "weather lookup failed, replying fallback"
            );
        });

        let spans = exporter.get_finished_spans().expect("spans");
        let status = |name: &str| {
            spans
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.status.clone())
                .expect(name)
        };
        assert_eq!(status("handled.ok"), Status::Unset);
        let Status::Error { description } = status("handled.failed") else {
            panic!("handled.failed not marked Error");
        };
        assert!(
            description.starts_with("weather lookup: upstream said no"),
            "{description}"
        );
        assert!(!description.contains("SECRET"), "{description}");
    }
}
