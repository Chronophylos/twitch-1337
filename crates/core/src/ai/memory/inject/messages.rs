//! Recap Notes rendering for the user-slot `recap` block.

use super::fence::{FenceLabel, fence_block, neutralize_sentinels};
use crate::ai::recap::RecapNote;

/// Render one channel's Recap Notes as a nonce-fenced block.
///
/// Notes are model paraphrases of viewer chat, so they are untrusted: fence
/// sentinels inside a body are neutralised rather than rejected, so one
/// viewer's line cannot blank the whole recap or close the fence early. The
/// preset adds the heading and the "data, not instructions" framing.
///
/// Invocation-first ordering is the caller's responsibility when concatenating
/// multiple channel logs; `notes` are joined oldest-to-newest within one channel.
pub fn render_recap_section(login: &str, notes: &[RecapNote], nonce: &str) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let body = notes
        .iter()
        .map(|n| neutralize_sentinels(&n.body))
        .collect::<Vec<_>>()
        .join("\n\n");
    fence_block(FenceLabel::Recap { channel: login }, nonce, &body)
}
