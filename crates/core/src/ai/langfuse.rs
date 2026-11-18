//! First-party Langfuse Cloud exporter (OTLP/JSON over HTTP).
//!
//! Presence of `[langfuse]` in config.toml enables export. Init and export
//! failures are logged and swallowed — they must not abort a Turn or the bot.
//! See `docs/superpowers/specs/2026-08-26-langfuse-otel-design.md`.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use llm::{GenerationEvent, LlmObserver, SpanEvent};
use reqwest::header::{self, HeaderMap, HeaderValue};
use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
};
use tracing::warn;

use crate::APP_USER_AGENT;
use crate::config::LangfuseBootstrap;

pub const DEFAULT_HOST: &str = "https://cloud.langfuse.com";
const TRACES_PATH: &str = "/api/public/otel/v1/traces";
const CHANNEL_CAP: usize = 256;
const BATCH_MAX: usize = 32;
const BATCH_WAIT: Duration = Duration::from_millis(200);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const FINALIZE_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const FINALIZE_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

enum ExporterCmd {
    Generation(GenerationEvent),
    Span(SpanEvent),
    Flush(oneshot::Sender<()>),
    Close(oneshot::Sender<()>),
}

/// Failure while finalizing an application-owned exporter task.
#[derive(Debug, Error)]
pub enum LangfuseFinalizeError {
    #[error("timed out requesting the Langfuse exporter flush")]
    FlushTimeout,

    #[error("the Langfuse exporter closed before the flush completed")]
    FlushClosed,

    #[error("timed out requesting the Langfuse exporter close")]
    CloseTimeout,

    #[error("the Langfuse exporter closed before the close completed")]
    CloseClosed,

    #[error("timed out joining the Langfuse exporter after abort")]
    JoinTimeout,

    #[error("Langfuse exporter task failed during finalization: {0}")]
    Join(#[from] tokio::task::JoinError),
}

struct ExporterOwner {
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for ExporterOwner {
    fn drop(&mut self) {
        // Correct runtime finalization removes and joins this handle. Aborting
        // here is only a last-resort guard against detaching exporter work when
        // composition itself unwinds before `run_bot` takes ownership.
        if let Ok(mut handle) = self.handle.try_lock()
            && let Some(handle) = handle.take()
        {
            handle.abort();
        }
    }
}

/// Handle held in [`crate::Services`]. Clones share both the event channel and
/// the single owner for the exporter's [`JoinHandle`].
#[derive(Clone)]
pub struct LangfuseHandle {
    tx: mpsc::Sender<ExporterCmd>,
    owner: std::sync::Arc<ExporterOwner>,
}

impl LangfuseHandle {
    /// Build the exporter from bootstrap secrets. Spawns a background flush
    /// task. Does not abort the caller on later export failures.
    pub fn start(bootstrap: &LangfuseBootstrap) -> eyre::Result<Self> {
        let host = bootstrap
            .host
            .as_deref()
            .unwrap_or(DEFAULT_HOST)
            .trim_end_matches('/');
        Self::start_inner(
            host,
            bootstrap.public_key.expose_secret(),
            bootstrap.secret_key.expose_secret(),
        )
    }

    /// Test hook: post to an arbitrary origin (e.g. wiremock).
    pub fn start_with_host(host: &str, public_key: &str, secret_key: &str) -> eyre::Result<Self> {
        Self::start_inner(host.trim_end_matches('/'), public_key, secret_key)
    }

