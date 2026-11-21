//! Token usage of one finished completion, and the process-wide hook that
//! receives it. `core` installs the hook to count `llm.tokens` / `llm.cost`;
//! the `chat` span carries the same counts.

use std::sync::OnceLock;

/// Token counts plus optional billed USD from the provider response.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// OpenRouter `usage.cost` (USD credits). `None` for providers that
    /// only return token counts.
    pub cost_usd: Option<f64>,
}

static HOOK: OnceLock<fn(&TokenUsage)> = OnceLock::new();

/// Install the hook every completion with reported usage calls, on the task
/// that made the call (so task-locals of the caller are visible). The first
/// install wins; returns `false` when a hook was already set.
pub fn set_usage_hook(hook: fn(&TokenUsage)) -> bool {
    HOOK.set(hook).is_ok()
}

/// Hand `usage` to the installed hook, if any.
pub(crate) fn report(usage: &TokenUsage) {
    if let Some(hook) = HOOK.get() {
        hook(usage);
    }
}
