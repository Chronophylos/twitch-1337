//! System/user message assembly from stable and volatile prompt parts.

use llm::Message;

use crate::ai::recap::RecapNote;

/// Stable parts of the chat-turn system message. For a given config + memory
/// snapshot these are byte-identical across turns, which is what lets Gemini's
/// implicit prompt cache hit. Volatile per-turn content is forbidden here by
/// type — it can only reach [`UserParts`].
pub struct SystemParts {
    /// Substituted `system.md`. After the `{speaker_role}` drop it interpolates
    /// only `{channel}`, so it is identical for every speaker.
    pub head: String,
    /// Stable per-mode appendices, appended to `head` in order. Each carries
    /// its own leading separator (the consts are `\n\n…`-prefixed): Konten/
    /// Accounts (mouth mapping), then emote-tools, packages/shipment, grok/web.
    pub appendices: Vec<String>,
    /// SOUL/LORE/user fences from [`build_chat_turn_context`]. Stable while the
    /// memory content and the (per-process) nonce are unchanged.
    pub memory: String,
    /// Recap Notes suffix. Appended after `memory` when non-empty; omitted
    /// entirely when empty so the cache prefix stays stable across turns with
    /// no recap.
    pub recap: String,
}

/// Per-turn (volatile) parts. Every field here may change turn to turn; all of
/// it lands in the user message so it never disturbs the cached system prefix.
pub struct UserParts {
    /// Relevant-emotes block for this turn (`prompt_block_for_turn`). `None`
    /// when the emote provider is inactive or produced nothing.
    pub emote_block: Option<String>,
    /// `scratch/<slug>` fences from [`build_chat_turn_context`].
    pub scratch: String,
    /// Rolling chat history for this turn.
    pub recent_chat: String,
    /// Substituted `ai_instructions.md` (carries speaker vars + date).
    pub instructions: String,
    /// The user's instruction, already augmented with any reply context.
    pub instruction: String,
}

/// Render a Recap Notes section for injection into the system message suffix.
///
/// Invocation-first ordering is the caller's responsibility when concatenating
/// multiple channel logs; `notes` are joined oldest-to-newest within one channel.
pub fn render_recap_section(login: &str, notes: &[RecapNote]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut s = format!("## Recap (#{login})\n");
    s.push_str(
        &notes
            .iter()
            .map(|n| n.body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    );
    s
}

/// Assemble the `!ai` chat-turn messages from stable and volatile parts.
///
/// `system` = `head` + each appendix + `"\n\n"` + `memory` + optional `"\n\n"` + `recap`.
/// `user`   = `emote_block?` + `scratch?` + `recent_chat?` + `instructions` + `instruction`.
///
/// Keeping every volatile section in the user message is what makes the system
/// message a stable cache prefix; see `docs/ai-prompts.md`.
pub fn build_chat_turn_messages(sys: SystemParts, user: UserParts) -> (Message, Message) {
    let mut system = sys.head;
    for appendix in &sys.appendices {
        system.push_str(appendix);
    }
    system.push_str("\n\n");
    system.push_str(&sys.memory);
    if !sys.recap.is_empty() {
        system.push_str("\n\n");
        system.push_str(&sys.recap);
    }

    let mut user_message = String::new();
    if let Some(block) = user.emote_block.as_deref() {
        user_message.push_str(block.trim_start());
        user_message.push_str("\n\n");
    }
    if !user.scratch.is_empty() {
        user_message.push_str(&user.scratch);
        user_message.push_str("\n\n");
    }
    if !user.recent_chat.is_empty() {
        user_message.push_str(&user.recent_chat);
        user_message.push_str("\n\n");
    }
    user_message.push_str(&user.instructions);
    user_message.push_str("\n\n");
    user_message.push_str(&user.instruction);

    (Message::system(system), Message::user(user_message))
}
