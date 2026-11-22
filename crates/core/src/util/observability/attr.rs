//! Attribute names for span attributes, log fields and metric labels
//! (docs/observability.md § Attribute names). OTel semantic conventions
//! first, re-exported from `opentelemetry-semantic-conventions`; project
//! names for the rest, using `CONTEXT.md` terms.
//!
//! `tracing` field syntax needs literals, so spans and log lines spell these
//! names out and the docs table keeps them consistent; the constants serve
//! metric labels and exporter mapping. "Label" below marks a bounded set that
//! may be a metric label; the others are span and log fields only.

pub use opentelemetry_semantic_conventions::attribute::{
    ERROR_TYPE, HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE, HTTP_ROUTE, SERVER_ADDRESS,
    URL_FULL,
};

// GenAI semconv names. `opentelemetry-semantic-conventions` 0.33 deprecates
// its `GEN_AI_*` constants (the GenAI conventions moved to their own repo),
// so the same names are spelled out here.

/// AI provider. Label.
pub const GEN_AI_PROVIDER_NAME: &str = "gen_ai.provider.name";
/// Upstream model id. Label.
pub const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
/// `chat`, `execute_tool`, …. Label.
pub const GEN_AI_OPERATION_NAME: &str = "gen_ai.operation.name";
/// Tool called by the model. Label.
pub const GEN_AI_TOOL_NAME: &str = "gen_ai.tool.name";
/// Prompt token count. A value, never a label.
pub const GEN_AI_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
/// Completion token count. A value, never a label.
pub const GEN_AI_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
/// Token direction of an `llm.tokens` count: `input` or `output`. Label.
pub const GEN_AI_TOKEN_TYPE: &str = "gen_ai.token.type";

/// Channel login. Label.
pub const TWITCH_CHANNEL: &str = "twitch.channel";
/// Chatter's Twitch user id. Never a label.
pub const TWITCH_USER_ID: &str = "twitch.user.id";
/// Chatter's Twitch login. Never a label.
pub const TWITCH_USER_LOGIN: &str = "twitch.user.login";
/// IRC message id. Never a label.
pub const TWITCH_MESSAGE_ID: &str = "twitch.message.id";

/// Broadcast consumer that handled the message. Label.
pub const BOT_HANDLER: &str = "bot.handler";
/// Canonical command name, not the alias typed. Label.
pub const BOT_COMMAND_NAME: &str = "bot.command.name";
/// Schedule name. Label.
pub const BOT_SCHEDULE_NAME: &str = "bot.schedule.name";
/// Ping name. Label.
pub const PING_NAME: &str = "ping.name";

/// Agent, or the feature calling the LLM outside one: `aurora`, `grok`,
/// `dreamer`, `recap`, `haiku`, `news`, `tldr`, `spoiler`, `profile_test`;
/// `unknown` outside `metrics::with_llm_scope`. Label.
pub const AI_AGENT: &str = "ai.agent";
/// AI profile name; `unknown` outside `metrics::with_llm_scope`. Label.
pub const AI_PROFILE: &str = "ai.profile";
/// Turn session nonce (the Langfuse session). Never a label.
pub const AI_TURN_SESSION_ID: &str = "ai.turn.session_id";
/// How a Turn ended: `reply`, `notice`, `silent`. Label.
pub const AI_TURN_OUTCOME: &str = "ai.turn.outcome";
/// Memory file kind: `soul`, `lore`, `character_sheet`, `scratch`. Label.
pub const MEMORY_KIND: &str = "memory.kind";

/// Tracked Flight callsign. Never a label.
pub const AVIATION_FLIGHT_CALLSIGN: &str = "aviation.flight.callsign";
/// Flight Phase. Label.
pub const AVIATION_FLIGHT_PHASE: &str = "aviation.flight.phase";
/// ADS-B or Aviationstack source. Label.
pub const AVIATION_PROVIDER: &str = "aviation.provider";

/// Alert feed: `quake`, `solar`, `cme`, `f1`. Label.
pub const ALERTS_FEED: &str = "alerts.feed";
