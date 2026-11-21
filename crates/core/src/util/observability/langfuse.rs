//! Langfuse as a filtered exporter on the OTel pipeline (ADR-0013 decision 5).
//!
//! A Turn is one `turn` span ([`with_turn`]). With `[langfuse]` configured,
//! the Turn span and every span started under it (`chat`, `execute_tool`,
//! `http.client`, …) also go to Langfuse's OTLP endpoint; its ancestors
//! (`irc.message`, `handler`, `command`) do not. The Langfuse copy of the Turn
//! span has no parent, so Langfuse shows the Turn as the trace root, and every
//! copy carries `langfuse.user.id` and `langfuse.session.id`. The collector
//! still gets the Turn inside its `irc.message` trace.
//!
//! Turns are traced whatever `observability.otel.traces_filter` says, and
//! without `[otel]`: the traces layer runs behind that filter *or*
//! [`TURN_FILTER`] (spans only). A span only the Turn arm let through skips the
//! collector.
//!
//! Membership is by OTel span id. [`TurnScope`] registers the Turn span when
//! it is created; [`Turns::adopt`] registers every span whose parent is a
//! member when the SDK starts it; an entry leaves when its span ends.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use eyre::WrapErr as _;
use opentelemetry::trace::{SpanId, TraceContextExt as _};
use opentelemetry::{Array, Context, KeyValue, StringValue, Value};
use opentelemetry_sdk::trace::SpanData;
use parking_lot::Mutex;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use secrecy::ExposeSecret as _;
use tracing::dispatcher::WeakDispatch;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Dispatch, Instrument as _, Subscriber};
use tracing_subscriber::layer::{Context as LayerContext, Layer};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, reload};

use super::attr;
use super::otel::SpanSinks;
use crate::config::LangfuseBootstrap;

/// Langfuse EU cloud, the default `[langfuse] host`.
const DEFAULT_HOST: &str = "https://cloud.langfuse.com";
const TRACES_PATH: &str = "/api/public/otel/v1/traces";
/// Per-request deadline, retries included. Langfuse is across the internet,
/// not on the LAN like the collector.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
/// Spans per request: prompts make Langfuse spans large.
pub(super) const MAX_EXPORT_BATCH: usize = 32;

/// Name of the Turn span.
const TURN_SPAN: &str = "turn";
/// Name of the tool span (`llm::execute_tool`).
const EXECUTE_TOOL_SPAN: &str = "execute_tool";
/// The traces-layer filter arm for Turns: every span inside a `turn` span.
pub(super) const TURN_FILTER: &str = "[turn]=info";

const LANGFUSE_USER_ID: &str = "langfuse.user.id";
const LANGFUSE_SESSION_ID: &str = "langfuse.session.id";
const LANGFUSE_TRACE_NAME: &str = "langfuse.trace.name";
const LANGFUSE_TRACE_TAGS: &str = "langfuse.trace.tags";

/// Run `fut` as one Turn: an agent's LLM work for one request, the root of
/// its Langfuse trace. `agent` is the `ai.agent` value; `speaker` is the
/// chatter's login, `None` for a Turn nobody asked for (dreamer, recap).
pub async fn with_turn<F: Future>(
    agent: &'static str,
    profile: &str,
    session_id: &str,
    speaker: Option<&str>,
    fut: F,
) -> F::Output {
    fut.instrument(turn_span(agent, profile, session_id, speaker))
        .await
}

fn turn_span(
    agent: &'static str,
    profile: &str,
    session_id: &str,
    speaker: Option<&str>,
) -> tracing::Span {
    tracing::info_span!(
        "turn",
        ai.agent = agent,
        ai.profile = profile,
        ai.turn.session_id = session_id,
        twitch.user.login = speaker,
    )
}

