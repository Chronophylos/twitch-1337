//! Prompt composition: nonce-fenced inject of every memory + scratch file body,
//! split by responsibility:
//! `fence` (FILE fence rendering + nonce),
//! `recent_chat` (rolling chat rendering), `aurora` (Aurora turn context),
//! `grok` (grok paths-only context), `messages` (recap suffix rendering).

mod aurora;
mod fence;
mod grok;
mod messages;
mod recent_chat;

pub use aurora::{BuildOpts, ChatTurnContext, InvocationChannel, build_chat_turn_context};
pub use fence::{FenceLabel, body_with_links, fence_block, fresh_nonce, scrub_for_inject};
pub use grok::{GrokTurnContext, ManifestOpts, build_grok_turn_context};
pub use messages::render_recap_section;
pub(crate) use recent_chat::format_entry_line;

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "test fixtures seed files directly"
)]
mod tests;
