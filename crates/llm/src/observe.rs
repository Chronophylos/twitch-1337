//! Task-local LLM turn tracing.
//!
//! When a [`TurnContext`] is in scope, [`OpenAiClient`] / [`OllamaClient`]
//! record generations and [`run_agent`] records tool spans. Absent a context,
//! every hook is a no-op so tests and untraced call sites stay unchanged.

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::{Message, ToolCall, ToolResultMessage};

/// Sink for finished generations and nested spans. Implemented by the
/// Langfuse exporter in `core`; tests use a recording double.
pub trait LlmObserver: Send + Sync {
    fn generation(&self, event: GenerationEvent);
    fn span(&self, event: SpanEvent);
}

/// Identity for one Turn (one Langfuse trace / session).
#[derive(Debug, Clone)]
pub struct TurnParams {
    pub name: String,
    pub user: Option<String>,
    pub session_id: Option<String>,
    pub tags: Vec<String>,
}

/// Token counts plus optional billed USD from the provider response.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// OpenRouter `usage.cost` (USD credits). `None` for providers that
    /// only return token counts.
    pub cost_usd: Option<f64>,
}

/// A finished LLM generation observation.
#[derive(Debug, Clone)]
pub struct GenerationEvent {
    pub name: String,
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub user: Option<String>,
    pub session_id: Option<String>,
    pub tags: Vec<String>,
    pub start_unix_nano: u64,
    pub end_unix_nano: u64,
    pub model: String,
    pub input: String,
    pub output: String,
    pub error: Option<String>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    /// Billed USD from the provider (`usage.cost` on OpenRouter). `None`
    /// when the provider did not report a cost so Langfuse can infer.
    pub cost_usd: Option<f64>,
}

/// A finished nested span (root Turn or a tool call).
#[derive(Debug, Clone)]
pub struct SpanEvent {
    pub name: String,
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub user: Option<String>,
    pub session_id: Option<String>,
    pub tags: Vec<String>,
    pub start_unix_nano: u64,
    pub end_unix_nano: u64,
    pub input: String,
    pub output: String,
    pub error: Option<String>,
}

struct TurnInner {
    params: TurnParams,
    observer: Arc<dyn LlmObserver>,
    trace_id: String,
    start_unix_nano: u64,
    /// Parent stack; the last entry is the current parent for new children.
    stack: Mutex<Vec<String>>,
}

tokio::task_local! {
    static TURN: Arc<TurnInner>;
}

static ID_SEQ: AtomicU64 = AtomicU64::new(1);

fn unix_nano() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// 16-byte trace id / 8-byte span id as lowercase hex. Process-unique, not
/// cryptographic — good enough for a single-bot Langfuse project.
fn new_id_hex(nbytes: usize) -> String {
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = unix_nano();
    let a = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(seq);
    let b = nanos.rotate_left(21) ^ seq.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    match nbytes {
        8 => format!("{a:016x}"),
        _ => format!("{a:016x}{b:016x}"),
    }
}

fn current() -> Option<Arc<TurnInner>> {
    TURN.try_with(Clone::clone).ok()
}

/// Run `fut` inside a Turn. When `observer` is `None`, just awaits `fut`.
pub async fn with_optional_turn<T>(
    observer: Option<Arc<dyn LlmObserver>>,
    params: TurnParams,
    fut: impl Future<Output = T>,
) -> T {
    match observer {
        Some(obs) => with_turn(obs, params, fut).await,
        None => fut.await,
    }
}

