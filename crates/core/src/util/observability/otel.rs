//! OTLP/HTTP export of logs, traces and metrics (ADR-0013).
//!
//! The OTel layers exist from [`super::install_tracing`] on, behind filters
//! that start `off` and span/log processors that start empty. Without
//! `[otel]` they stay that way, so the layers never see an event.
//! [`Pipeline::enable`] fills the processors with batch exporters running on
//! the tokio runtime, applies the settings filters, and keeps them in sync
//! with the settings store.
//!
//! Export failures never reach the bot: a failing batch is dropped, logged
//! once per outage as `warn`, and export resumes when the collector answers.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use eyre::WrapErr as _;
use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _};
use opentelemetry::metrics::{Counter, MeterProvider as _};
use opentelemetry::trace::{Status, TracerProvider as _};
use opentelemetry::{Context, InstrumentationScope, KeyValue, Value};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::logs::log_processor_with_async_runtime::BatchLogProcessor;
use opentelemetry_sdk::logs::{LogBatch, LogProcessor, SdkLogRecord, SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
use opentelemetry_sdk::metrics::{SdkMeterProvider, Temporality};
use opentelemetry_sdk::runtime::Tokio;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider, Span, SpanData, SpanProcessor};
use opentelemetry_semantic_conventions as semconv;
use secrecy::ExposeSecret as _;
use tokio::task::JoinHandle;
use tracing::{Subscriber, debug, info, warn};
use tracing_subscriber::{EnvFilter, reload};

use super::parse_otel_filter;
use crate::config::OtelBootstrap;
use crate::settings::{OtelSettings, SettingsHandle, Subscription};
use crate::util::redact::redact_secrets;

/// `service.name` on every signal.
const SERVICE_NAME: &str = "twitch-1337";

/// Per-request deadline, retries included. The collector is on the LAN; a
/// slower answer means it is down, and the batch is dropped.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on the final flush at shutdown, all three signals together.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// A reloadable OTel filter, with the subscriber type erased so the handle
/// can be stored next to the providers.
#[derive(Clone)]
pub(super) struct FilterReload(Arc<dyn Fn(EnvFilter) -> Result<(), reload::Error> + Send + Sync>);

/// A per-layer filter that exports nothing until reloaded, plus its handle.
pub(super) fn off_filter<S>() -> (reload::Layer<EnvFilter, S>, FilterReload)
where
    S: Subscriber + 'static,
{
    let (layer, handle) = reload::Layer::new(EnvFilter::new("off"));
    let reload = move |filter: EnvFilter| handle.reload(filter);
    (layer, FilterReload(Arc::new(reload)))
}

impl FilterReload {
    /// Parse and install `directives`. Saved settings were validated by the
    /// same parser, so a failure here means a hand-edited settings.ron; the
    /// previous filter stays.
    fn apply(&self, knob: &'static str, directives: &str) {
        let result = parse_otel_filter(directives)
            .wrap_err("parse filter")
            .and_then(|filter| (self.0)(filter).wrap_err("reload filter"));
        match result {
            Ok(()) => debug!(knob, directives, "OTel filter applied"),
            Err(error) => warn!(knob, ?error, "OTel filter left unchanged"),
        }
    }
}

/// Span processor slot, empty until [`Pipeline::enable`]. It lets the tracer
/// provider, and the tracing layer holding its tracer, exist before
/// config.toml is read.
#[derive(Debug, Clone, Default)]
struct DeferredSpans(Arc<OnceLock<Box<dyn SpanProcessor>>>);

impl SpanProcessor for DeferredSpans {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        if let Some(inner) = self.0.get() {
            inner.on_start(span, cx);
        }
    }

    fn on_end(&self, span: SpanData) {
        if let Some(inner) = self.0.get() {
            inner.on_end(span);
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        match self.0.get() {
            Some(inner) => inner.force_flush(),
            None => Ok(()),
        }
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        match self.0.get() {
            Some(inner) => inner.shutdown_with_timeout(timeout),
            None => Ok(()),
        }
    }

    // `set_resource` keeps its no-op default: the provider calls it at build
    // time, before there is an inner processor, so `enable` hands the same
    // resource to the inner one directly.
}

/// Log processor slot, empty until [`Pipeline::enable`]. See
/// [`DeferredSpans`].
#[derive(Debug, Clone, Default)]
struct DeferredLogs(Arc<OnceLock<Box<dyn LogProcessor>>>);

