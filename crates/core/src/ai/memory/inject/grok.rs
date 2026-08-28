//! Grok paths-only memory context: rolling chat plus a manifest of readable
//! paths, no injected bodies.

use std::collections::BTreeSet;

use eyre::Result;

use crate::ai::memory::store::MemoryStore;
use crate::ai::memory::types::FileKind;

use super::aurora::BuildOpts;
use super::recent_chat::render_recent_chat;

/// Result of [`build_grok_turn_context`].
///
/// The grok agent reads memory on request instead of having it injected, so
/// `manifest` carries paths only and the bodies stay on disk until a
/// `read_memory` call pulls one. Two consequences worth knowing:
/// - the system message has no memory block, making grok's cache prefix
///   smaller and more stable than Aurora's;
/// - `readable_paths` is the authorization list for `read_memory`. A path not
///   in it does not exist as far as the model is concerned.
pub struct GrokTurnContext {
    pub recent_chat: String,
    pub manifest: String,
    pub readable_paths: BTreeSet<String>,
}

pub struct ManifestOpts {
    pub build: BuildOpts,
    /// Numeric Twitch id of the invoking user.
    pub speaker_user_id: String,
    /// Numeric Twitch id of the user whose message was replied to, when the
    /// invocation is a reply.
    pub reply_target_user_id: Option<String>,
}

/// Build the grok chat-turn context: rolling chat plus a paths-only memory
/// index. SOUL is deliberately absent — the second agent has no business
/// reading Aurora's self-description.
pub async fn build_grok_turn_context(
    store: &MemoryStore,
    opts: ManifestOpts,
) -> Result<GrokTurnContext> {
    let recent = render_recent_chat(&opts.build).await;

    let mut entries: Vec<String> = Vec::new();
    let mut readable: BTreeSet<String> = BTreeSet::new();

    entries.push("- LORE.md".to_string());
    readable.insert("LORE.md".to_string());

    // Scope to the people this turn is actually about: the speaker and, on a
    // reply invocation, whoever is being checked.
    let mut wanted: BTreeSet<&str> = BTreeSet::from([opts.speaker_user_id.as_str()]);
    if let Some(id) = opts.reply_target_user_id.as_deref() {
        wanted.insert(id);
    }

    let users = store.list_users().await?;
    for f in &users {
        let FileKind::User { user_id } = &f.kind else {
            continue;
        };
        if !wanted.contains(user_id.as_str()) {
            continue;
        }
        let path = format!("users/{user_id}.md");
        match f.frontmatter.username.as_deref() {
            Some(login) if !login.is_empty() => entries.push(format!("- {path} ({login})")),
            _ => entries.push(format!("- {path}")),
        }
        readable.insert(path);
    }

    let scratch = store.list_scratch().await?;
    for f in &scratch {
        let FileKind::Scratch { slug } = &f.kind else {
            continue;
        };
        let path = format!("scratch/{slug}.md");
        entries.push(format!("- {path}"));
        readable.insert(path);
    }

    let manifest = format!(
        "## Memory index\n\nThese files exist. Bodies are not included. Call read_memory(path) with one of these exact paths to pull a body. Nothing outside this list is readable.\n\n{}",
        entries.join("\n")
    );

    Ok(GrokTurnContext {
        recent_chat: recent.body,
        manifest,
        readable_paths: readable,
    })
}