/// Run `fut` as one Langfuse trace. Emits the root span when `fut` completes.
pub async fn with_turn<T>(
    observer: Arc<dyn LlmObserver>,
    params: TurnParams,
    fut: impl Future<Output = T>,
) -> T {
    let root_span_id = new_id_hex(8);
    let inner = Arc::new(TurnInner {
        params,
        observer: observer.clone(),
        trace_id: new_id_hex(16),
        start_unix_nano: unix_nano(),
        stack: Mutex::new(vec![root_span_id.clone()]),
    });
    let out = TURN.scope(inner.clone(), fut).await;
    observer.span(SpanEvent {
        name: inner.params.name.clone(),
        trace_id: inner.trace_id.clone(),
        span_id: root_span_id,
        parent_span_id: None,
        user: inner.params.user.clone(),
        session_id: inner.params.session_id.clone(),
        tags: inner.params.tags.clone(),
        start_unix_nano: inner.start_unix_nano,
        end_unix_nano: unix_nano(),
        input: String::new(),
        output: String::new(),
        error: None,
    });
    out
}

/// In-flight generation; call [`finish_ok`](GenerationInProgress::finish_ok)
/// or [`finish_err`](GenerationInProgress::finish_err). Dropping without
/// finish records a generic error so a forgotten path still shows up.
pub struct GenerationInProgress {
    inner: Arc<TurnInner>,
    span_id: String,
    parent_span_id: Option<String>,
    start_unix_nano: u64,
    model: String,
    input: String,
    finished: bool,
}

impl GenerationInProgress {
    /// `None` when no Turn is in scope.
    pub fn start(model: impl Into<String>, input: impl Into<String>) -> Option<Self> {
        let inner = current()?;
        let span_id = new_id_hex(8);
        let parent_span_id = inner.stack.lock().ok().and_then(|s| s.last().cloned());
        Some(Self {
            inner,
            span_id,
            parent_span_id,
            start_unix_nano: unix_nano(),
            model: model.into(),
            input: input.into(),
            finished: false,
        })
    }

    pub fn finish_ok(mut self, output: impl Into<String>, usage: Option<TokenUsage>) {
        let (input_tokens, output_tokens, cost_usd) = match usage {
            Some(u) => (Some(u.input_tokens), Some(u.output_tokens), u.cost_usd),
            None => (None, None, None),
        };
        self.emit(output.into(), None, input_tokens, output_tokens, cost_usd);
        self.finished = true;
    }

    pub fn finish_err(mut self, err: &dyn fmt::Display) {
        self.emit(String::new(), Some(err.to_string()), None, None, None);
        self.finished = true;
    }

    fn emit(
        &mut self,
        output: String,
        error: Option<String>,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
        cost_usd: Option<f64>,
    ) {
        self.inner.observer.generation(GenerationEvent {
            name: "generation".to_string(),
            trace_id: self.inner.trace_id.clone(),
            span_id: self.span_id.clone(),
            parent_span_id: self.parent_span_id.clone(),
            user: self.inner.params.user.clone(),
            session_id: self.inner.params.session_id.clone(),
            tags: self.inner.params.tags.clone(),
            start_unix_nano: self.start_unix_nano,
            end_unix_nano: unix_nano(),
            model: self.model.clone(),
            input: self.input.clone(),
            output,
            error,
            input_tokens,
            output_tokens,
            cost_usd,
        });
    }
}

impl Drop for GenerationInProgress {
    fn drop(&mut self) {
        if !self.finished {
            self.emit(String::new(), Some("dropped".to_string()), None, None, None);
        }
    }
}

/// Attach a generation outcome to an in-flight recorder.
pub fn finish_generation<T, E: fmt::Display>(
    rec: Option<GenerationInProgress>,
    result: &std::result::Result<T, E>,
    ok: impl FnOnce(&T) -> (String, Option<TokenUsage>),
) {
    match (rec, result) {
        (Some(r), Ok(v)) => {
            let (output, usage) = ok(v);
            r.finish_ok(output, usage);
        }
        (Some(r), Err(e)) => r.finish_err(e),
        _ => {}
    }
}

/// JSON array of `{role, content}` for generation input.
pub fn messages_json(messages: &[Message]) -> String {
    let rows: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "role": m.role.to_string(),
                "content": m.content,
            })
        })
        .collect();
    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string())
}

