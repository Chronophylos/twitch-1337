# Emote tracker chat copy (`!e`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Hard-rename `!emote` → `!e` and replace all chat-facing English emote-tracker strings with the locked German copy from the design spec.

**Architecture:** Pure string/constant changes in `emote_tracker::{format,lookup}` plus the command `name()` and IRC integration tests. No resolve-order, attribution, or schema changes. Dashboard audit templates are out of scope except where they already call `format_actor_display` / `format_berlin_millis` (shared fallbacks become German as a deliberate side effect of the locked decisions).

**Tech Stack:** Existing Rust command dispatcher, `EmoteTrackerHandle`, `TestBotBuilder` IRC tests.

**Spec:** [docs/superpowers/specs/2026-07-21-emote-tracker-chat-copy-design.md](../specs/2026-07-21-emote-tracker-chat-copy-design.md)

## Global Constraints

- Trigger: `!e` only — no `!emote` alias
- Usage: `Benutzung: !e <Name, ID oder https://7tv.app/emotes/…> FDM`
- Not found: `Emote nicht gefunden FDM`
- Success template: `{name} ({id}) {clause}` (space only; no `—`, no `|`)
- Bootstrap clause: `war schon da als Tracking anfing ({when})`
- Add: `hinzugefügt von {by} am {when}`
- Remove: `entfernt von {by} am {when}`
- Rename: `umbenannt von {by} am {when}`
- No history / gap: `noch keine Set-History`
- Actor fallback: `unbekannt`
- Invalid timestamp: `ungültige Zeit`
- Potat clause unchanged: `{login} via PotatBotat`
- Timestamps unchanged: `DD.MM.YYYY HH:MM` Europe/Berlin
- Commits: Conventional Commits (feral subject, serious body). Never commit `emote_tracker.sqlite*` / secrets.
- Worktree: `.worktrees/feature-emote-tracker-chat-copy` on `feature/emote-tracker-chat-copy`

---

## File map

| File | Responsibility |
|---|---|
| `crates/core/src/emote_tracker/format.rs` | Actor fallback + invalid-time German strings |
| `crates/core/src/emote_tracker/lookup.rs` | Usage / not-found constants + `format_emote_chat_reply` |
| `crates/core/src/commands/emote.rs` | Command trigger `!e` |
| `crates/core/src/twitch/handlers/commands.rs` | Doc comment that mentions the trigger |
| `crates/core/src/twitch/handlers/spawn.rs` | Doc comment that mentions the trigger |
| `crates/core/src/emote_tracker/eventapi.rs` | Unit test asserting `format_actor_display` fallback |
| `crates/core/tests/emote_lookup.rs` | IRC integration expectations |
| Historical plans/specs | Leave prose as-is (historical); this plan + design are the live authority |

---

### Task 1: German shared format fallbacks

**Files:**
- Modify: `crates/core/src/emote_tracker/format.rs`
- Modify: `crates/core/src/emote_tracker/eventapi.rs` (test assertion only)
- Test: unit tests inside `format.rs`; `eventapi.rs` actor-null test

**Interfaces:**
- Consumes: existing `format_actor_display` / `format_berlin_millis` signatures (unchanged)
- Produces: fallback strings `unbekannt` and `ungültige Zeit`

- [ ] **Step 1: Update failing assertions in `format.rs` tests**

Change expected fallbacks:

```rust
assert_eq!(format_actor_display(None, None, None), "unbekannt");
assert_eq!(format_actor_display(Some(""), Some(""), None), "unbekannt");
```

Update the doc comment on `format_actor_display` to say `` `unbekannt` `` instead of `` `unknown` ``.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core format_actor_display_prefers_display --nocapture`

Expected: FAIL — got `"unknown"`, expected `"unbekannt"`

- [ ] **Step 3: Implement German fallbacks**

In `format_actor_display`, replace the final `"unknown".to_owned()` with `"unbekannt".to_owned()`.

In `format_berlin_millis`, replace `"invalid time".to_owned()` with `"ungültige Zeit".to_owned()`.

- [ ] **Step 4: Fix `eventapi.rs` assertion that expects `"unknown"`**

In `live_push_null_body_actor_uses_value_actor_id_for_add` (or the sibling null-actor display test around the `format_actor_display(...), "unknown"` assertion), change the expected string to `"unbekannt"`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo nextest run -p twitch-1337-core format_actor_display_prefers_display format_berlin_millis_uses_berlin_offset live_push_null_body`

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/emote_tracker/format.rs crates/core/src/emote_tracker/eventapi.rs
git commit -m "$(cat <<'EOF'
fix(emote): unknown got deported and unbekannt just moved in 🇩🇪👻