/// OTLP/HTTP span exporter for `config`'s Langfuse project.
pub(super) fn exporter(
    config: &LangfuseBootstrap,
) -> eyre::Result<opentelemetry_otlp::SpanExporter> {
    use opentelemetry_otlp::{WithExportConfig as _, WithHttpConfig as _};

    // reqwest is built `rustls-no-provider`; building any client without an
    // installed provider panics.
    crate::util::install_crypto_provider();
    let host = config
        .host
        .as_deref()
        .unwrap_or(DEFAULT_HOST)
        .trim_end_matches('/');
    let credentials = BASE64.encode(format!(
        "{}:{}",
        config.public_key.expose_secret(),
        config.secret_key.expose_secret()
    ));
    let mut auth = HeaderValue::from_str(&format!("Basic {credentials}"))
        .wrap_err("Langfuse keys are not a valid header value")?;
    auth.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, auth);
    // Without it, Langfuse may hold OTLP data back for up to 10 minutes.
    headers.insert(
        "x-langfuse-ingestion-version",
        HeaderValue::from_static("4"),
    );
    let client = reqwest::Client::builder()
        .user_agent(crate::APP_USER_AGENT)
        .default_headers(headers)
        .timeout(EXPORT_TIMEOUT)
        .build()
        .wrap_err("build Langfuse HTTP client")?;
    opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(client)
        .with_endpoint(format!("{host}{TRACES_PATH}"))
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .wrap_err("build Langfuse span exporter")
}

/// What Langfuse gets beyond the span itself, fixed when the Turn starts.
#[derive(Debug)]
pub(super) struct Turn {
    /// `langfuse.user.id`, `langfuse.session.id`: on every span.
    every: Vec<KeyValue>,
    /// `langfuse.trace.name`, `langfuse.trace.tags`: on the Turn span.
    root: Vec<KeyValue>,
}

impl Turn {
    /// From the fields [`turn_span`] sets. The user is the speaker, or
    /// `<agent>` for a Turn without one.
    fn new(attrs: &Attributes<'_>) -> Self {
        let mut fields = TurnFields::default();
        attrs.record(&mut fields);
        let agent: Arc<str> = fields.agent.unwrap_or_default().into();
        let user: Arc<str> = match fields.speaker {
            Some(login) => login.into(),
            None => format!("<{agent}>").into(),
        };
        let mut every = vec![KeyValue::new(LANGFUSE_USER_ID, user)];
        if let Some(session_id) = fields.session_id {
            every.push(KeyValue::new(
                LANGFUSE_SESSION_ID,
                Arc::<str>::from(session_id),
            ));
        }
        let tags = std::iter::once(StringValue::from(Arc::clone(&agent)))
            .chain(fields.profile.map(StringValue::from))
            .collect();
        Self {
            every,
            root: vec![
                KeyValue::new(LANGFUSE_TRACE_NAME, agent),
                KeyValue::new(LANGFUSE_TRACE_TAGS, Value::Array(Array::String(tags))),
            ],
        }
    }

    /// The Langfuse copy of `span`. The Turn span loses its parent, so Langfuse
    /// roots the trace there; the trace id stays the collector's. A tool span
    /// is named `execute_tool {gen_ai.tool.name}` (GenAI semconv), so
    /// Langfuse's tree tells the tools apart.
    pub(super) fn copy(&self, mut span: SpanData, is_turn: bool) -> SpanData {
        if is_turn {
            span.parent_span_id = SpanId::INVALID;
            span.parent_span_is_remote = false;
            span.attributes.extend(self.root.iter().cloned());
        }
        if span.name == EXECUTE_TOOL_SPAN
            && let Some(tool) = span
                .attributes
                .iter()
                .find(|kv| kv.key.as_str() == attr::GEN_AI_TOOL_NAME)
        {
            span.name = format!("{EXECUTE_TOOL_SPAN} {}", tool.value.as_str()).into();
        }
        span.attributes.extend(self.every.iter().cloned());
        span
    }
}

/// The [`turn_span`] fields a [`Turn`] is built from.
#[derive(Default)]
struct TurnFields {
    agent: Option<String>,
    profile: Option<String>,
    session_id: Option<String>,
    speaker: Option<String>,
}

impl TurnFields {
    fn set(&mut self, field: &Field, value: String) {
        let slot = match field.name() {
            attr::AI_AGENT => &mut self.agent,
            attr::AI_PROFILE => &mut self.profile,
            attr::AI_TURN_SESSION_ID => &mut self.session_id,
            attr::TWITCH_USER_LOGIN => &mut self.speaker,
            _ => return,
        };
        *slot = Some(value);
    }
}