/// Record a tool span around `execute`.
pub async fn observe_tool<E>(executor: &E, call: &ToolCall) -> ToolResultMessage
where
    E: crate::agent::ToolExecutor + ?Sized,
{
    let Some(inner) = current() else {
        return executor.execute(call).await;
    };
    let span_id = new_id_hex(8);
    let parent_span_id = {
        let mut stack = inner
            .stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let parent = stack.last().cloned();
        stack.push(span_id.clone());
        parent
    };
    let start_unix_nano = unix_nano();
    let result = executor.execute(call).await;
    if let Ok(mut stack) = inner.stack.lock() {
        stack.pop();
    }
    inner.observer.span(SpanEvent {
        name: format!("tool {}", call.name),
        trace_id: inner.trace_id.clone(),
        span_id,
        parent_span_id,
        user: inner.params.user.clone(),
        session_id: inner.params.session_id.clone(),
        tags: inner.params.tags.clone(),
        start_unix_nano,
        end_unix_nano: unix_nano(),
        input: call.arguments.to_string(),
        output: result.content.clone(),
        error: None,
    });
    result
}

#[cfg(test)]
pub(crate) struct RecordingObserver {
    pub generations: Mutex<Vec<GenerationEvent>>,
    pub spans: Mutex<Vec<SpanEvent>>,
}

#[cfg(test)]
impl RecordingObserver {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            generations: Mutex::new(Vec::new()),
            spans: Mutex::new(Vec::new()),
        })
    }
}

#[cfg(test)]
impl LlmObserver for RecordingObserver {
    fn generation(&self, event: GenerationEvent) {
        self.generations.lock().unwrap().push(event);
    }
    fn span(&self, event: SpanEvent) {
        self.spans.lock().unwrap().push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Role;

    #[tokio::test]
    async fn no_observer_is_a_noop() {
        let rec = GenerationInProgress::start("m", "in");
        assert!(rec.is_none());
        let n = with_optional_turn(
            None,
            TurnParams {
                name: "t".into(),
                user: None,
                session_id: None,
                tags: vec![],
            },
            async { 7 },
        )
        .await;
        assert_eq!(n, 7);
    }

    #[tokio::test]
    async fn with_turn_emits_root_span_and_generation() {
        let obs = RecordingObserver::new();
        let obs_dyn: Arc<dyn LlmObserver> = obs.clone();
        with_turn(
            obs_dyn,
            TurnParams {
                name: "aurora-turn".into(),
                user: Some("alice".into()),
                session_id: Some("sess".into()),
                tags: vec!["aurora".into()],
            },
            async {
                let rec = GenerationInProgress::start("model-x", "[{\"role\":\"user\"}]").unwrap();
                rec.finish_ok(
                    "hello",
                    Some(TokenUsage {
                        input_tokens: 3,
                        output_tokens: 5,
                        cost_usd: Some(0.0125),
                    }),
                );
            },
        )
        .await;

        let gens = obs.generations.lock().unwrap();
        assert_eq!(gens.len(), 1);
        assert_eq!(gens[0].model, "model-x");
        assert_eq!(gens[0].output, "hello");
        assert_eq!(gens[0].input_tokens, Some(3));
        assert_eq!(gens[0].output_tokens, Some(5));
        assert_eq!(gens[0].cost_usd, Some(0.0125));
        assert_eq!(gens[0].user.as_deref(), Some("alice"));
        assert_eq!(gens[0].session_id.as_deref(), Some("sess"));
        assert!(gens[0].parent_span_id.is_some());

        let spans = obs.spans.lock().unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "aurora-turn");
        assert_eq!(spans[0].parent_span_id, None);
        assert_eq!(gens[0].trace_id, spans[0].trace_id);
        assert_eq!(
            gens[0].parent_span_id.as_deref(),
            Some(spans[0].span_id.as_str())
        );
    }

    #[test]
    fn messages_json_includes_roles() {
        let json = messages_json(&[Message {
            role: Role::User,
            content: "hi".into(),
        }]);
        assert!(json.contains("user"));
        assert!(json.contains("hi"));
    }
}
