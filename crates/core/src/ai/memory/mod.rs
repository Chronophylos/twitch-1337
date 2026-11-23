pub mod frontmatter;
pub mod inject;
pub mod ritual;
pub mod sanitize;
pub mod store;
pub mod tools;
pub mod transcript;
pub mod types;

pub use ritual::{run_ritual, spawn_ritual};
pub use store::{MemoryStore, WriteError};
pub use tools::{
    ChatTurnExecutor, ChatTurnExecutorOpts, DreamerExecutor, DreamerExecutorOpts, chat_turn_tools,
    dreamer_tools, grok_chat_turn_tools,
};
pub use transcript::TranscriptWriter;
pub use types::{Caps, FileKind, Frontmatter, MemoryFile, Role};

/// Memory v2 bundle: store handle and transcript writer.
#[derive(Clone)]
pub struct AiMemoryV2 {
    pub store: MemoryStore,
    pub transcript: TranscriptWriter,
}

/// Construct the AI memory v2 bundle. `None` when `[ai]` is absent from
/// `config.toml`.
///
/// `store` is built once in main.rs (or by tests) and shared with the web
/// dashboard via [`crate::Services::memory_store`]. Sharing the same `Arc`-
/// backed store keeps the per-path mutex map coherent across the bot's
/// IRC handlers, the dreamer ritual, and the dashboard editor — two
/// distinct stores would silently race past each other's locks and break
/// byte-cap enforcement.
pub async fn build_ai_memory_v2(
    ai_present: bool,
    store: MemoryStore,
) -> eyre::Result<Option<AiMemoryV2>> {
    if !ai_present {
        return Ok(None);
    }

    let transcript = TranscriptWriter::open(store.memories_dir()).await?;
    Ok(Some(AiMemoryV2 { store, transcript }))
}
