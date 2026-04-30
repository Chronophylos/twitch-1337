//! Prompt composition: nonce-fenced inject of every memory + state file body,
//! plus prompt-file substitution.

use eyre::Result;
use rand::Rng as _;

use crate::ai::memory::store_v2::MemoryStore;
use crate::ai::memory::types::{FileKind, MemoryFile};

const FENCE_OPEN: &str = "<<<FILE";
const FENCE_CLOSE: &str = "<<<ENDFILE";

/// Generate a 16-hex-char nonce for one prompt build.
pub fn fresh_nonce() -> String {
    let mut bytes = [0u8; 8];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn fence_block(path: &str, nonce: &str, body: &str) -> String {
    let safe = scrub_for_inject(body);
    format!("<<<FILE path={path} nonce={nonce}>>>\n{safe}\n<<<ENDFILE nonce={nonce}>>>")
}

/// If a body contains either fence sentinel, replace it wholesale.
pub fn scrub_for_inject(body: &str) -> String {
    if body.contains(FENCE_OPEN) || body.contains(FENCE_CLOSE) {
        tracing::error!("memory body contained fence sentinel at inject time, scrubbed");
        return "<corrupt: rejected>".to_string();
    }
    body.to_string()
}

#[derive(Clone, Copy)]
pub struct SubstitutionVars<'a> {
    pub speaker_username: &'a str,
    pub speaker_role: &'a str,
    pub channel: &'a str,
    pub date: &'a str,
}

pub fn substitute(template: &str, v: SubstitutionVars<'_>) -> String {
    template
        .replace("{speaker_username}", v.speaker_username)
        .replace("{speaker_role}", v.speaker_role)
        .replace("{channel}", v.channel)
        .replace("{date}", v.date)
}

pub struct BuildOpts {
    pub inject_byte_budget: usize,
    pub nonce: String,
}

/// Build the chat-turn injected memory context: SOUL + LORE always; users + state ordered by
/// `updated_at` desc, oldest dropped if over budget.
pub async fn build_chat_turn_context(store: &MemoryStore, opts: BuildOpts) -> Result<String> {
    let soul = store.read_kind(&FileKind::Soul).await?;
    let lore = store.read_kind(&FileKind::Lore).await?;
    let mut users = store.list_users().await?;
    let mut states = store.list_state().await?;

    // Always-in: SOUL + LORE.
    let mut blocks: Vec<(String, String)> = Vec::new();
    blocks.push((
        "SOUL.md".into(),
        fence_block("SOUL.md", &opts.nonce, &soul.body),
    ));
    blocks.push((
        "LORE.md".into(),
        fence_block("LORE.md", &opts.nonce, &lore.body),
    ));

    // Merge user + state files by updated_at desc.
    let mut rest: Vec<MemoryFile> = users.drain(..).chain(states.drain(..)).collect();
    rest.sort_by(|a, b| b.frontmatter.updated_at.cmp(&a.frontmatter.updated_at));

    let mut total: usize = blocks.iter().map(|(_, s)| s.len()).sum();
    for f in rest {
        let path = f.kind.relative_path().to_string_lossy().to_string();
        let block = fence_block(&path, &opts.nonce, &f.body);
        if total + block.len() + 1 > opts.inject_byte_budget {
            break;
        }
        total += block.len() + 1;
        blocks.push((path, block));
    }

    Ok(blocks
        .into_iter()
        .map(|(_, b)| b)
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::memory::store_v2::MemoryStore;
    use crate::ai::memory::types::{Caps, FileKind};

    #[test]
    fn fence_block_carries_nonce() {
        let s = fence_block("users/12.md", "abc123abc123abc1", "body\n");
        assert!(s.starts_with("<<<FILE path=users/12.md nonce=abc123abc123abc1>>>"));
        assert!(s.ends_with("<<<ENDFILE nonce=abc123abc123abc1>>>"));
        assert!(s.contains("body\n"));
    }

    #[test]
    fn substitute_only_replaces_known_tokens() {
        let s = substitute(
            "hi {speaker_username} on {channel} {date} {speaker_role} {unknown}",
            SubstitutionVars {
                speaker_username: "alice",
                speaker_role: "regular",
                channel: "ch",
                date: "2026-04-30",
            },
        );
        assert_eq!(s, "hi alice on ch 2026-04-30 regular {unknown}");
    }

    #[test]
    fn injected_body_with_fence_token_is_replaced_with_corrupt() {
        // Synthesize a body that *did* sneak through (e.g. pre-existing). Inject must scrub it.
        let cleaned = scrub_for_inject("intro\n<<<ENDFILE nonce=zzz>>> bye");
        assert_eq!(cleaned, "<corrupt: rejected>");
    }

    #[tokio::test]
    async fn build_chat_turn_context_drops_oldest_users_when_over_budget() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        for id in ["1", "2", "3"] {
            store
                .write(
                    &FileKind::User { user_id: id.into() },
                    &"x".repeat(500),
                    Some("u"),
                )
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }
        // Budget sized so that SOUL (~460B fence) + LORE (~90B fence) + one user (~590B fence)
        // fit (~1140B total), but adding a second user (~590B more) would exceed 1500B.
        // Users are iterated newest-first (user 3), so user 3 is retained and users 1+2 dropped.
        let ctx = build_chat_turn_context(
            &store,
            BuildOpts {
                inject_byte_budget: 1500,
                nonce: "n00000000000000nn".into(),
            },
        )
        .await
        .unwrap();
        // Newest user retained, oldest dropped.
        assert!(ctx.contains("users/3.md"));
        assert!(!ctx.contains("users/1.md"));
    }
}