Chat-facing format fallbacks for missing actors and invalid timestamps
now use German strings shared by !e and any caller of format_actor_display
/ format_berlin_millis.

EOF
)"
```

---

### Task 2: German `!e` reply formatter + constants

**Files:**
- Modify: `crates/core/src/emote_tracker/lookup.rs`
- Test: unit tests inside `lookup.rs` (`format_bootstrap_add_remove_copy`)

**Interfaces:**
- Consumes: `format_actor_display`, `format_set_event_attribution`, `format_berlin_millis` from Task 1
- Produces:
  - `EMOTE_CMD_USAGE = "Benutzung: !e <Name, ID oder https://7tv.app/emotes/…> FDM"`
  - `EMOTE_NOT_FOUND = "Emote nicht gefunden FDM"`
  - `format_emote_chat_reply` → `{name} ({id}) {clause}` with locked German clauses

- [ ] **Step 1: Rewrite unit test expectations to locked German copy**

In `format_bootstrap_add_remove_copy`, replace assertions with:

```rust
assert_eq!(
    format_emote_chat_reply(&emote, Some(&bootstrap)),
    format!("Kappa ({ID}) war schon da als Tracking anfing ({when})")
);
assert_eq!(
    format_emote_chat_reply(&emote, Some(&add)),
    format!("Kappa ({ID}) hinzugefügt von Mod User am {when}")
);
assert_eq!(
    format_emote_chat_reply(&emote, Some(&add_unknown)),
    format!("Kappa ({ID}) hinzugefügt von unbekannt am {when}")
);
assert_eq!(
    format_emote_chat_reply(&emote, Some(&remove)),
    format!("Kappa ({ID}) entfernt von unbekannt am {when}")
);
assert_eq!(
    format_emote_chat_reply(&emote, Some(&attributed)),
    format!("Kappa ({ID}) hinzugefügt von nozzlenols via PotatBotat am {when}")
);
assert_eq!(
    format_emote_chat_reply(&emote, Some(&rename)),
    format!("Kappa ({ID}) umbenannt von Mod User am {when}")
);
assert_eq!(
    format_emote_chat_reply(&emote, None),
    "Kappa (0123456789abcdef01234567) noch keine Set-History"
);
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core format_bootstrap_add_remove_copy`

Expected: FAIL on English / em-dash expectations

- [ ] **Step 3: Implement constants + formatter**

```rust
pub const EMOTE_CMD_USAGE: &str =
    "Benutzung: !e <Name, ID oder https://7tv.app/emotes/…> FDM";
pub const EMOTE_NOT_FOUND: &str = "Emote nicht gefunden FDM";

