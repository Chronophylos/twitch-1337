//! State instruments (docs/observability.md § Metrics). RED comes from
//! spans; these hold only state spans cannot express. Every label is a
//! bounded set: `&'static str` values or an operator-configured profile name,
//! never a user id, login, callsign or message text.
//!
//! [`init`] creates the instruments once `Observability::enable_otel` has set
//! the meter provider. Before that, and without `[otel]`, every function here
//! returns without touching anything.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use llm::TokenUsage;
use opentelemetry::metrics::{Counter, Gauge, Meter};
use opentelemetry::{KeyValue, StringValue};

use super::attr;

static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

tokio::task_local! {
    static LLM_SCOPE: LlmScope;
}

/// `ai.agent` / `ai.profile` when an LLM call runs outside [`with_llm_scope`].
const UNKNOWN: &str = "unknown";

/// Labels for the LLM calls made inside [`with_llm_scope`].
struct LlmScope {
    agent: &'static str,
    profile: StringValue,
}

struct Instruments {
    meter: Meter,
    broadcast_lagged: Counter<u64>,
    llm_tokens: Counter<u64>,
    llm_cost: Counter<f64>,
    flights_tracked: Gauge<u64>,
}

impl Instruments {
    fn new(meter: Meter) -> Self {
        Self {
            broadcast_lagged: meter
                .u64_counter("broadcast.lagged")
                .with_unit("{message}")
                .with_description(
                    "IRC messages a broadcast consumer skipped because it fell behind",
                )
                .build(),
            llm_tokens: meter
                .u64_counter("llm.tokens")
                .with_unit("{token}")
                .with_description("LLM tokens the provider reported, by direction")
                .build(),
            llm_cost: meter
                .f64_counter("llm.cost")
                .with_unit("{USD}")
                .with_description("LLM cost the provider billed (OpenRouter usage.cost)")
                .build(),
            flights_tracked: meter
                .u64_gauge("tracker.flights.tracked")
                .with_unit("{flight}")
                .with_description("Tracked Flights by Flight Phase")
                .build(),
            meter,
        }
    }

    fn broadcast_lagged(&self, handler: &'static str, skipped: u64) {
        self.broadcast_lagged
            .add(skipped, &[KeyValue::new(attr::BOT_HANDLER, handler)]);
    }

    fn llm_usage(&self, scope: &LlmScope, usage: &TokenUsage) {
        let agent = KeyValue::new(attr::AI_AGENT, scope.agent);
        let profile = KeyValue::new(attr::AI_PROFILE, scope.profile.clone());
        for (token_type, count) in [
            ("input", usage.input_tokens),
            ("output", usage.output_tokens),
        ] {
            if count > 0 {
                self.llm_tokens.add(
                    u64::from(count),
                    &[
                        agent.clone(),
                        profile.clone(),
                        KeyValue::new(attr::GEN_AI_TOKEN_TYPE, token_type),
                    ],
                );
            }
        }
        if let Some(cost) = usage.cost_usd.filter(|c| c.is_finite() && *c > 0.0) {
            self.llm_cost.add(cost, &[agent, profile]);
        }
    }

    /// Labels from the enclosing [`with_llm_scope`], `unknown` outside one.
    fn llm_usage_in_scope(&self, usage: &TokenUsage) {
        if LLM_SCOPE
            .try_with(|scope| self.llm_usage(scope, usage))
            .is_err()
        {
            let unknown = LlmScope {
                agent: UNKNOWN,
                profile: StringValue::from(UNKNOWN),
            };
            self.llm_usage(&unknown, usage);
        }
    }

    /// `profile` with the enclosing scope's agent: a call on its own profile
    /// inside an agent's scope (the media tool).
    fn llm_usage_on_profile(&self, profile: &str, usage: &TokenUsage) {
        let scope = LlmScope {
            agent: LLM_SCOPE.try_with(|scope| scope.agent).unwrap_or(UNKNOWN),
            profile: StringValue::from(profile.to_owned()),
        };
        self.llm_usage(&scope, usage);
    }