impl LogProcessor for DeferredLogs {
    fn emit(&self, data: &mut SdkLogRecord, instrumentation: &InstrumentationScope) {
        if let Some(inner) = self.0.get() {
            inner.emit(data, instrumentation);
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        match self.0.get() {
            Some(inner) => inner.force_flush(),
            None => Ok(()),
        }
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        match self.0.get() {
            Some(inner) => inner.shutdown_with_timeout(timeout),
            None => Ok(()),
        }
    }
}

/// `redact_secrets` on one OTel string; `None` when nothing was redacted, so
/// the clean path allocates nothing.
fn redact_str(s: &str) -> Option<String> {
    match redact_secrets(s) {
        Cow::Owned(redacted) => Some(redacted),
        Cow::Borrowed(_) => None,
    }
}

fn redact_value(value: &mut Value) {
    if let Value::String(s) = value
        && let Some(redacted) = redact_str(s.as_str())
    {
        *s = redacted.into();
    }
}

fn redact_any(value: &AnyValue) -> Option<AnyValue> {
    match value {
        AnyValue::String(s) => redact_str(s.as_str()).map(|r| AnyValue::String(r.into())),
        _ => None,
    }
}

/// Span processor running the stdout/ring `redact_secrets` pass over span
/// attributes, span events (tracing events inside the span) and the status
/// description before the batch processor queues the span.
#[derive(Debug)]
struct RedactingSpans<P>(P);

impl<P: SpanProcessor> SpanProcessor for RedactingSpans<P> {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        self.0.on_start(span, cx);
    }

    fn on_end(&self, mut span: SpanData) {
        for kv in &mut span.attributes {
            redact_value(&mut kv.value);
        }
        for event in &mut span.events.events {
            if let Some(redacted) = redact_str(&event.name) {
                event.name = redacted.into();
            }
            for kv in &mut event.attributes {
                redact_value(&mut kv.value);
            }
        }
        if let Status::Error { description } = &mut span.status
            && let Some(redacted) = redact_str(description)
        {
            *description = redacted.into();
        }
        self.0.on_end(span);
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource);
    }
}

/// Log processor running the stdout/ring `redact_secrets` pass over the
/// record body (the message) and string attributes (the fields) before the
/// batch processor queues it. `SdkLogRecord` attributes are append-only, so a
/// record that needs redaction is rebuilt from a fresh one minted by
/// `minter`, a logger of a processor-less provider.
struct RedactingLogs<P> {
    inner: P,
    minter: SdkLogger,
}

impl<P> RedactingLogs<P> {
    fn new(inner: P) -> Self {
        Self {
            inner,
            minter: SdkLoggerProvider::builder().build().logger("redact"),
        }
    }
}

impl<P: std::fmt::Debug> std::fmt::Debug for RedactingLogs<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RedactingLogs").field(&self.inner).finish()
    }
}