pub fn format_emote_chat_reply(emote: &EmoteRow, lifecycle: Option<&SetEventRow>) -> String {
    let name = &emote.name;
    let id = &emote.emote_id;
    let Some(event) = lifecycle else {
        return format!("{name} ({id}) noch keine Set-History");
    };
    let when = format_berlin_millis(event.at);
    match event.kind {
        SetEventKind::Bootstrap => {
            format!("{name} ({id}) war schon da als Tracking anfing ({when})")
        }
        SetEventKind::Add | SetEventKind::Remove | SetEventKind::Rename => {
            let verb = match event.kind {
                SetEventKind::Add => "hinzugefügt",
                SetEventKind::Remove => "entfernt",
                SetEventKind::Rename => "umbenannt",
                SetEventKind::Bootstrap | SetEventKind::Gap => unreachable!(),
            };
            let by = super::format::format_set_event_attribution(
                event.requester_login.as_deref(),
                event.via.as_deref(),
            )
            .unwrap_or_else(|| {
                super::format::format_actor_display(
                    event.actor_display_name.as_deref(),
                    event.actor_username.as_deref(),
                    event.actor_7tv_id.as_deref(),
                )
            });
            format!("{name} ({id}) {verb} von {by} am {when}")
        }
        SetEventKind::Gap => {
            format!("{name} ({id}) noch keine Set-History")
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core format_bootstrap_add_remove_copy`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/emote_tracker/lookup.rs
git commit -m "$(cat <<'EOF'
feat(emote): !e replies went full deutsch no thoughts head empty 🇩🇪💬

Replace English set-audit one-liners and usage/not-found constants with
the locked German chat copy (space-separated clause, FDM on errors).

EOF
)"
```

---

### Task 3: Command rename `!emote` → `!e` + IRC tests

**Files:**
- Modify: `crates/core/src/commands/emote.rs`
- Modify: `crates/core/src/twitch/handlers/commands.rs` (doc comment only)
- Modify: `crates/core/src/twitch/handlers/spawn.rs` (doc comment only)
- Modify: `crates/core/tests/emote_lookup.rs`

**Interfaces:**
- Consumes: `EMOTE_CMD_USAGE`, `EMOTE_NOT_FOUND`, `format_emote_chat_reply` from Task 2
- Produces: `EmoteCommand::name() == "!e"`

- [ ] **Step 1: Update IRC integration tests first**

In `crates/core/tests/emote_lookup.rs`:

```rust
bot.send("alice", "!e Kappa").await;
let expected = format!("Kappa ({TEST_EMOTE_ID}) war schon da als Tracking anfing ({when})");

// add test:
bot.send("alice", &format!("!e https://7tv.app/emotes/{TEST_EMOTE_ID}")).await;
let expected = format!("Kappa ({TEST_EMOTE_ID}) hinzugefügt von Mod User am {when}");

// usage / not found:
bot.send("alice", "!e").await;
assert_eq!(
    usage,
    "Benutzung: !e <Name, ID oder https://7tv.app/emotes/…> FDM"
);
bot.send("alice", "!e NoSuchEmote").await;
assert_eq!(nf, "Emote nicht gefunden FDM");

// absent when disabled:
bot.send("alice", "!e Kappa").await;
```

- [ ] **Step 2: Run IRC tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core emote_command_`

Expected: FAIL — still registered as `!emote` and/or English replies

- [ ] **Step 3: Rename command + comments**

`crates/core/src/commands/emote.rs`:

```rust
fn name(&self) -> &str {
    "!e"
}
```

`commands.rs` doc comment: `None` disables `!e` (was `!emote`).

`spawn.rs` comment: Plan 4 `!e` (was `!emote`).

- [ ] **Step 4: Run IRC tests to verify they pass**

Run: `cargo nextest run -p twitch-1337-core emote_command_`

Expected: PASS (4 tests)

- [ ] **Step 5: Full CI gate for touched crate**

Run:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

Expected: all green

- [ ] **Step 6: Commit**

```bash
git add \
  crates/core/src/commands/emote.rs \
  crates/core/src/twitch/handlers/commands.rs \
  crates/core/src/twitch/handlers/spawn.rs \
  crates/core/tests/emote_lookup.rs
git commit -m "$(cat <<'EOF'
feat(emote): !emote got shrunk to !e and chat said guten tag 🇩🇪✂️

Hard-rename the public set-audit command trigger and align IRC
integration tests with the German usage, not-found, and success copy.

EOF
)"
```

---

## Spec coverage checklist

| Spec requirement | Task |
|---|---|
| `!e` hard rename, no alias | 3 |
| German usage + not-found with `FDM` | 2, 3 |
| Success space template + German clauses | 2 |
| `unbekannt` / `ungültige Zeit` | 1 |
| Potat `via PotatBotat` unchanged | 2 (attribution path untouched) |
| No dashboard template rewrite | honored (out of scope) |
| Unit + IRC tests | 1–3 |
| CI gate | Task 3 Step 5 |

## Self-review notes

- No placeholders / TBD steps
- Historical plan/spec docs that still say `!emote` are intentionally left alone; this plan + design are live authority
- `format_actor_display` change also updates audit Actor column fallback to `unbekannt` — accepted side effect called out in Architecture