    fn flights_tracked(&self, by_phase: impl IntoIterator<Item = (&'static str, u64)>) {
        for (phase, count) in by_phase {
            self.flights_tracked
                .record(count, &[KeyValue::new(attr::AVIATION_FLIGHT_PHASE, phase)]);
        }
    }
}

/// Create the instruments from `meter` and install the LLM usage hook. Only
/// the first call has an effect.
pub(super) fn init(meter: Meter) {
    if INSTRUMENTS.set(Instruments::new(meter)).is_ok() {
        llm::set_usage_hook(record_llm_usage);
    }
}

/// `broadcast.lagged`: `handler` (`bot.handler`, a fixed snake_case name)
/// fell behind the IRC broadcast and lost `skipped` messages.
pub fn broadcast_lagged(handler: &'static str, skipped: u64) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.broadcast_lagged(handler, skipped);
    }
}

/// `tracker.flights.tracked`: Tracked Flights per `aviation.flight.phase`.
/// Pass every phase, zeros included, so a phase that empties reads 0.
pub fn flights_tracked(by_phase: impl IntoIterator<Item = (&'static str, u64)>) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.flights_tracked(by_phase);
    }
}

/// `irc.latency`: the latency EMA in `latency`, read at every export.
/// Call once, after `enable_otel`.
pub fn observe_irc_latency(latency: Arc<AtomicU32>) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    // The SDK keeps the callback for the provider's lifetime; the returned
    // handle is only a marker.
    let _gauge = instruments
        .meter
        .u64_observable_gauge("irc.latency")
        .with_unit("ms")
        .with_description("One-way IRC latency estimate (EMA of PING/PONG round trips / 2)")
        .with_callback(move |gauge| gauge.observe(u64::from(latency.load(Ordering::Relaxed)), &[]))
        .build();
}

/// `memory.bytes`: registers the gauge over one cached value per `kinds`
/// entry and returns the cache for a sampler to refresh, so the export
/// callback never touches the disk. `None` when metrics are off.
/// Call once, after `enable_otel`.
pub fn memory_bytes_gauge<const N: usize>(kinds: [&'static str; N]) -> Option<Arc<[AtomicU64; N]>> {
    let instruments = INSTRUMENTS.get()?;
    let cache: Arc<[AtomicU64; N]> = Arc::new(std::array::from_fn(|_| AtomicU64::new(0)));
    let read = Arc::clone(&cache);
    let _gauge = instruments
        .meter
        .u64_observable_gauge("memory.bytes")
        .with_unit("By")
        .with_description("Memory and Scratch bytes on disk by file kind")
        .with_callback(move |gauge| {
            for (kind, bytes) in kinds.iter().zip(read.iter()) {
                gauge.observe(
                    bytes.load(Ordering::Relaxed),
                    &[KeyValue::new(attr::MEMORY_KIND, *kind)],
                );
            }
        })
        .build();
    Some(cache)
}

/// Run `fut` with its LLM calls counted under `ai.agent = agent` and
/// `ai.profile = profile` (`llm.tokens` / `llm.cost`). `agent` is a fixed
/// name: the agent, or the feature calling the LLM outside one. With metrics
/// off, just awaits `fut`.
pub async fn with_llm_scope<F: Future>(agent: &'static str, profile: &str, fut: F) -> F::Output {
    if INSTRUMENTS.get().is_none() {
        return fut.await;
    }
    llm_scoped(agent, profile, fut).await
}

async fn llm_scoped<F: Future>(agent: &'static str, profile: &str, fut: F) -> F::Output {
    let scope = LlmScope {
        agent,
        profile: StringValue::from(Arc::<str>::from(profile)),
    };
    LLM_SCOPE.scope(scope, fut).await
}

/// Count `usage` under the enclosing [`with_llm_scope`]. The `llm` crate
/// calls this through its usage hook.
pub fn record_llm_usage(usage: &TokenUsage) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.llm_usage_in_scope(usage);
    }
}