    fn start_inner(host: &str, public_key: &str, secret_key: &str) -> eyre::Result<Self> {
        crate::install_crypto_provider();
        let url = format!("{host}{TRACES_PATH}");
        let auth = format!(
            "Basic {}",
            BASE64.encode(format!("{public_key}:{secret_key}"))
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&auth).map_err(|e| eyre::eyre!("langfuse auth header: {e}"))?,
        );
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            "x-langfuse-ingestion-version",
            HeaderValue::from_static("4"),
        );
        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .default_headers(headers)
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| eyre::eyre!("build langfuse HTTP client: {e}"))?;
        let (tx, rx) = mpsc::channel(CHANNEL_CAP);
        let handle = tokio::spawn(exporter_loop(http, url, rx));
        Ok(Self {
            tx,
            owner: std::sync::Arc::new(ExporterOwner {
                handle: Mutex::new(Some(handle)),
            }),
        })
    }

    /// Drain queued observations and wait for one POST attempt.
    pub async fn flush(&self, timeout: Duration) {
        let (done_tx, done_rx) = oneshot::channel();
        if self.tx.send(ExporterCmd::Flush(done_tx)).await.is_err() {
            return;
        }
        let _ = tokio::time::timeout(timeout, done_rx).await;
    }

    /// Stop the exporter and synchronously confirm its task termination.
    ///
    /// Runtime callers invoke this only after all observation producers have
    /// joined. Every phase is bounded; if the exporter misses a request or join
    /// deadline, its handle is aborted and joined before the error returns.
    pub async fn finalize(&self) -> Result<(), LangfuseFinalizeError> {
        let mut first_error = None;

        let (flush_tx, flush_rx) = oneshot::channel();
        match tokio::time::timeout(
            FINALIZE_COMMAND_TIMEOUT,
            self.tx.send(ExporterCmd::Flush(flush_tx)),
        )
        .await
        {
            Ok(Ok(())) => match tokio::time::timeout(FINALIZE_COMMAND_TIMEOUT, flush_rx).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => first_error = Some(LangfuseFinalizeError::FlushClosed),
                Err(_) => first_error = Some(LangfuseFinalizeError::FlushTimeout),
            },
            Ok(Err(_)) => first_error = Some(LangfuseFinalizeError::FlushClosed),
            Err(_) => first_error = Some(LangfuseFinalizeError::FlushTimeout),
        }

        let (close_tx, close_rx) = oneshot::channel();
        match tokio::time::timeout(
            FINALIZE_COMMAND_TIMEOUT,
            self.tx.send(ExporterCmd::Close(close_tx)),
        )
        .await
        {
            Ok(Ok(())) => match tokio::time::timeout(FINALIZE_COMMAND_TIMEOUT, close_rx).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    if first_error.is_none() {
                        first_error = Some(LangfuseFinalizeError::CloseClosed);
                    }
                }
                Err(_) => {
                    if first_error.is_none() {
                        first_error = Some(LangfuseFinalizeError::CloseTimeout);
                    }
                }
            },
            Ok(Err(_)) => {
                if first_error.is_none() {
                    first_error = Some(LangfuseFinalizeError::CloseClosed);
                }
            }
            Err(_) => {
                if first_error.is_none() {
                    first_error = Some(LangfuseFinalizeError::CloseTimeout);
                }
            }
        }

        if let Some(mut handle) = self.owner.handle.lock().await.take() {
            match tokio::time::timeout(FINALIZE_JOIN_TIMEOUT, &mut handle).await {
                Ok(Ok(())) => {}
                Ok(Err(source)) => {
                    if first_error.is_none() {
                        first_error = Some(LangfuseFinalizeError::Join(source));
                    }
                }
                Err(_) => {
                    handle.abort();
                    // Abort is not completion confirmation. Join the aborted
                    // task before returning the finalization error.
                    let _ = handle.await;
                    if first_error.is_none() {
                        first_error = Some(LangfuseFinalizeError::JoinTimeout);
                    }
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub fn as_observer(self: &std::sync::Arc<Self>) -> std::sync::Arc<dyn LlmObserver> {
        self.clone()
    }

    pub fn observer_from(
        opt: &Option<std::sync::Arc<Self>>,
    ) -> Option<std::sync::Arc<dyn LlmObserver>> {
        opt.as_ref().map(Self::as_observer)
    }
}

impl LlmObserver for LangfuseHandle {
    fn generation(&self, event: GenerationEvent) {
        if self.tx.try_send(ExporterCmd::Generation(event)).is_err() {
            warn!("langfuse export queue full, dropping generation");
        }
    }

    fn span(&self, event: SpanEvent) {
        if self.tx.try_send(ExporterCmd::Span(event)).is_err() {
            warn!("langfuse export queue full, dropping span");
        }
    }
}

async fn exporter_loop(http: reqwest::Client, url: String, mut rx: mpsc::Receiver<ExporterCmd>) {
    let mut batch: Vec<ExporterCmd> = Vec::new();
    loop {
        let next = if batch.is_empty() {
            rx.recv().await
        } else {
            match tokio::time::timeout(BATCH_WAIT, rx.recv()).await {
                Ok(v) => v,
                Err(_) => {
                    post_batch(&http, &url, &mut batch).await;
                    continue;
                }
            }
        };
        match next {
            None => {
                if !batch.is_empty() {
                    post_batch(&http, &url, &mut batch).await;
                }
                return;
            }
            Some(ExporterCmd::Flush(done)) => {
                post_batch(&http, &url, &mut batch).await;
                let _ = done.send(());
            }
            Some(ExporterCmd::Close(done)) => {
                // Finalizer ordering is explicit: flush has already completed,
                // but drain anything that raced the close before terminating.
                post_batch(&http, &url, &mut batch).await;
                let _ = done.send(());
                return;
            }
            Some(cmd) => {
                batch.push(cmd);
                if batch.len() >= BATCH_MAX {
                    post_batch(&http, &url, &mut batch).await;
                }
            }
        }
    }
}

async fn post_batch(http: &reqwest::Client, url: &str, batch: &mut Vec<ExporterCmd>) {
    if batch.is_empty() {
        return;
    }
    let body = otlp_traces(std::mem::take(batch));
    match http.post(url).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            warn!(status = %resp.status(), "langfuse OTLP export failed");
        }
        Err(error) => {
            warn!(?error, "langfuse OTLP export request failed");
        }
    }
}

