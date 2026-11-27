//! Aurora chat-turn context: recent chat + nonce-fenced memory/scratch blocks.

use std::collections::BTreeSet;
use std::sync::Arc;

use eyre::Result;
use tokio::sync::Mutex;

use crate::ai::chat_history::ChatHistoryBuffer;
use crate::ai::memory::store::MemoryStore;
use crate::ai::memory::types::FileKind;
use crate::twitch::mouth::Agent;

use super::fence::{FenceLabel, body_with_links, fence_block};
use super::recent_chat::{render_recent_chat, user_pack_tier};

pub struct BuildOpts {
    pub inject_byte_budget: usize,
    pub nonce: String,
    pub primary_history: Option<Arc<Mutex<ChatHistoryBuffer>>>,
    pub primary_login: String,
    /// Which agent is speaking this turn. Drives the `(self)` tag in rendered
    /// recent-chat lines: a line is self iff `entry.agent == Some(current_agent)`.
    pub current_agent: Agent,
    /// Lowercased logins the bot speaks under: the utility account plus any
    /// configured mouths. Only consulted for entries with `agent: None`, which
    /// is what `ChatHistoryBuffer::from_prefill` produces — recovered history
    /// carries no agent tag, so without this the bot reads its own past lines
    /// back as a stranger's for the whole prefill window after a restart.
    pub self_logins: Vec<String>,
    /// Lowercased Twitch login of the user that triggered this turn (i.e.
    /// `!ai` invoker). Empty string for the dreamer ritual (no speaker).
    /// Used by [`build_chat_turn_context`] to scope the injected user-file
    /// set down to chat-window users plus the speaker and recap identities.
    pub speaker_login: String,
    /// Lowercased logins stamped on injected Recap Notes (caller-capped).
    /// Third packing tier after speaker and chat-window logins.
    pub recap_sheet_logins: BTreeSet<String>,
    /// Verbatim tail: newest N lines that are not this agent's own.
    pub n: usize,
    /// When Recap is on, eligible lag lines may stretch the tail up to
    /// `n + batch_size` other-people lines (never adds extra own lines).
    pub batch_size: usize,
    /// Recap watermark seq; `0` when Recap is off.
    pub recap_through_seq: u64,
    /// When false, no stretch beyond the N-not-own window.
    pub recap_enabled: bool,
}

/// Result of [`build_chat_turn_context`].
///
/// The three fields exist so callers can route blocks to whichever message
/// maximises prompt-cache hits:
/// - `memory` holds SOUL/LORE/users (changes on dreamer runs only).
///   Lives in the system message so consecutive turns hit the prompt cache.
/// - `scratch` holds the `scratch/<slug>` blocks (every `write_scratch` /
///   `delete_scratch` mutates them). Lives in the user message so it can change
///   freely without invalidating the system-message cache.
/// - `recent_chat` holds the rolling per-turn chat history (volatile by
///   definition). Lives in the user message.
///
/// Any field may be empty. Callers that want the legacy combined memory blob
/// (dreamer) concatenate `memory` + `scratch` themselves.
pub struct ChatTurnContext {
    pub recent_chat: String,
    pub memory: String,
    pub scratch: String,
    /// Ids of the user sheets whose body made it into `memory`.
    pub packed_user_ids: BTreeSet<String>,
}

