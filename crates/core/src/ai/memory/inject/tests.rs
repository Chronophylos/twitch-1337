use std::collections::BTreeSet;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::*;
use crate::ai::chat_history::{ChatHistoryBuffer, ChatHistoryEntry, ChatHistorySource};
use crate::ai::memory::store::{MemoryStore, UserIdentity};
use crate::ai::memory::types::FileKind;
use crate::settings::test_handle;
use crate::twitch::mouth::Agent;

#[test]
fn format_entry_line_self_uses_persona_and_self_tag() {
    let entry = ChatHistoryEntry {
        seq: 1,
        username: "chronophylosbot".into(),
        display_name: Some("Aurora".into()),
        user_id: None,
        text: "gemerkt".into(),
        source: ChatHistorySource::Bot,
        agent: Some(Agent::Aurora),
        timestamp: jiff::Timestamp::now(),
    };
    let line = format_entry_line(&entry, Agent::Aurora, &[]);
    assert!(line.ends_with(" Aurora (self): gemerkt"), "got: {line}");
}

#[test]
fn format_entry_line_other_falls_back_to_username_when_no_display() {
    let entry = ChatHistoryEntry {
        seq: 1,
        username: "lurker42".into(),
        display_name: None,
        user_id: None,
        text: "?".into(),
        source: ChatHistorySource::User,
        agent: None,
        timestamp: jiff::Timestamp::now(),
    };
    let line = format_entry_line(&entry, Agent::Aurora, &[]);
    assert!(line.ends_with(" lurker42: ?"), "got: {line}");
}

#[test]
fn grok_line_on_aurora_turn_is_not_self() {
    use crate::twitch::mouth::Agent;
    let ts = jiff::Timestamp::now();
    let grok_entry = ChatHistoryEntry {
        seq: 1,
        username: "grok_bot".into(),
        display_name: Some("Grok".into()),
        user_id: None,
        text: "nuh uh".into(),
        source: ChatHistorySource::Bot,
        agent: Some(Agent::Grok),
        timestamp: ts,
    };
    let line = format_entry_line(&grok_entry, Agent::Aurora, &[]);
    assert!(line.ends_with(" Grok: nuh uh"), "got: {line}");

    let aurora_entry = ChatHistoryEntry {
        agent: Some(Agent::Aurora),
        ..grok_entry.clone()
    };
    let line = format_entry_line(&aurora_entry, Agent::Aurora, &[]);
    assert!(line.contains(" Grok (self): "), "got: {line}");
}

/// Prefilled history carries `agent: None` (the speaking agent was never
/// recorded), so after a restart the bot would read its own past lines back
/// as a stranger's for the whole prefill window without the login fallback.
#[test]
fn format_entry_line_untagged_bot_login_is_self() {
    let ts = jiff::Timestamp::now();
    let prefilled = ChatHistoryEntry {
        seq: 1,
        username: "GrokBot".into(),
        display_name: None,
        user_id: None,
        text: "nuh uh".into(),
        source: ChatHistorySource::User,
        agent: None,
        timestamp: ts,
    };
    let self_logins = vec!["chronophylosbot".to_string(), "grokbot".to_string()];

    // Case-insensitive, and it does not matter which agent is speaking now:
    // the tag is only about whether the bot said it.
    let line = format_entry_line(&prefilled, Agent::Aurora, &self_logins);
    assert!(line.contains(" GrokBot (self): "), "got: {line}");
    let line = format_entry_line(&prefilled, Agent::Grok, &self_logins);
    assert!(line.contains(" GrokBot (self): "), "got: {line}");

    // A real viewer stays a third party.
    let viewer = ChatHistoryEntry {
        username: "someviewer".into(),
        ..prefilled.clone()
    };
    let line = format_entry_line(&viewer, Agent::Aurora, &self_logins);
    assert!(line.ends_with(" someviewer: nuh uh"), "got: {line}");

    // An empty list is the pre-mouths behavior: nothing is self.
    let line = format_entry_line(&prefilled, Agent::Aurora, &[]);
    assert!(line.ends_with(" GrokBot: nuh uh"), "got: {line}");
}

async fn test_store() -> (MemoryStore, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(tmp.path(), test_handle()).await.unwrap();
    (store, tmp)
}

fn history_buffer() -> Arc<Mutex<ChatHistoryBuffer>> {
    Arc::new(Mutex::new(ChatHistoryBuffer::new(test_handle())))
}

