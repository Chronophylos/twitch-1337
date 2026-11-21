//! Signal export runtime settings. The collector endpoint is bootstrap
//! (`[otel]` in config.toml); only the per-sink filters live here, applied
//! live through `tracing_subscriber::reload` (ADR-0013 decision 4).

use serde::{Deserialize, Serialize};

use super::knob::Knobs;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = ObservabilityOverrides)]
pub struct ObservabilitySettings {
    #[knob(nested)]
    pub otel: OtelSettings,
}

/// `EnvFilter` directive strings for the two OTel sinks. stdout keeps
/// `RUST_LOG` and the `/logs` ring is fixed at `info`; neither is a knob.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = OtelOverrides)]
pub struct OtelSettings {
    /// Which events the OTel logs layer exports (Loki).
    pub logs_filter: String,
    /// Which spans the OTel traces layer exports (Tempo).
    pub traces_filter: String,
}

/// Our crates at `debug`, dependencies at `warn`. Targets match by prefix,
/// so `twitch_1337` alone would already cover `twitch_1337_core` and
/// `twitch_1337_web`; they are listed so an edit to one is obvious.
pub const DEFAULT_LOGS_FILTER: &str =
    "warn,twitch_1337=debug,twitch_1337_core=debug,twitch_1337_web=debug,llm=debug";

/// Our crates at `info` (every span is `info`, docs/observability.md § Spans
/// rule 4); dependencies only at `warn`, which in practice exports none of
/// their spans.
pub const DEFAULT_TRACES_FILTER: &str =
    "warn,twitch_1337=info,twitch_1337_core=info,twitch_1337_web=info,llm=info";

impl Default for OtelSettings {
    fn default() -> Self {
        Self {
            logs_filter: DEFAULT_LOGS_FILTER.to_owned(),
            traces_filter: DEFAULT_TRACES_FILTER.to_owned(),
        }
    }
}