impl Visit for TurnFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.set(field, value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.set(field, format!("{value:?}"));
    }
}

/// One span in [`Turns`].
#[derive(Debug)]
struct Member {
    /// `None`: not in a Turn, only kept from the collector.
    turn: Option<Arc<Turn>>,
    is_turn: bool,
    /// Whether the collector filter let the span through.
    collector: bool,
    /// OTel parent; `INVALID` when not known (the Turn span).
    parent: SpanId,
    /// The nearest ancestor the collector gets, when that is not `parent`:
    /// the collector copy's parent, and what a child of a span the collector
    /// skips hangs under. `INVALID` makes the collector copy a root.
    collector_parent: Option<SpanId>,
}

impl Member {
    /// What a child of this span hangs under in the collector, when that is
    /// not this span.
    fn collector_parent_of_child(&self) -> Option<SpanId> {
        (!self.collector).then(|| self.collector_parent.unwrap_or(self.parent))
    }
}

/// Where an ended span goes.
pub(super) struct Route {
    /// Whether the collector filter let the span through. Spans outside any
    /// Turn always are.
    pub(super) collector: bool,
    /// The collector copy's parent, when it is not the span's own.
    collector_parent: Option<SpanId>,
    /// The span's Turn, and whether it is the Turn span itself.
    pub(super) langfuse: Option<(Arc<Turn>, bool)>,
}

impl Route {
    /// The collector copy of `span`: hung under its nearest ancestor the
    /// collector gets, so a span the collector filter skipped inside a Turn
    /// leaves no hole in the collector's tree.
    pub(super) fn for_collector(&self, mut span: SpanData) -> SpanData {
        if let Some(parent) = self.collector_parent {
            span.parent_span_id = parent;
        }
        span
    }
}

/// Spans in flight that are Turns, belong to one, or skip the collector, by
/// OTel span id.
#[derive(Debug, Default)]
pub(super) struct Turns(Mutex<HashMap<SpanId, Member>>);

impl Turns {
    /// Make `span` a member of its parent's Turn, if the parent is in one.
    pub(super) fn adopt(&self, span: SpanId, parent: &Context) {
        if !parent.has_active_span() {
            return;
        }
        let parent = parent.span().span_context().span_id();
        let mut members = self.0.lock();
        let Some(member) = members
            .get(&parent)
            .filter(|m| m.turn.is_some())
            .map(|p| Member {
                turn: p.turn.clone(),
                is_turn: false,
                collector: true,
                parent,
                collector_parent: p.collector_parent_of_child(),
            })
        else {
            return;
        };
        members.insert(span, member);
    }

    /// Register the Turn span. `parent` is its parent in the collector, used
    /// only when the collector skips the Turn span itself.
    fn insert_turn(&self, span: SpanId, turn: Turn, collector: bool, parent: SpanId) {
        self.0.lock().insert(
            span,
            Member {
                turn: Some(Arc::new(turn)),
                is_turn: true,
                collector,
                parent,
                collector_parent: None,
            },
        );
    }

    fn skip_collector(&self, span: SpanId) {
        self.0
            .lock()
            .entry(span)
            .or_insert(Member {
                turn: None,
                is_turn: false,
                collector: true,
                parent: SpanId::INVALID,
                collector_parent: None,
            })
            .collector = false;
    }

    /// Where the ended `span` goes; it leaves the registry.
    pub(super) fn route(&self, span: SpanId) -> Route {
        match self.0.lock().remove(&span) {
            Some(member) => Route {
                collector: member.collector,
                collector_parent: member.collector_parent,
                langfuse: member.turn.map(|turn| (turn, member.is_turn)),
            },
            None => Route {
                collector: true,
                collector_parent: None,
                langfuse: None,
            },
        }
    }
}