/// Baseline options: no history, no speaker, 16 KiB budget, Recap off.
/// Tests override only the fields they exercise.
fn build_opts() -> BuildOpts {
    BuildOpts {
        inject_byte_budget: 16 * 1024,
        nonce: "n00000000000000nn".into(),
        primary_history: None,
        primary_login: "chan".into(),
        current_agent: Agent::Aurora,
        self_logins: Vec::new(),
        speaker_login: String::new(),
        recap_sheet_logins: BTreeSet::new(),
        n: 200,
        batch_size: 40,
        recap_through_seq: 0,
        recap_enabled: false,
    }
}

/// Shared scenario: five-line history where aurora's older self-line is
/// superseded; returns the built context for the speaker "carol".
async fn ctx_with_history(recap_enabled: bool) -> ChatTurnContext {
    let (store, _tmp) = test_store().await;
    let buf = history_buffer();
    {
        let mut h = buf.lock().await;
        h.push_user("alice", "hi");
        h.push_bot_with_identity("aurora", Some("Aurora"), Agent::Aurora, "old joke");
        h.push_user("bob", "yo");
        h.push_bot_with_identity("aurora", Some("Aurora"), Agent::Aurora, "new joke");
        h.push_user("carol", "ok");
    }
    build_chat_turn_context(
        &store,
        BuildOpts {
            primary_history: Some(buf),
            self_logins: vec!["aurora".into()],
            speaker_login: "carol".into(),
            n: 2,
            recap_enabled,
            ..build_opts()
        },
    )
    .await
    .unwrap()
}

/// Holds with Recap off (plain tail) and on (stretch must skip own lines too).
#[tokio::test]
async fn tail_keeps_only_latest_own_line() {
    for recap_enabled in [false, true] {
        let ctx = ctx_with_history(recap_enabled).await;
        assert!(ctx.recent_chat.contains("new joke"));
        assert!(!ctx.recent_chat.contains("old joke"));
        let self_hits = ctx.recent_chat.matches("(self)").count();
        assert_eq!(self_hits, 1, "recap_enabled={recap_enabled}");
    }
}

#[tokio::test]
async fn grok_tail_keeps_grok_not_aurora() {
    let (store, _tmp) = test_store().await;
    let buf = history_buffer();
    {
        let mut h = buf.lock().await;
        h.push_user("alice", "hi");
        h.push_bot_with_identity("aurora", Some("Aurora"), Agent::Aurora, "aurora line");
        h.push_user("bob", "yo");
        h.push_bot_with_identity("grok", Some("Grok"), Agent::Grok, "grok line");
        h.push_user("carol", "ok");
    }
    let ctx = build_chat_turn_context(
        &store,
        BuildOpts {
            primary_history: Some(buf),
            current_agent: Agent::Grok,
            self_logins: vec!["aurora".into(), "grok".into()],
            speaker_login: "carol".into(),
            n: 2,
            ..build_opts()
        },
    )
    .await
    .unwrap();
    assert!(ctx.recent_chat.contains("grok line"));
    let self_hits = ctx.recent_chat.matches("(self)").count();
    assert_eq!(self_hits, 1);
    assert!(ctx.recent_chat.contains("Grok (self): grok line"));
    assert!(!ctx.recent_chat.contains("Aurora (self):"));
    if ctx.recent_chat.contains("aurora line") {
        assert!(
            ctx.recent_chat.contains("Aurora: aurora line"),
            "aurora line must not be tagged (self): {}",
            ctx.recent_chat
        );
    }
}

#[tokio::test]
async fn stretch_caps_at_n_plus_batch_size() {
    let (store, _tmp) = test_store().await;
    let buf = history_buffer();
    {
        let mut h = buf.lock().await;
        for i in 0..20 {
            h.push_user(format!("viewer{i}"), format!("line{i}"));
        }
    }
    let ctx = build_chat_turn_context(
        &store,
        BuildOpts {
            primary_history: Some(buf),
            speaker_login: "viewer19".into(),
            n: 2,
            batch_size: 3,
            recap_enabled: true,
            ..build_opts()
        },
    )
    .await
    .unwrap();
    let other_lines = ctx
        .recent_chat
        .lines()
        .filter(|line| !line.starts_with("##") && !line.contains("(self)"))
        .count();
    assert!(
        other_lines <= 5,
        "expected <= 5 other lines, got {other_lines}"
    );
}