fn otlp_traces(batch: Vec<ExporterCmd>) -> Value {
    let spans: Vec<Value> = batch
        .into_iter()
        .filter_map(|cmd| match cmd {
            ExporterCmd::Generation(e) => Some(generation_span(e)),
            ExporterCmd::Span(e) => Some(generic_span(e)),
            ExporterCmd::Flush(_) | ExporterCmd::Close(_) => None,
        })
        .collect();
    json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [attr_str("service.name", "twitch-1337")]
            },
            "scopeSpans": [{
                "scope": { "name": "twitch-1337" },
                "spans": spans
            }]
        }]
    })
}

fn generation_span(e: GenerationEvent) -> Value {
    let mut attributes = common_attrs(
        e.user.as_deref(),
        e.session_id.as_deref(),
        &e.tags,
        "generation",
    );
    attributes.push(attr_str("gen_ai.request.model", &e.model));
    attributes.push(attr_str("langfuse.observation.input", &e.input));
    attributes.push(attr_str("langfuse.observation.output", &e.output));
    if let Some(n) = e.input_tokens {
        attributes.push(attr_int("gen_ai.usage.input_tokens", n));
    }
    if let Some(n) = e.output_tokens {
        attributes.push(attr_int("gen_ai.usage.output_tokens", n));
    }
    if let Some(cost) = e.cost_usd {
        attributes.push(attr_f64("gen_ai.usage.cost", cost));
    }
    span_json(
        OtlpSpanMeta {
            trace_id: &e.trace_id,
            span_id: &e.span_id,
            parent: e.parent_span_id.as_deref(),
            name: &e.name,
            start: e.start_unix_nano,
            end: e.end_unix_nano,
            error: e.error.as_deref(),
        },
        attributes,
    )
}

fn generic_span(e: SpanEvent) -> Value {
    let mut attributes = common_attrs(e.user.as_deref(), e.session_id.as_deref(), &e.tags, "span");
    if !e.input.is_empty() {
        attributes.push(attr_str("langfuse.observation.input", &e.input));
    }
    if !e.output.is_empty() {
        attributes.push(attr_str("langfuse.observation.output", &e.output));
    }
    span_json(
        OtlpSpanMeta {
            trace_id: &e.trace_id,
            span_id: &e.span_id,
            parent: e.parent_span_id.as_deref(),
            name: &e.name,
            start: e.start_unix_nano,
            end: e.end_unix_nano,
            error: e.error.as_deref(),
        },
        attributes,
    )
}

fn common_attrs(
    user: Option<&str>,
    session: Option<&str>,
    tags: &[String],
    ty: &str,
) -> Vec<Value> {
    let mut attrs = vec![attr_str("langfuse.observation.type", ty)];
    if let Some(u) = user {
        attrs.push(attr_str("langfuse.user.id", u));
        attrs.push(attr_str("user.id", u));
    }
    if let Some(s) = session {
        attrs.push(attr_str("langfuse.session.id", s));
        attrs.push(attr_str("session.id", s));
    }
    if !tags.is_empty() {
        attrs.push(json!({
            "key": "langfuse.trace.tags",
            "value": {
                "arrayValue": {
                    "values": tags.iter().map(|t| json!({"stringValue": t})).collect::<Vec<_>>()
                }
            }
        }));
    }
    attrs
}

struct OtlpSpanMeta<'a> {
    trace_id: &'a str,
    span_id: &'a str,
    parent: Option<&'a str>,
    name: &'a str,
    start: u64,
    end: u64,
    error: Option<&'a str>,
}

fn span_json(meta: OtlpSpanMeta<'_>, attributes: Vec<Value>) -> Value {
    let mut span = json!({
        "traceId": meta.trace_id,
        "spanId": meta.span_id,
        "name": meta.name,
        "kind": 1,
        "startTimeUnixNano": meta.start.to_string(),
        "endTimeUnixNano": meta.end.to_string(),
        "attributes": attributes,
        "status": status_json(meta.error),
    });
    if let Some(p) = meta.parent {
        span["parentSpanId"] = Value::String(p.to_string());
    }
    span
}