impl<P: LogProcessor> LogProcessor for RedactingLogs<P> {
    fn emit(&self, data: &mut SdkLogRecord, instrumentation: &InstrumentationScope) {
        if let Some(clean) = redacted_record(data, &self.minter) {
            *data = clean;
        }
        self.inner.emit(data, instrumentation);
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

/// A copy of `record` with secrets redacted, or `None` when it has none.
fn redacted_record(record: &SdkLogRecord, minter: &SdkLogger) -> Option<SdkLogRecord> {
    let body = record.body().and_then(redact_any);
    if body.is_none()
        && !record
            .attributes_iter()
            .any(|(_, v)| redact_any(v).is_some())
    {
        return None;
    }
    let mut clean = minter.create_log_record();
    if let Some(name) = record.event_name() {
        clean.set_event_name(name);
    }
    if let Some(target) = record.target() {
        clean.set_target(target.clone());
    }
    if let Some(at) = record.timestamp() {
        clean.set_timestamp(at);
    }
    if let Some(at) = record.observed_timestamp() {
        clean.set_observed_timestamp(at);
    }
    if let Some(cx) = record.trace_context() {
        clean.set_trace_context(cx.trace_id, cx.span_id, cx.trace_flags);
    }
    if let Some(text) = record.severity_text() {
        clean.set_severity_text(text);
    }
    if let Some(number) = record.severity_number() {
        clean.set_severity_number(number);
    }
    if let Some(body) = body.or_else(|| record.body().cloned()) {
        clean.set_body(body);
    }
    for (key, value) in record.attributes_iter() {
        clean.add_attribute(
            key.clone(),
            redact_any(value).unwrap_or_else(|| value.clone()),
        );
    }
    Some(clean)
}

/// `service.name`, `service.version` (BUILD_NUM) and
/// `vcs.ref.head.revision` (the full GIT_SHA), as baked by `build.rs`.
fn resource() -> Resource {
    Resource::builder()
        .with_service_name(SERVICE_NAME)
        .with_attributes([
            KeyValue::new(semconv::resource::SERVICE_VERSION, env!("BUILD_NUM")),
            KeyValue::new(
                semconv::resource::VCS_REF_HEAD_REVISION,
                env!("GIT_SHA_FULL"),
            ),
        ])
        .build()
}

/// The OTel half of the subscriber: providers whose processors are filled
/// on [`Self::enable`], and the reload handles of the two OTel filters.
pub(super) struct Pipeline {
    tracer_provider: SdkTracerProvider,
    logger_provider: SdkLoggerProvider,
    spans: DeferredSpans,
    logs: DeferredLogs,
    traces_filter: FilterReload,
    logs_filter: FilterReload,
}

impl Pipeline {
    pub(super) fn new(traces_filter: FilterReload, logs_filter: FilterReload) -> Self {
        let spans = DeferredSpans::default();
        let logs = DeferredLogs::default();
        let tracer_provider = SdkTracerProvider::builder()
            .with_span_processor(spans.clone())
            .with_resource(resource())
            .build();
        let logger_provider = SdkLoggerProvider::builder()
            .with_log_processor(logs.clone())
            .with_resource(resource())
            .build();
        Self {
            tracer_provider,
            logger_provider,
            spans,
            logs,
            traces_filter,
            logs_filter,
        }
    }

    /// Tracer for the `tracing-opentelemetry` layer.
    pub(super) fn tracer(&self) -> SdkTracer {
        self.tracer_provider.tracer(SERVICE_NAME)
    }

    /// The `tracing` → OTel logs bridge.
    pub(super) fn logs_layer(&self) -> OpenTelemetryTracingBridge<SdkLoggerProvider, SdkLogger> {
        OpenTelemetryTracingBridge::new(&self.logger_provider)
    }

    /// Wire exporters to `config`'s collector and open the OTel filters.
    /// `None` when setup fails (logged) or export was already enabled.
    pub(super) fn enable(
        &self,
        config: &OtelBootstrap,
        settings: &SettingsHandle,
    ) -> Option<OtelExport> {
        let exporters = match Exporters::build(config) {
            Ok(exporters) => exporters,
            Err(error) => {
                warn!(?error, "OTLP export disabled: exporter setup failed");
                return None;
            }
        };
        let resource = resource();

        let meter_provider = SdkMeterProvider::builder()
            .with_reader(
                PeriodicReader::builder(Watched::new(exporters.metrics, "metrics", None), Tokio)
                    .build(),
            )
            .with_resource(resource.clone())
            .build();
        let meter = meter_provider.meter(SERVICE_NAME);
        let spans_exported = meter
            .u64_counter(semconv::metric::OTEL_SDK_EXPORTER_SPAN_EXPORTED)
            .with_unit("{span}")
            .with_description("Spans whose OTLP export finished; failures carry error.type")
            .build();
        let logs_exported = meter
            .u64_counter(semconv::metric::OTEL_SDK_EXPORTER_LOG_EXPORTED)
            .with_unit("{log_record}")
            .with_description("Log records whose OTLP export finished; failures carry error.type")
            .build();

        let mut span_processor = BatchSpanProcessor::builder(
            Watched::new(exporters.spans, "traces", Some(spans_exported)),
            Tokio,
        )
        .build();
        span_processor.set_resource(&resource);
        let mut log_processor = BatchLogProcessor::builder(
            Watched::new(exporters.logs, "logs", Some(logs_exported)),
            Tokio,
        )
        .build();
        log_processor.set_resource(&resource);
        let spans_set = self
            .spans
            .0
            .set(Box::new(RedactingSpans(span_processor)))
            .is_ok();
        let logs_set = self
            .logs
            .0
            .set(Box::new(RedactingLogs::new(log_processor)))
            .is_ok();
        if !(spans_set && logs_set) {
            warn!("OTLP export already enabled; second enable ignored");
            return None;
        }
        // Armed before the initial read, so a save in between is not lost.
        let changes = settings.subscribe();
        let current = settings.load().observability.otel.clone();
        apply_filters(&self.traces_filter, &self.logs_filter, &current, None);
        let sync = tokio::spawn(sync_filters(
            settings.clone(),
            changes,
            self.traces_filter.clone(),
            self.logs_filter.clone(),
            current,
        ));

        info!(endpoint = %config.endpoint, "OTLP export enabled");
        Some(OtelExport {
            tracer_provider: self.tracer_provider.clone(),
            logger_provider: self.logger_provider.clone(),
            meter_provider,
            sync,
        })
    }
}

/// Reload the filters whose directive string changed since `previous`.
fn apply_filters(
    traces_filter: &FilterReload,
    logs_filter: &FilterReload,
    current: &OtelSettings,
    previous: Option<&OtelSettings>,
) {
    if previous.is_none_or(|p| p.traces_filter != current.traces_filter) {
        traces_filter.apply("observability.otel.traces_filter", &current.traces_filter);
    }
    if previous.is_none_or(|p| p.logs_filter != current.logs_filter) {
        logs_filter.apply("observability.otel.logs_filter", &current.logs_filter);
    }
}

/// Apply dashboard edits of the OTel filters live. Runs until
/// [`OtelExport::shutdown`] aborts it.
async fn sync_filters(
    settings: SettingsHandle,
    mut changes: Subscription,
    traces_filter: FilterReload,
    logs_filter: FilterReload,
    mut applied: OtelSettings,
) {
    loop {
        changes.next().await;
        let current = settings.load().observability.otel.clone();
        apply_filters(&traces_filter, &logs_filter, &current, Some(&applied));
        applied = current;
    }
}

/// The three OTLP/HTTP exporters, sharing one HTTP client.
struct Exporters {
    spans: opentelemetry_otlp::SpanExporter,
    logs: opentelemetry_otlp::LogExporter,
    metrics: opentelemetry_otlp::MetricExporter,
}

impl Exporters {
    fn build(config: &OtelBootstrap) -> eyre::Result<Self> {
        // reqwest is built `rustls-no-provider`; building any client without
        // an installed provider panics.
        crate::util::install_crypto_provider();
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(auth) = config.auth_header.as_ref() {
            let mut value = reqwest::header::HeaderValue::from_str(auth.expose_secret())
                .wrap_err("otel.auth_header is not a valid header value")?;
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(EXPORT_TIMEOUT)
            .build()
            .wrap_err("build OTLP HTTP client")?;
        let base = config.endpoint.trim_end_matches('/');

        let spans = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_http_client(client.clone())
            .with_endpoint(format!("{base}/v1/traces"))
            .with_timeout(EXPORT_TIMEOUT)
            .build()
            .wrap_err("build OTLP span exporter")?;
        let logs = opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_http_client(client.clone())
            .with_endpoint(format!("{base}/v1/logs"))
            .with_timeout(EXPORT_TIMEOUT)
            .build()
            .wrap_err("build OTLP log exporter")?;
        let metrics = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_http_client(client)
            .with_endpoint(format!("{base}/v1/metrics"))
            .with_timeout(EXPORT_TIMEOUT)
            .build()
            .wrap_err("build OTLP metric exporter")?;
        Ok(Self {
            spans,
            logs,
            metrics,
        })
    }
}

/// Exporter wrapper that turns failures into one `warn` per outage (and one
/// `info` on recovery) and counts exported items by outcome. The SDK's own
/// diagnostics are compiled out (`internal-logs` off), so this is the only
/// place an export failure is reported.
#[derive(Debug)]
struct Watched<E> {
    inner: E,
    signal: &'static str,
    failing: AtomicBool,
    exported: Option<Counter<u64>>,
}

impl<E> Watched<E> {
    fn new(inner: E, signal: &'static str, exported: Option<Counter<u64>>) -> Self {
        Self {
            inner,
            signal,
            failing: AtomicBool::new(false),
            exported,
        }
    }

    fn observe(&self, items: usize, result: &OTelSdkResult) {
        let signal = self.signal;
        match result {
            Ok(()) => {
                if self.failing.swap(false, Ordering::Relaxed) {
                    info!(signal, "OTLP export recovered");
                }
                if let Some(counter) = &self.exported {
                    counter.add(items as u64, &[]);
                }
            }
            Err(error) => {
                if !self.failing.swap(true, Ordering::Relaxed) {
                    warn!(
                        signal,
                        ?error,
                        "OTLP export failed; dropping this signal until the collector answers"
                    );
                }
                if let Some(counter) = &self.exported {
                    counter.add(
                        items as u64,
                        &[KeyValue::new(
                            semconv::attribute::ERROR_TYPE,
                            error_type(error),
                        )],
                    );
                }
            }
        }
    }
}

/// Bounded `error.type` label for a failed export.
fn error_type(error: &OTelSdkError) -> &'static str {
    match error {
        OTelSdkError::AlreadyShutdown => "already_shutdown",
        OTelSdkError::Timeout(_) => "timeout",
        // Semconv's fallback for causes without a well-known type.
        _ => "_OTHER",
    }
}

impl<E: opentelemetry_sdk::trace::SpanExporter> opentelemetry_sdk::trace::SpanExporter
    for Watched<E>
{
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let items = batch.len();
        let result = self.inner.export(batch).await;
        self.observe(items, &result);
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

impl<E: opentelemetry_sdk::logs::LogExporter> opentelemetry_sdk::logs::LogExporter for Watched<E> {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        let items = batch.iter().count();
        let result = self.inner.export(batch).await;
        self.observe(items, &result);
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

impl<E: PushMetricExporter> PushMetricExporter for Watched<E> {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let result = self.inner.export(metrics).await;
        self.observe(0, &result);
        result
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn temporality(&self) -> Temporality {
        self.inner.temporality()
    }
}

/// Running OTLP export, from [`super::Observability::enable_otel`]. Flush it
/// with [`Self::shutdown`] when the bot stops.
pub struct OtelExport {
    tracer_provider: SdkTracerProvider,
    logger_provider: SdkLoggerProvider,
    meter_provider: SdkMeterProvider,
    sync: JoinHandle<()>,
}

impl OtelExport {
    /// Make this export's meter provider the process-wide one behind
    /// `opentelemetry::global::meter`. Kept out of [`Pipeline::enable`] so
    /// tests can wire a pipeline without touching process globals.
    pub(super) fn set_global_meter_provider(&self) {
        opentelemetry::global::set_meter_provider(self.meter_provider.clone());
    }

    /// Stop following settings and flush all three signals, giving up after
    /// [`SHUTDOWN_TIMEOUT`] in total. Failures are logged; shutdown goes on.
    pub async fn shutdown(self) {
        self.sync.abort();
        let Self {
            tracer_provider,
            logger_provider,
            meter_provider,
            ..
        } = self;
        // The async-runtime processors block their caller until the runtime
        // has run the final export, so the flush must not hold a worker. A
        // detached std thread rather than `spawn_blocking`: the runtime waits
        // for its blocking pool on shutdown, so a hung flush there would
        // outlive the cap. Traces and logs go first: their exports feed the
        // counters the final metrics push reports.
        let deadline = std::time::Instant::now() + SHUTDOWN_TIMEOUT;
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let flush = move || {
            let left = || deadline.saturating_duration_since(std::time::Instant::now());
            let results = [
                ("traces", tracer_provider.shutdown_with_timeout(left())),
                ("logs", logger_provider.shutdown_with_timeout(left())),
                ("metrics", meter_provider.shutdown_with_timeout(left())),
            ];
            #[expect(
                clippy::let_underscore_must_use,
                reason = "Err only means shutdown already stopped waiting (ADR-0012 rule 6)"
            )]
            let _ = done_tx.send(results);
        };
        if let Err(error) = std::thread::Builder::new()
            .name("otel-flush".to_owned())
            .spawn(flush)
        {
            warn!(?error, "OTLP flush on shutdown skipped: no flush thread");
            return;
        }
        match tokio::time::timeout_at(deadline.into(), done_rx).await {
            Ok(Ok(results)) => {
                for (signal, result) in results {
                    if let Err(error) = result {
                        warn!(signal, ?error, "OTLP flush on shutdown failed");
                    }
                }
            }
            Ok(Err(error)) => warn!(?error, "OTLP flush thread ended without a result"),
            Err(_elapsed) => warn!(
                timeout_secs = SHUTDOWN_TIMEOUT.as_secs(),
                "OTLP flush on shutdown timed out; unsent signals dropped"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use secrecy::SecretString;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::build_subscriber;
    use crate::config::OtelBootstrap;
    use crate::settings::{
        Actor, MemoryAuditLog, ObservabilityOverrides, OtelOverrides, SettingsOverrides,
        SettingsStore,
    };

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
    }

    /// Bodies POSTed to `path`, each checked for the configured auth header.
    async fn bodies(collector: &MockServer, path: &str) -> Vec<Vec<u8>> {
        let requests = collector.received_requests().await.expect("recording");
        requests
            .into_iter()
            .filter(|r| r.url.path() == path)
            .map(|r| {
                assert_eq!(
                    r.headers.get("authorization").and_then(|v| v.to_str().ok()),
                    Some("Bearer t0k"),
                    "{path}"
                );
                r.body
            })
            .collect()
    }

    /// One span, log lines, and the export counters reach a collector over
    /// OTLP/HTTP; a dashboard save widens the logs filter live.
    ///
    /// No `tracing::enabled!` here: with per-layer filters it leaves the
    /// filter state of the layers that said no set for the next span on the
    /// thread, which then silently skips them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exports_all_signals_and_follows_settings() {
        let collector = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&collector)
            .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let (store, settings) = SettingsStore::open(
            dir.path(),
            std::sync::Arc::new(MemoryAuditLog::new()),
            "chan",
        )
        .expect("open store");

        // Scoped subscriber and `Pipeline::enable` (not `enable_otel`): no
        // global subscriber or meter provider leaks into other tests.
        let (subscriber, obs) = build_subscriber(std::io::sink, || None);
        let _default = tracing::subscriber::set_default(subscriber);
        let config = OtelBootstrap {
            endpoint: format!("{}/", collector.uri()),
            auth_header: Some(SecretString::new("Bearer t0k".into())),
        };
        let export = obs.otel.enable(&config, &settings).expect("enabled");
        let flush_logs = || {
            let logs = export.logger_provider.clone();
            async move {
                tokio::task::spawn_blocking(move || logs.force_flush())
                    .await
                    .expect("join")
                    .expect("flush logs");
            }
        };

        {
            let span = tracing::info_span!(
                target: "twitch_1337_core::t",
                "otel.test.unit",
                url = "https://x.test/s?access_key=SPANSECRET"
            );
            let _enter = span.enter();
            tracing::info!(target: "twitch_1337_core::t", "hello collector");
            // Recorded twice: as a log record and as a span event.
            tracing::warn!(
                target: "twitch_1337_core::t",
                url = "https://x.test/f?access_key=FIELDSECRET",
                "fetch https://x.test/m?access_key=MSGSECRET failed"
            );
        }
        // Dependencies sit at `warn` in the default logs filter.
        tracing::debug!(target: "probe", "probe before save");

        let patch = SettingsOverrides {
            observability: ObservabilityOverrides {
                otel: OtelOverrides {
                    logs_filter: Some("warn,probe=debug".into()),
                    ..Default::default()
                },
            },
            ..Default::default()
        };
        let actor = Actor {
            user_id: "1".into(),
            user_login: "owner".into(),
        };
        store.apply(patch, actor).await.expect("apply");

        // The reload happens on the sync task; poll until a probe gets out.
        // Each round rebuilds callsite interest on this thread: a rebuild on
        // the sync task's thread does not reach this thread's scoped
        // subscriber (with the global one in production it does).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        for attempt in 1.. {
            tracing::callsite::rebuild_interest_cache();
            tracing::debug!(target: "probe", attempt, "probe after save");
            flush_logs().await;
            let logs = bodies(&collector, "/v1/logs").await;
            if logs.iter().any(|b| contains(b, "probe after save")) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "logs filter not reloaded after the settings save"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        export.shutdown().await;
        let traces = bodies(&collector, "/v1/traces").await;
        assert!(traces.iter().any(|b| contains(b, "otel.test.unit")));
        assert!(traces.iter().any(|b| contains(b, "twitch-1337")));
        let logs = bodies(&collector, "/v1/logs").await;
        assert!(logs.iter().any(|b| contains(b, "hello collector")));
        assert!(!logs.iter().any(|b| contains(b, "probe before save")));
        assert!(logs.iter().any(|b| contains(b, "access_key=<redacted>")));
        let everything = collector.received_requests().await.expect("recording");
        for request in &everything {
            assert!(
                !contains(&request.body, "SECRET"),
                "secret reached the collector via {}",
                request.url.path()
            );
        }
        let metrics = bodies(&collector, "/v1/metrics").await;
        assert!(metrics.iter().any(|b| contains(
            b,
            opentelemetry_semantic_conventions::metric::OTEL_SDK_EXPORTER_SPAN_EXPORTED
        )));
    }
}