#[tokio::test]
async fn recap_off_keeps_only_tail_and_latest_own_line() {
    let (store, _tmp) = test_store().await;
    let buf = history_buffer();
    {
        let mut h = buf.lock().await;
        h.push_user("old", "old line");
        h.push_user("alice", "tail one");
        h.push_user("bob", "tail two");
        h.push_bot_with_identity("aurora", Some("Aurora"), Agent::Aurora, "own line");
    }
    let ctx = build_chat_turn_context(
        &store,
        BuildOpts {
            primary_history: Some(buf),
            self_logins: vec!["aurora".into()],
            speaker_login: "bob".into(),
            n: 2,
            ..build_opts()
        },
    )
    .await
    .unwrap();
    assert!(!ctx.recent_chat.contains("old line"));
    assert!(ctx.recent_chat.contains("tail one"));
    assert!(ctx.recent_chat.contains("tail two"));
    assert!(ctx.recent_chat.contains("own line"));
}

#[tokio::test]
async fn grok_manifest_lists_paths_without_bodies() {
    let (store, _tmp) = test_store().await;
    store
        .write(&FileKind::Lore, "lore body text", UserIdentity::default())
        .await
        .unwrap();
    store
        .write(
            &FileKind::User {
                user_id: "111".into(),
            },
            "alice body text",
            UserIdentity::of("alice", "Alice"),
        )
        .await
        .unwrap();
    store
        .write_scratch(
            &FileKind::Scratch {
                slug: "quiz".into(),
            },
            "quiz body text",
            Some("111"),
        )
        .await
        .unwrap();

    let ctx = build_grok_turn_context(
        &store,
        ManifestOpts {
            build: BuildOpts {
                speaker_login: "alice".into(),
                ..build_opts()
            },
            speaker_user_id: "111".into(),
            reply_target_user_id: None,
        },
    )
    .await
    .unwrap();

    assert!(ctx.manifest.contains("LORE.md"));
    assert!(ctx.manifest.contains("users/111.md"));
    assert!(ctx.manifest.contains("scratch/quiz.md"));
    // Paths only. A body in the manifest defeats the whole design.
    assert!(!ctx.manifest.contains("lore body text"));
    assert!(!ctx.manifest.contains("alice body text"));
    assert!(!ctx.manifest.contains("quiz body text"));
    // No fences: nothing is injected, so nothing needs fencing.
    assert!(!ctx.manifest.contains("<<<FILE"));

    assert_eq!(
        ctx.readable_paths,
        ["LORE.md", "users/111.md", "scratch/quiz.md"]
            .into_iter()
            .map(String::from)
            .collect::<BTreeSet<_>>()
    );
}

#[tokio::test]
async fn grok_manifest_scopes_users_to_speaker_and_reply_target() {
    let (store, _tmp) = test_store().await;
    store
        .write(&FileKind::Lore, "lore", UserIdentity::default())
        .await
        .unwrap();
    for (id, login) in [("111", "alice"), ("222", "bob"), ("333", "carol")] {
        store
            .write(
                &FileKind::User { user_id: id.into() },
                "body",
                UserIdentity::of(login, login),
            )
            .await
            .unwrap();
    }

    let ctx = build_grok_turn_context(
        &store,
        ManifestOpts {
            build: BuildOpts {
                speaker_login: "alice".into(),
                ..build_opts()
            },
            speaker_user_id: "111".into(),
            reply_target_user_id: Some("222".into()),
        },
    )
    .await
    .unwrap();

    assert!(ctx.readable_paths.contains("users/111.md"));
    assert!(ctx.readable_paths.contains("users/222.md"));
    // carol is a bystander. A validator works the claim in front of it.
    assert!(!ctx.readable_paths.contains("users/333.md"));
    assert!(!ctx.manifest.contains("users/333.md"));
}

#[tokio::test]
async fn grok_manifest_never_lists_soul() {
    let (store, _tmp) = test_store().await;
    store
        .write(
            &FileKind::Soul,
            "aurora self description",
            UserIdentity::default(),
        )
        .await
        .unwrap();

    let ctx = build_grok_turn_context(
        &store,
        ManifestOpts {
            build: BuildOpts {
                speaker_login: "alice".into(),
                ..build_opts()
            },
            speaker_user_id: "111".into(),
            reply_target_user_id: None,
        },
    )
    .await
    .unwrap();

    assert!(!ctx.manifest.contains("SOUL.md"));
    assert!(!ctx.readable_paths.contains("SOUL.md"));
}