/// The tracing side of the Langfuse sink, layered after the OTel traces layer
/// under the same filter: registers each Turn span, and marks the spans the
/// collector filter turned down (only the Turn arm let them through).
pub(super) struct TurnScope<S> {
    sinks: SpanSinks,
    /// The collector's `traces_filter`.
    collector_filter: reload::Handle<EnvFilter, S>,
    dispatch: OnceLock<WeakDispatch>,
}

impl<S> TurnScope<S> {
    pub(super) fn new(sinks: SpanSinks, collector_filter: reload::Handle<EnvFilter, S>) -> Self {
        Self {
            sinks,
            collector_filter,
            dispatch: OnceLock::new(),
        }
    }

    /// The OTel span id of `id`, starting the OTel span if it has not been.
    fn otel_span_id(&self, id: &Id) -> Option<SpanId> {
        let dispatch = self.dispatch.get()?.upgrade()?;
        let cx = tracing_opentelemetry::get_otel_context(id, &dispatch)?;
        Some(cx.span().span_context().span_id())
    }
}

impl<S> Layer<S> for TurnScope<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_register_dispatch(&self, dispatch: &Dispatch) {
        self.dispatch.get_or_init(|| dispatch.downgrade());
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: LayerContext<'_, S>) {
        if !self.sinks.langfuse_on() {
            return;
        }
        let is_turn = attrs.metadata().name() == TURN_SPAN;
        let collector_on = self.sinks.collector_on();
        let collector = collector_on
            && self
                .collector_filter
                .with_current(|filter| filter.enabled(attrs.metadata(), ctx.clone()))
                .unwrap_or(false);
        if !is_turn && (collector || !collector_on) {
            return;
        }
        let Some(span) = self.otel_span_id(id) else {
            return;
        };
        if !is_turn {
            self.sinks.turns().skip_collector(span);
            return;
        }
        // The Turn's ancestors are outside any Turn, so only the collector
        // filter let them through: the nearest one with an OTel span is the
        // Turn's collector parent, needed when the collector skips the Turn.
        let parent = if collector || !collector_on {
            SpanId::INVALID
        } else {
            ctx.span(id)
                .and_then(|span| {
                    span.scope()
                        .skip(1)
                        .find_map(|ancestor| self.otel_span_id(&ancestor.id()))
                })
                .unwrap_or(SpanId::INVALID)
        };
        self.sinks
            .turns()
            .insert_turn(span, Turn::new(attrs), collector, parent);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use base64::Engine as _;
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use opentelemetry_proto::tonic::common::v1::any_value;
    use opentelemetry_proto::tonic::trace::v1::Span;
    use prost::Message as _;
    use secrecy::SecretString;
    use tracing::Instrument as _;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::build_subscriber;
    use super::{TRACES_PATH, with_turn};
    use crate::config::{LangfuseBootstrap, OtelBootstrap};
    use crate::settings::{
        Actor, MemoryAuditLog, ObservabilityOverrides, OtelOverrides, SettingsOverrides,
        SettingsStore,
    };