/// Count `usage` of a completion made outside `llm` on its own `profile`
/// (the media client), under the enclosing scope's `ai.agent`.
pub fn record_llm_usage_on_profile(profile: &str, usage: &TokenUsage) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.llm_usage_on_profile(profile, usage);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    use super::*;

    /// Instruments on a private provider (never the global one, which would
    /// leak into other tests), plus the exporter that collects them. The
    /// interval is explicit (it overrides `OTEL_METRIC_EXPORT_INTERVAL`) and
    /// long, so only `force_flush` ever exports.
    fn instruments() -> (Instruments, SdkMeterProvider, InMemoryMetricExporter) {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone())
            .with_interval(Duration::from_secs(24 * 60 * 60))
            .build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        (Instruments::new(provider.meter("test")), provider, exporter)
    }

    /// Data points of the counter `name` as (sorted labels, value), from one
    /// fresh export (cumulative, so it holds every recording so far).
    fn points<T: Copy>(
        provider: &SdkMeterProvider,
        exporter: &InMemoryMetricExporter,
        name: &str,
        pick: fn(&AggregatedMetrics) -> Option<&MetricData<T>>,
    ) -> Vec<(Vec<(String, String)>, T)> {
        exporter.reset();
        provider.force_flush().unwrap();
        let finished = exporter.get_finished_metrics().unwrap();
        let mut out = Vec::new();
        for resource in &finished {
            for scope in resource.scope_metrics() {
                for metric in scope.metrics().filter(|m| m.name() == name) {
                    let Some(MetricData::Sum(sum)) = pick(metric.data()) else {
                        panic!("{name} is not a sum");
                    };
                    for point in sum.data_points() {
                        let mut labels: Vec<(String, String)> = point
                            .attributes()
                            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
                            .collect();
                        labels.sort();
                        out.push((labels, point.value()));
                    }
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn u64_data(data: &AggregatedMetrics) -> Option<&MetricData<u64>> {
        match data {
            AggregatedMetrics::U64(d) => Some(d),
            _ => None,
        }
    }

    fn f64_data(data: &AggregatedMetrics) -> Option<&MetricData<f64>> {
        match data {
            AggregatedMetrics::F64(d) => Some(d),
            _ => None,
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut v: Vec<_> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn broadcast_lagged_sums_skipped_per_handler_and_carries_only_the_handler() {
        let (instruments, provider, exporter) = instruments();
        instruments.broadcast_lagged("commands", 3);
        instruments.broadcast_lagged("commands", 4);
        instruments.broadcast_lagged("transcript", 1);

        assert_eq!(
            points(&provider, &exporter, "broadcast.lagged", u64_data),
            vec![
                (labels(&[("bot.handler", "commands")]), 7),
                (labels(&[("bot.handler", "transcript")]), 1),
            ]
        );
    }

    /// Labels come from the enclosing scope, a nested profile override keeps
    /// the agent, and a call outside any scope is still counted.
    #[tokio::test]
    async fn llm_usage_labels_are_agent_profile_and_direction_only() {
        let (instruments, provider, exporter) = instruments();
        let instruments = &instruments;
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            cost_usd: Some(0.5),
        };
        llm_scoped("aurora", "main", async {
            instruments.llm_usage_in_scope(&usage);
            instruments.llm_usage_on_profile(
                "vision",
                &TokenUsage {
                    input_tokens: 7,
                    output_tokens: 0,
                    cost_usd: None,
                },
            );
        })
        .await;
        instruments.llm_usage_in_scope(&usage);

        let tokens = points(&provider, &exporter, "llm.tokens", u64_data);
        let row = |agent, profile, dir| {
            labels(&[
                ("ai.agent", agent),
                ("ai.profile", profile),
                ("gen_ai.token.type", dir),
            ])
        };
        let mut expected = vec![
            (row("aurora", "main", "input"), 100),
            (row("aurora", "main", "output"), 20),
            // Zero output tokens add no series.
            (row("aurora", "vision", "input"), 7),
            (row("unknown", "unknown", "input"), 100),
            (row("unknown", "unknown", "output"), 20),
        ];
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(tokens, expected);

        let cost = points(&provider, &exporter, "llm.cost", f64_data);
        assert_eq!(
            cost,
            vec![
                (
                    labels(&[("ai.agent", "aurora"), ("ai.profile", "main")]),
                    0.5
                ),
                (
                    labels(&[("ai.agent", "unknown"), ("ai.profile", "unknown")]),
                    0.5
                ),
            ]
        );
    }
}