#[test]
fn fence_block_user_renders_identity_attrs() {
    let s = fence_block(
        FenceLabel::User {
            id: "12",
            login: Some("alicepleb"),
            display_name: Some("Alice Pleb"),
            pronouns: Some("They/Them"),
        },
        "abc123abc123abc1",
        "body\n",
    );
    assert!(s.starts_with(
            "<<<FILE kind=user id=12 login=alicepleb name=\"Alice Pleb\" pronouns=\"They/Them\" nonce=abc123abc123abc1>>>"
        ));
    assert!(s.ends_with("<<<ENDFILE nonce=abc123abc123abc1>>>"));
    assert!(s.contains("body\n"));
}

#[test]
fn fence_block_user_omits_missing_identity_attrs() {
    let s = fence_block(
        FenceLabel::User {
            id: "12",
            login: None,
            display_name: None,
            pronouns: None,
        },
        "n",
        "x",
    );
    assert!(s.starts_with("<<<FILE kind=user id=12 nonce=n>>>"));
}

#[test]
fn fence_block_soul_lore_state_transcript_use_kind_attrs() {
    assert!(fence_block(FenceLabel::Soul, "n", "x").starts_with("<<<FILE kind=soul nonce=n>>>"));
    assert!(fence_block(FenceLabel::Lore, "n", "x").starts_with("<<<FILE kind=lore nonce=n>>>"));
    assert!(
        fence_block(FenceLabel::Scratch { slug: "quiz" }, "n", "x")
            .starts_with("<<<FILE kind=scratch slug=quiz nonce=n>>>")
    );
    assert!(
        fence_block(FenceLabel::Transcript { date: "2026-05-05" }, "n", "x",)
            .starts_with("<<<FILE kind=transcript date=2026-05-05 nonce=n>>>")
    );
}

#[test]
fn fence_block_strips_quote_breakers_from_display_name() {
    let s = fence_block(
        FenceLabel::User {
            id: "1",
            login: Some("a"),
            display_name: Some("we\"ird>guy"),
            pronouns: None,
        },
        "n",
        "x",
    );
    assert!(s.contains("name=\"weirdguy\""), "got: {s}");
}

#[test]
fn injected_body_with_fence_token_is_replaced_with_corrupt() {
    // Synthesize a body that *did* sneak through (e.g. pre-existing). Inject must scrub it.
    let cleaned = scrub_for_inject("intro\n<<<ENDFILE nonce=zzz>>> bye");
    assert_eq!(cleaned, "<corrupt: rejected>");
}

#[test]
fn neutralized_transcript_never_carries_a_sentinel() {
    // Runs of `<` longer than three must not collapse back into `<<<FILE`.
    for raw in [
        "lol <<<FILE",
        "gg <<<ENDFILE nonce=x>>>",
        "<<<<FILE",
        "<<<<<<ENDFILE",
        "a<<<FILE<<<ENDFILE",
    ] {
        let out = neutralize_sentinels(raw);
        assert!(!out.contains("<<<"), "{raw:?} -> {out:?}");
        assert_ne!(scrub_for_inject(&out), "<corrupt: rejected>", "{raw:?}");
    }
    assert_eq!(
        neutralize_sentinels("12:00  a#1: hi <3 <<"),
        "12:00  a#1: hi <3 <<"
    );
}

#[tokio::test]
async fn build_chat_turn_context_routes_scratch_to_user_msg_and_users_to_memory() {
    let (store, _tmp) = test_store().await;
    store
        .write(
            &FileKind::User {
                user_id: "42".into(),
            },
            "alice body",
            UserIdentity::of("alice", "Alice"),
        )
        .await
        .unwrap();
    store
        .write_scratch(
            &FileKind::Scratch {
                slug: "quiz".into(),
            },
            "score: 3",
            Some("42"),
        )
        .await
        .unwrap();

    let ctx = build_chat_turn_context(
        &store,
        BuildOpts {
            inject_byte_budget: 24576,
            primary_login: "main".into(),
            ..build_opts()
        },
    )
    .await
    .unwrap();

    assert!(ctx.memory.contains("kind=soul"));
    assert!(ctx.memory.contains("kind=lore"));
    assert!(ctx.memory.contains("kind=user id=42"));
    assert!(
        !ctx.memory.contains("kind=scratch"),
        "scratch must not appear in memory: {}",
        ctx.memory
    );
    assert!(ctx.scratch.contains("kind=scratch slug=quiz"));
    assert!(!ctx.scratch.contains("kind=user"));
}