    async fn endpoint() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        server
    }

    fn langfuse_config(server: &MockServer) -> LangfuseBootstrap {
        LangfuseBootstrap {
            public_key: SecretString::new("pk-lf-test".into()),
            secret_key: SecretString::new("sk-lf-test".into()),
            host: Some(format!("{}/", server.uri())),
        }
    }

    /// Every span POSTed to `path`.
    async fn spans(server: &MockServer, path: &str) -> Vec<Span> {
        let requests = server.received_requests().await.expect("recording");
        requests
            .iter()
            .filter(|r| r.url.path() == path)
            .flat_map(|r| {
                ExportTraceServiceRequest::decode(r.body.as_slice())
                    .expect("OTLP protobuf")
                    .resource_spans
            })
            .flat_map(|r| r.scope_spans)
            .flat_map(|s| s.spans)
            .collect()
    }

    fn attr<'a>(span: &'a Span, key: &str) -> Option<&'a str> {
        span.attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref()?.value.as_ref())
            .and_then(|value| match value {
                any_value::Value::StringValue(s) => Some(s.as_str()),
                _ => None,
            })
    }

    fn by_name(spans: &[Span]) -> HashMap<&str, &Span> {
        spans.iter().map(|s| (s.name.as_str(), s)).collect()
    }

    /// One `!ai` message: `irc.message` → `handler` → `command` → `turn` →
    /// `chat` and `execute_tool` → `http.client`, the LLM spans on the `llm`
    /// target like the real ones.
    async fn ai_message() {
        let message = tracing::info_span!("irc.message");
        let handler = tracing::info_span!(parent: &message, "handler");
        let command = tracing::info_span!(parent: &handler, "command");
        let call = llm::ToolCall {
            id: "c1".into(),
            name: "web_search".into(),
            arguments: serde_json::json!({"query": "x"}),
            arguments_parse_error: None,
        };
        let turn = async {
            let chat = tracing::info_span!(target: "llm", "chat", gen_ai.operation.name = "chat");
            drop(chat.entered());
            llm::execute_tool("web_search", &call, async {
                drop(tracing::info_span!("http.client").entered());
                llm::ToolResultMessage::for_call(&call, "found x")
            })
            .await
        };
        with_turn("aurora", "main", "sess-1", Some("alice"), turn).instrument(command).await;
    }

    /// The Turn tree reaches Langfuse with `[otel]` absent: the Turn as the
    /// root, its children under it, user and session on every span, Basic
    /// auth, and none of the Turn's ancestors.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwards_turn_tree_without_otel() {
        let langfuse = endpoint().await;
        let settings = crate::settings::test_handle();
        // Scoped subscriber: no global subscriber or meter provider leaks.
        let (subscriber, obs) = build_subscriber(std::io::sink, || None);
        let _default = tracing::subscriber::set_default(subscriber);
        let export = obs
            .enable_otel(None, Some(&langfuse_config(&langfuse)), &settings)
            .expect("Langfuse export enabled");

        ai_message().await;
        // Outside any Turn: not Langfuse's.
        drop(tracing::info_span!("irc.message").entered());
        export.shutdown().await;

        let requests = langfuse.received_requests().await.expect("recording");
        assert!(!requests.is_empty(), "nothing reached Langfuse");
        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("pk-lf-test:sk-lf-test")
        );
        for request in &requests {
            assert_eq!(request.url.path(), TRACES_PATH);
            let header = |name: &str| request.headers.get(name).and_then(|v| v.to_str().ok());
            assert_eq!(header("authorization"), Some(basic.as_str()));
            assert_eq!(header("x-langfuse-ingestion-version"), Some("4"));
        }

        let spans = spans(&langfuse, TRACES_PATH).await;
        let mut names: Vec<_> = spans.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["chat", "execute_tool web_search", "http.client", "turn"]
        );
        let tree = by_name(&spans);
        let turn = tree["turn"];
        let tool = tree["execute_tool web_search"];
        assert!(
            turn.parent_span_id.is_empty(),
            "the Turn is the Langfuse root"
        );
        assert_eq!(tree["chat"].parent_span_id, turn.span_id);
        assert_eq!(tool.parent_span_id, turn.span_id);
        assert_eq!(tree["http.client"].parent_span_id, tool.span_id);
        assert_eq!(
            attr(tool, "gen_ai.tool.call.arguments"),
            Some(r#"{"query":"x"}"#)
        );
        assert_eq!(attr(tool, "gen_ai.tool.call.result"), Some("found x"));
        for span in &spans {
            assert_eq!(span.trace_id, turn.trace_id, "{}", span.name);
            assert_eq!(
                attr(span, "langfuse.user.id"),
                Some("alice"),
                "{}",
                span.name
            );
            assert_eq!(
                attr(span, "langfuse.session.id"),
                Some("sess-1"),
                "{}",
                span.name
            );
        }
        assert_eq!(attr(turn, "langfuse.trace.name"), Some("aurora"));
    }

    /// With `[otel]` too, and a traces filter that leaves out the `llm`
    /// target: the collector keeps the Turn inside its `irc.message` trace and
    /// gets nothing the filter turned down, while Langfuse still gets the
    /// whole Turn tree.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collector_keeps_its_filter_and_its_tree() {
        let collector = endpoint().await;
        let langfuse = endpoint().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let (store, settings) =
            SettingsStore::open(dir.path(), Arc::new(MemoryAuditLog::new()), "chan")
                .expect("open store");
        let patch = SettingsOverrides {
            observability: ObservabilityOverrides {
                otel: OtelOverrides {
                    traces_filter: Some("warn,twitch_1337_core=info".into()),
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

        let (subscriber, obs) = build_subscriber(std::io::sink, || None);
        let _default = tracing::subscriber::set_default(subscriber);
        let otel = OtelBootstrap {
            endpoint: collector.uri(),
            auth_header: None,
        };
        // `Pipeline` calls, not `enable_otel`: no global meter provider.
        let collector_export = obs.otel.enable(&otel, &settings).expect("collector");
        assert!(obs.otel.enable_langfuse(&langfuse_config(&langfuse)));
        let export = obs.otel.export(Some(collector_export));

        ai_message().await;
        export.shutdown().await;

        let tempo = spans(&collector, "/v1/traces").await;
        let mut names: Vec<_> = tempo.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["command", "handler", "http.client", "irc.message", "turn"]
        );
        let tempo = by_name(&tempo);
        assert_eq!(tempo["turn"].parent_span_id, tempo["command"].span_id);
        // The `execute_tool` between them was filtered out of the collector.
        assert_eq!(tempo["http.client"].parent_span_id, tempo["turn"].span_id);
        assert_eq!(attr(tempo["turn"], "langfuse.user.id"), None);

        let lf = spans(&langfuse, TRACES_PATH).await;
        let mut names: Vec<_> = lf.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["chat", "execute_tool web_search", "http.client", "turn"]
        );
        let lf = by_name(&lf);
        assert!(lf["turn"].parent_span_id.is_empty());
        assert_eq!(lf["turn"].span_id, tempo["turn"].span_id);
        assert_eq!(lf["turn"].trace_id, tempo["irc.message"].trace_id);
        assert_eq!(
            lf["http.client"].parent_span_id,
            lf["execute_tool web_search"].span_id
        );
    }

    /// A Turn the collector filter skips hangs, in the collector, under its
    /// nearest exported ancestor, even past an ancestor no layer took: the
    /// collector gets `irc.message` and the Turn's `http.client`, the latter
    /// directly under the former.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn skipped_turn_hangs_under_nearest_exported_ancestor() {
        let collector = endpoint().await;
        let langfuse = endpoint().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let (store, settings) =
            SettingsStore::open(dir.path(), Arc::new(MemoryAuditLog::new()), "chan")
                .expect("open store");
        // This test module only: not the `turn` span's module, not `dropped`.
        let patch = SettingsOverrides {
            observability: ObservabilityOverrides {
                otel: OtelOverrides {
                    traces_filter: Some(format!("warn,{}=info", module_path!())),
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

        let (subscriber, obs) = build_subscriber(std::io::sink, || None);
        let _default = tracing::subscriber::set_default(subscriber);
        let otel = OtelBootstrap {
            endpoint: collector.uri(),
            auth_header: None,
        };
        let collector_export = obs.otel.enable(&otel, &settings).expect("collector");
        assert!(obs.otel.enable_langfuse(&langfuse_config(&langfuse)));
        let export = obs.otel.export(Some(collector_export));

        let message = tracing::info_span!("irc.message");
        let command = tracing::info_span!(target: "dropped", parent: &message, "command");
        let turn = async { drop(tracing::info_span!("http.client").entered()) };
        with_turn("aurora", "main", "sess-2", None, turn)
            .instrument(command)
            .await;
        drop(message);
        export.shutdown().await;

        let tempo = spans(&collector, "/v1/traces").await;
        let mut names: Vec<_> = tempo.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["http.client", "irc.message"]);
        let tempo = by_name(&tempo);
        assert_eq!(
            tempo["http.client"].parent_span_id,
            tempo["irc.message"].span_id
        );

        let lf = spans(&langfuse, TRACES_PATH).await;
        let mut names: Vec<_> = lf.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["http.client", "turn"]);
    }
}