fn status_json(error: Option<&str>) -> Value {
    match error {
        Some(msg) => json!({"code": 2, "message": msg}),
        None => json!({"code": 1}),
    }
}

fn attr_str(key: &str, value: &str) -> Value {
    json!({"key": key, "value": {"stringValue": value}})
}

fn attr_int(key: &str, value: u32) -> Value {
    json!({"key": key, "value": {"intValue": value.to_string()}})
}

fn attr_f64(key: &str, value: f64) -> Value {
    json!({"key": key, "value": {"doubleValue": value}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::{GenerationEvent, SpanEvent, TurnParams, with_turn};
    use std::sync::Arc;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_generation() -> GenerationEvent {
        GenerationEvent {
            name: "generation".into(),
            trace_id: "a".repeat(32),
            span_id: "b".repeat(16),
            parent_span_id: Some("c".repeat(16)),
            user: Some("alice".into()),
            session_id: Some("sess".into()),
            tags: vec!["aurora".into()],
            start_unix_nano: 1,
            end_unix_nano: 2,
            model: "m".into(),
            input: "in".into(),
            output: "out".into(),
            error: None,
            input_tokens: Some(4),
            output_tokens: Some(6),
            cost_usd: None,
        }
    }

    #[test]
    fn otlp_json_copies_user_and_session_onto_every_span() {
        let body = otlp_traces(vec![
            ExporterCmd::Generation(sample_generation()),
            ExporterCmd::Span(SpanEvent {
                name: "aurora-turn".into(),
                trace_id: "a".repeat(32),
                span_id: "c".repeat(16),
                parent_span_id: None,
                user: Some("alice".into()),
                session_id: Some("sess".into()),
                tags: vec!["aurora".into()],
                start_unix_nano: 0,
                end_unix_nano: 3,
                input: String::new(),
                output: String::new(),
                error: None,
            }),
        ]);
        let spans = body["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .expect("spans");
        assert_eq!(spans.len(), 2);
        for span in spans {
            let keys: Vec<&str> = span["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["key"].as_str().unwrap())
                .collect();
            assert!(keys.contains(&"langfuse.user.id"), "{keys:?}");
            assert!(keys.contains(&"langfuse.session.id"), "{keys:?}");
        }
        let generation = &spans[0];
        let keys: Vec<&str> = generation["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["key"].as_str().unwrap())
            .collect();
        assert!(keys.contains(&"gen_ai.request.model"));
        assert!(keys.contains(&"gen_ai.usage.input_tokens"));
        assert!(
            !keys.contains(&"gen_ai.usage.cost"),
            "cost must be omitted when the provider did not bill so Langfuse can infer"
        );
        assert_eq!(
            generation["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["key"] == "langfuse.observation.type")
                .unwrap()["value"]["stringValue"],
            "generation"
        );
    }

    #[test]
    fn otlp_json_includes_ingested_generation_cost() {
        let mut g = sample_generation();
        g.cost_usd = Some(0.00125);
        let body = otlp_traces(vec![ExporterCmd::Generation(g)]);
        let generation = &body["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        let cost = generation["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == "gen_ai.usage.cost")
            .unwrap_or_else(|| {
                panic!(
                    "missing gen_ai.usage.cost in {:?}",
                    generation["attributes"]
                )
            });
        assert_eq!(cost["value"]["doubleValue"], 0.00125);
    }

    #[tokio::test]
    async fn exporter_posts_otlp_with_ingestion_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(TRACES_PATH))
            .and(header("x-langfuse-ingestion-version", "4"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let handle = Arc::new(
            LangfuseHandle::start_with_host(&server.uri(), "pk-lf-test", "sk-lf-test").unwrap(),
        );
        let observer = handle.as_observer();
        with_turn(
            observer,
            TurnParams {
                name: "haiku".into(),
                user: Some("alice".into()),
                session_id: Some("s1".into()),
                tags: vec!["haiku".into()],
            },
            async {
                let rec = llm::observe::GenerationInProgress::start("m", "in").unwrap();
                rec.finish_ok("out", None);
            },
        )
        .await;
        handle.finalize().await.unwrap();
        assert!(handle.owner.handle.lock().await.is_none());
    }

    #[tokio::test]
    async fn exporter_survives_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(TRACES_PATH))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let handle =
            LangfuseHandle::start_with_host(&server.uri(), "pk-lf-test", "sk-lf-test").unwrap();
        handle.generation(sample_generation());
        handle.finalize().await.unwrap();
        assert!(handle.owner.handle.lock().await.is_none());
    }
}