#[tokio::test]
async fn build_chat_turn_context_drops_oldest_users_when_over_budget() {
    let (store, _tmp) = test_store().await;
    for id in ["1", "2", "3"] {
        store
            .write(
                &FileKind::User { user_id: id.into() },
                &"x".repeat(500),
                UserIdentity::of("u", "U"),
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }
    // Budget sized so that SOUL (~460B fence) + LORE (~90B fence) + one user (~590B fence)
    // fit (~1140B total), but adding a second user (~590B more) would exceed 1500B.
    // Users are iterated newest-first (user 3), so user 3 is retained and users 1+2 dropped.
    // Empty speaker + None history: dreamer-style path, no chat-window scope
    // filter is applied, so the byte-budget ordering this test exercises
    // remains in force.
    let ctx = build_chat_turn_context(
        &store,
        BuildOpts {
            inject_byte_budget: 1500,
            primary_login: "main".into(),
            ..build_opts()
        },
    )
    .await
    .unwrap();
    // Newest user retained, oldest dropped.
    assert!(ctx.memory.contains("kind=user id=3"));
    assert!(!ctx.memory.contains("kind=user id=1"));
    assert!(ctx.recent_chat.is_empty());
}

#[tokio::test]
async fn build_chat_turn_context_renders_history_into_recent_chat_only() {
    let (store, _tmp) = test_store().await;

    let primary = history_buffer();
    primary.lock().await.push_user("alice", "hello primary");

    let body = build_chat_turn_context(
        &store,
        BuildOpts {
            inject_byte_budget: 24576,
            primary_history: Some(primary),
            primary_login: "main".into(),
            ..build_opts()
        },
    )
    .await
    .unwrap();

    assert!(body.recent_chat.contains("Recent chat (#main)"));
    assert!(body.recent_chat.contains("alice: hello primary"));
    assert!(!body.memory.contains("Recent chat"));
    assert!(!body.scratch.contains("Recent chat"));
}

/// Seed a user/<id>.md directly with a hand-rolled frontmatter so the test
/// can pin `updated_at` precisely. `MemoryStore::write` always stamps
/// `Timestamp::now()`, which is not enough control to verify that the newest
/// user (`lurker`) is dropped by the scope filter rather than by recency.
async fn seed_user_file(
    dir: &std::path::Path,
    id: &str,
    login: &str,
    display: &str,
    updated_at: jiff::Timestamp,
    body: &str,
) {
    let users_dir = dir.join("memories/users");
    tokio::fs::create_dir_all(&users_dir).await.unwrap();
    let raw = format!(
        "---\nupdated_at: {}\nusername: {login}\ndisplay_name: {display}\n---\n{body}",
        updated_at.strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
    );
    tokio::fs::write(users_dir.join(format!("{id}.md")), raw)
        .await
        .unwrap();
}

#[tokio::test]
async fn build_chat_turn_context_scopes_users_to_chat_window_plus_speaker() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();

    // Seed four user files. `lurker` has the newest `updated_at` and
    // would normally win the recency scan; we expect it to be excluded
    // because it's neither the speaker nor present in the chat window.
    let now = jiff::Timestamp::now();
    let backdate = jiff::SignedDuration::from_hours(7 * 24);
    for (id, login, display) in [
        ("11", "alice", "Alice"),
        ("22", "bob", "Bob"),
        ("33", "carol", "Carol"),
    ] {
        seed_user_file(dir.path(), id, login, display, now - backdate, "body").await;
    }
    seed_user_file(dir.path(), "99", "lurker", "Lurker", now, "body").await;

    let history = history_buffer();
    {
        let mut h = history.lock().await;
        h.push_user("alice", "hi");
        h.push_user("bob", "yo");
    }

    let ctx = build_chat_turn_context(
        &store,
        BuildOpts {
            primary_history: Some(history),
            speaker_login: "carol".into(),
            ..build_opts()
        },
    )
    .await
    .unwrap();

    assert!(
        ctx.memory.contains("login=alice"),
        "alice (chat-window) missing:\n{}",
        ctx.memory
    );
    assert!(
        ctx.memory.contains("login=bob"),
        "bob (chat-window) missing:\n{}",
        ctx.memory
    );
    assert!(
        ctx.memory.contains("login=carol"),
        "carol (speaker) missing:\n{}",
        ctx.memory
    );
    assert!(
        !ctx.memory.contains("login=lurker"),
        "lurker is neither speaker nor in chat window, must be excluded:\n{}",
        ctx.memory
    );

    // Speaker (carol) must appear before the chat-window users so the
    // byte-budget packing loop can never drop the speaker's own file.
    let carol_idx = ctx.memory.find("login=carol").unwrap();
    let alice_idx = ctx.memory.find("login=alice").unwrap();
    let bob_idx = ctx.memory.find("login=bob").unwrap();
    assert!(
        carol_idx < alice_idx && carol_idx < bob_idx,
        "speaker must come first:\n{}",
        ctx.memory
    );
}