/// Build the chat-turn injected context split into a recent-chat section and a
/// memory section. Callers place each in the message that maximizes prompt
/// caching: memory in the system message, recent chat in the user message.
pub async fn build_chat_turn_context(
    store: &MemoryStore,
    opts: BuildOpts,
) -> Result<ChatTurnContext> {
    let recent = render_recent_chat(&opts).await;
    let mentioned = recent.mentioned;

    let soul = store.read_kind(&FileKind::Soul).await?;
    let lore = store.read_kind(&FileKind::Lore).await?;
    let mut users = store.list_users().await?;
    let mut states = store.list_scratch().await?;

    let mut memory_blocks: Vec<String> = Vec::new();
    memory_blocks.push(fence_block(FenceLabel::Soul, &opts.nonce, &soul.body));
    memory_blocks.push(fence_block(FenceLabel::Lore, &opts.nonce, &lore.body));

    // Scope user files to logins present in the chat window plus the speaker.
    // Users who didn't appear in either are excluded outright: the model only
    // needs character sheets for people it's actually about to talk to or
    // about. Without this filter, lurkers with recent `updated_at` would
    // crowd out the speaker's own file when the budget is tight.
    //
    // Short-circuit for the dreamer: no history buffers AND no speaker means
    // this isn't a chat turn, it's the dreamer ritual, which wants every user
    // file in its system prompt. Skipping the filter in that case keeps the
    // dreamer's pre-existing "every memory file in one shot" contract intact.
    let speaker_lc = opts.speaker_login.to_ascii_lowercase();
    let is_chat_turn = opts.primary_history.is_some() || !speaker_lc.is_empty();
    if is_chat_turn {
        let mut scope: std::collections::BTreeSet<String> = mentioned.clone();
        if !speaker_lc.is_empty() {
            scope.insert(speaker_lc.clone());
        }
        for login in &opts.recap_sheet_logins {
            scope.insert(login.to_ascii_lowercase());
        }
        users.retain(|f| {
            let Some(login) = f.frontmatter.username.as_deref() else {
                return false;
            };
            scope.contains(&login.to_ascii_lowercase())
        });
    }

    // Pack speaker → chat-window logins → recap-only identities, newest-first
    // within each tier. Ensures the speaker's file always survives the
    // byte-budget packing loop below. When speaker_login is empty (dreamer),
    // tier 0 is unused and this collapses to mentioned → recap → updated_at.
    let recap_lc = &opts.recap_sheet_logins;
    users.sort_by(|a, b| {
        let a_login = a
            .frontmatter
            .username
            .as_deref()
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let b_login = b
            .frontmatter
            .username
            .as_deref()
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let a_tier = user_pack_tier(&a_login, &speaker_lc, &mentioned, recap_lc);
        let b_tier = user_pack_tier(&b_login, &speaker_lc, &mentioned, recap_lc);
        a_tier
            .cmp(&b_tier)
            .then_with(|| b.frontmatter.updated_at.cmp(&a.frontmatter.updated_at))
    });
    states.sort_by_key(|f| std::cmp::Reverse(f.frontmatter.updated_at));

    // Pack user blocks until the memory budget is hit. Scratch blocks get
    // whatever budget is left over so heavy LORE/user nights don't crowd them
    // out entirely; remaining scratch still pops as oldest-first drops.
    let mut total: usize = memory_blocks.iter().map(String::len).sum();
    let mut packed_user_ids = BTreeSet::new();
    for f in users.drain(..) {
        let FileKind::User { user_id } = &f.kind else {
            tracing::error!(?f.kind, "non-user file in user list, skipping");
            continue;
        };
        let label = FenceLabel::User {
            id: user_id,
            login: f.frontmatter.username.as_deref(),
            display_name: f.frontmatter.display_name.as_deref(),
            pronouns: f.frontmatter.pronouns.as_deref(),
        };
        let block = fence_block(
            label,
            &opts.nonce,
            &body_with_links(&f.frontmatter, &f.body),
        );
        if total + block.len() + 1 > opts.inject_byte_budget {
            break;
        }
        total += block.len() + 1;
        memory_blocks.push(block);
        packed_user_ids.insert(user_id.clone());
    }

    let mut scratch_blocks: Vec<String> = Vec::new();
    for f in states.drain(..) {
        let FileKind::Scratch { slug } = &f.kind else {
            tracing::error!(?f.kind, "non-scratch file in scratch list, skipping");
            continue;
        };
        let block = fence_block(FenceLabel::Scratch { slug }, &opts.nonce, &f.body);
        if total + block.len() + 1 > opts.inject_byte_budget {
            break;
        }
        total += block.len() + 1;
        scratch_blocks.push(block);
    }

    let memory = memory_blocks.join("\n");
    let scratch = scratch_blocks.join("\n");
    let recent_chat = recent.body;
    Ok(ChatTurnContext {
        recent_chat,
        memory,
        scratch,
        packed_user_ids,
    })
}
