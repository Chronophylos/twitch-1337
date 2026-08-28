//! Prompt composition: nonce-fenced inject of every memory + scratch file body,
//! plus prompt-file substitution. Split by responsibility:
//! `fence` (FILE fence rendering), `substitute` (template vars + baked prompts),
//! `recent_chat` (rolling chat rendering), `aurora` (Aurora turn context),
//! `grok` (grok paths-only context), `messages` (system/user message assembly).

mod aurora;
mod fence;
mod grok;
mod messages;
mod recent_chat;
mod substitute;

pub use aurora::{BuildOpts, ChatTurnContext, InvocationChannel, build_chat_turn_context};
pub use fence::{FenceLabel, body_with_links, fence_block, scrub_for_inject};
pub use grok::{GrokTurnContext, ManifestOpts, build_grok_turn_context};
pub use messages::{SystemParts, UserParts, build_chat_turn_messages, render_recap_section};
pub(crate) use recent_chat::format_entry_line;
pub(crate) use substitute::{
    PROMPT_DREAMER, PROMPT_GROK, PROMPT_INSTRUCTIONS, PROMPT_SYSTEM, SubstitutionVars, fresh_nonce,
    substitute,
};

#[cfg(test)]
mod tests;