#[tokio::test]
async fn note_only_lurker_loses_to_tail_when_budget_tight() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();

    let now = jiff::Timestamp::now();
    let backdate = jiff::SignedDuration::from_hours(7 * 24);
    seed_user_file(
        dir.path(),
        "11",
        "carol",
        "Carol",
        now - backdate,
        &"x".repeat(400),
    )
    .await;
    seed_user_file(
        dir.path(),
        "22",
        "alice",
        "Alice",
        now - backdate,
        &"x".repeat(400),
    )
    .await;
    // Newer than alice — without tier ordering lurker would win the second slot.
    seed_user_file(dir.path(), "99", "lurker", "Lurker", now, &"x".repeat(400)).await;

    let history = history_buffer();
    history.lock().await.push_user("alice", "hi");

    let recap_sheet_logins: BTreeSet<String> = ["lurker"].into_iter().map(str::to_string).collect();

    let ctx = build_chat_turn_context(
        &store,
        BuildOpts {
            // SOUL+LORE+carol+alice fit; lurker (third user) exceeds budget.
            inject_byte_budget: 1950,
            primary_history: Some(history),
            speaker_login: "carol".into(),
            recap_sheet_logins,
            ..build_opts()
        },
    )
    .await
    .unwrap();

    assert!(
        ctx.memory.contains("login=carol"),
        "speaker must survive:\n{}",
        ctx.memory
    );
    assert!(
        ctx.memory.contains("login=alice"),
        "tail login alice must beat recap-only lurker:\n{}",
        ctx.memory
    );
    assert!(
        !ctx.memory.contains("login=lurker"),
        "lurker must lose when budget is tight:\n{}",
        ctx.memory
    );
}

#[tokio::test]
async fn recap_only_identity_included_when_budget_allows() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();

    let now = jiff::Timestamp::now();
    seed_user_file(dir.path(), "11", "carol", "Carol", now, "carol body").await;
    seed_user_file(dir.path(), "99", "lurker", "Lurker", now, "lurker body").await;

    let history = history_buffer();
    history.lock().await.push_user("alice", "hi");

    let recap_sheet_logins: BTreeSet<String> = ["lurker"].into_iter().map(str::to_string).collect();

    let ctx = build_chat_turn_context(
        &store,
        BuildOpts {
            primary_history: Some(history),
            speaker_login: "carol".into(),
            recap_sheet_logins,
            ..build_opts()
        },
    )
    .await
    .unwrap();

    assert!(
        ctx.memory.contains("login=carol"),
        "speaker missing:\n{}",
        ctx.memory
    );
    assert!(
        ctx.memory.contains("login=lurker"),
        "recap-only lurker must be included when budget allows:\n{}",
        ctx.memory
    );
}

#[tokio::test]
async fn build_chat_turn_context_same_nonce_yields_identical_memory() {
    // The per-process nonce makes memory a stable cache prefix:
    // with the nonce fixed and the store unchanged, two builds must be
    // byte-identical. Guards against future per-build variation
    // (timestamps, reordering, fresh randomness) creeping into the block.
    let (store, _tmp) = test_store().await;
    store
        .write(
            &FileKind::User {
                user_id: "42".into(),
            },
            "alice body",
            UserIdentity::of("alice", "Alice"),
        )
        .await
        .unwrap();
    store
        .write_scratch(
            &FileKind::Scratch {
                slug: "quiz".into(),
            },
            "score: 3",
            Some("42"),
        )
        .await
        .unwrap();

    let opts = || BuildOpts {
        inject_byte_budget: 24576,
        nonce: "stablenonce000000".into(),
        primary_login: "main".into(),
        ..build_opts()
    };

    let first = build_chat_turn_context(&store, opts()).await.unwrap();
    let second = build_chat_turn_context(&store, opts()).await.unwrap();

    assert!(
        first.memory.contains("kind=soul"),
        "sanity: memory should hold the fenced blocks"
    );
    assert_eq!(
        first.memory, second.memory,
        "same nonce + unchanged store must yield byte-identical memory"
    );
    assert_eq!(
        first.scratch, second.scratch,
        "same nonce + unchanged store must yield byte-identical scratch"
    );
}
