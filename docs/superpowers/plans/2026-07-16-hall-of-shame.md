# Hall of Shame (`!sb` / `!pw`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist an all-time per-user 1337 personal-worst board (`hall_of_shame.ron`), expose it via `!sb` / `!pw`, and reframe the daily slowest announce as `ms übrig`.

**Architecture:** Parallel mirror of the fast leaderboard — dedicated `{ms, date}` type, separate RON file, `Arc<RwLock<HashMap>>` plumbed IRC-only through `Services` → tracker + commands. Persist elapsed ms; convert to `60_000 - ms` only in chat strings. No dashboard / `WebState` field.

**Tech Stack:** Rust workspace (`twitch-1337-core` tracker + commands, binary wiring, `TestBotBuilder` integration tests), existing atomic RON persist helpers.

**Spec:** `docs/superpowers/specs/2026-07-16-hall-of-shame-design.md`

## Global Constraints

- Eligibility for shame updates: `ms >= SLOWEST_CHATTER_MIN_MS` (50_000) only.
- Update rule: insert or replace only when new `ms` is **strictly greater** than stored (ties keep old date).
- Chat unit: `remaining_ms = 60_000 - ms` with the word `übrig` (daily line + `!sb` + `!pw`).
- Persist elapsed `ms` after 13:37:00.000 — never store remaining.
- No shame achievement suffixes on the 13:38 stats line.
- No `WebState` / dashboard / nav changes.
- Do not merge into `leaderboard.ron`.
- Work in worktree `.worktrees/feature-hall-of-shame` on branch `feature/hall-of-shame`.
- After Rust edits: `cargo fmt --all`, then `cargo clippy --workspace --all-targets -- -D warnings`, then `cargo nextest run --workspace`.
- Commits: Conventional Commits with stupid subject + serious body (repo `git-commits.mdc`).

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/core/src/twitch/handlers/tracker_1337.rs` | `PersonalWorst`, filename const, load/save, `remaining_ms_in_minute`, `beats_own_worst`, 13:38 announce + upsert |
| `crates/core/src/commands/hall_of_shame.rs` | `!sb` command |
| `crates/core/src/commands/pw.rs` | `!pw` command |
| `crates/core/src/commands/mod.rs` | `pub mod` for new command modules |
| `crates/core/src/twitch/handlers/commands.rs` | Register `!sb` / `!pw`; add Arc to `CommandHandlerConfig` |
| `crates/core/src/twitch/handlers/spawn.rs` | `SpawnDeps.hall_of_shame`; pass into tracker + commands |
| `crates/core/src/lib.rs` | `Services.hall_of_shame`; re-export `PersonalWorst` / `load_hall_of_shame` |
| `crates/twitch-1337/src/main.rs` | Load file at startup into Services |
| `crates/core/tests/common/test_bot.rs` | Seed + wire Arc |
| `crates/core/tests/common/mod.rs` | Optional `seed_hall_of_shame` helper |
| `crates/core/tests/hall_of_shame.rs` | `!sb` integration tests |
| `crates/core/tests/pw.rs` | `!pw` integration tests |
| `crates/core/tests/tracker_1337.rs` | Update slowest asserts; add persistence cases |
| `.gitignore` | `hall_of_shame.ron` |
| `CLAUDE.md`, `README.md`, `CONTRIBUTING.md` | Mention `!sb` / `!pw` |

---

### Task 1: `PersonalWorst` type + load/save + remaining helper

**Files:**
- Modify: `crates/core/src/twitch/handlers/tracker_1337.rs`
- Modify: `crates/core/src/lib.rs` (re-exports)
- Modify: `.gitignore`
- Test: unit tests inside `tracker_1337.rs` `mod tests`

**Interfaces:**
- Produces:
  - `pub struct PersonalWorst { pub ms: u64, pub date: chrono::NaiveDate }` (`Serialize` + `Deserialize`, same derives as `PersonalBest`)
  - `pub(crate) const HALL_OF_SHAME_FILENAME: &str = "hall_of_shame.ron"`
  - `pub async fn load_hall_of_shame(data_dir: &Path) -> HashMap<String, PersonalWorst>`
  - `pub(crate) async fn save_hall_of_shame(map: &HashMap<String, PersonalWorst>, data_dir: &Path)`
  - `pub fn remaining_ms_in_minute(ms: u64) -> u64` → `60_000 - ms`
  - `fn beats_own_worst(user: &str, ms: u64, map: &HashMap<String, PersonalWorst>) -> bool` → true when absent or `ms >` stored
- Consumes: existing `atomic_save_ron_async` pattern from `save_leaderboard`

- [ ] **Step 1: Write failing unit tests**

In `tracker_1337.rs` `mod tests`, add:

```rust
#[test]
fn remaining_ms_in_minute_subtracts_from_60000() {
    assert_eq!(remaining_ms_in_minute(58_791), 1_209);
    assert_eq!(remaining_ms_in_minute(50_000), 10_000);
}

#[test]
fn beats_own_worst_when_absent_or_strictly_slower() {
    let date = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
    let mut map = HashMap::new();
    map.insert(
        "alice".into(),
        PersonalWorst {
            ms: 55_000,
            date,
        },
    );
    assert!(beats_own_worst("bob", 50_000, &map));
    assert!(beats_own_worst("alice", 56_000, &map));
    assert!(!beats_own_worst("alice", 55_000, &map));
    assert!(!beats_own_worst("alice", 54_000, &map));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core -- tracker_1337::tests::remaining_ms_in_minute_subtracts_from_60000 tracker_1337::tests::beats_own_worst_when_absent_or_strictly_slower`

Expected: FAIL / compile error — symbols missing.

- [ ] **Step 3: Implement type, helpers, load/save, gitignore, re-exports**

Next to `PersonalBest` / `LEADERBOARD_FILENAME`:

```rust
pub(crate) const HALL_OF_SHAME_FILENAME: &str = "hall_of_shame.ron";

const MINUTE_MS: u64 = 60_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonalWorst {
    /// Milliseconds after 13:37:00.000 (elapsed; typically >= 50_000 when stored).
    pub ms: u64,
    pub date: chrono::NaiveDate,
}

pub fn remaining_ms_in_minute(ms: u64) -> u64 {
    MINUTE_MS - ms
}

fn beats_own_worst(user: &str, ms: u64, map: &HashMap<String, PersonalWorst>) -> bool {
    map.get(user).map(|pw| pw.ms).is_none_or(|own| ms > own)
}
```

Clone `load_leaderboard` / `save_leaderboard` into `load_hall_of_shame` / `save_hall_of_shame`, swapping type + filename + log strings (`"hall of shame"` / `HALL_OF_SHAME_FILENAME`).

`.gitignore` — add `hall_of_shame.ron` immediately under `leaderboard.ron`.

`crates/core/src/lib.rs` re-export:

```rust
pub use twitch::{
    handlers::tracker_1337::{PersonalBest, PersonalWorst, load_hall_of_shame, load_leaderboard},
    // ...unchanged...
};
```

- [ ] **Step 4: Run unit tests to verify they pass**

Run: `cargo nextest run -p twitch-1337-core -- tracker_1337::tests::remaining_ms_in_minute_subtracts_from_60000 tracker_1337::tests::beats_own_worst_when_absent_or_strictly_slower`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/twitch/handlers/tracker_1337.rs crates/core/src/lib.rs .gitignore
git commit -m "$(cat <<'EOF'
feat(tracker): hall of shame file format just materialized out of the ether

Add PersonalWorst, hall_of_shame.ron load/save, remaining_ms_in_minute, and
beats_own_worst so the all-time slow board can persist beside the fast
leaderboard without touching leaderboard.ron.

EOF
)"
```

---

### Task 2: Plumb Arc + tracker announce/persist

**Files:**
- Modify: `crates/core/src/lib.rs` (`Services`)
- Modify: `crates/core/src/twitch/handlers/spawn.rs`
- Modify: `crates/core/src/twitch/handlers/tracker_1337.rs` (`run_1337_handler`)
- Modify: `crates/twitch-1337/src/main.rs`
- Modify: `crates/core/tests/common/test_bot.rs`
- Modify: `crates/core/tests/common/mod.rs` (optional seed helper)
- Modify: `crates/core/tests/tracker_1337.rs`
- Test: `crates/core/tests/tracker_1337.rs`

**Interfaces:**
- Consumes: `load_hall_of_shame`, `save_hall_of_shame`, `beats_own_worst`, `remaining_ms_in_minute`, `PersonalWorst`
- Produces: `Services.hall_of_shame: Arc<RwLock<HashMap<String, PersonalWorst>>>` threaded to `run_1337_handler(..., hall_of_shame, ...)`. Commands wiring lands in Task 3.

- [ ] **Step 1: Write / update failing tracker integration tests**

Update existing slowest tests in `crates/core/tests/tracker_1337.rs`:

```rust
// tracker_1337_posts_slowest_only_for_final_ten_seconds — change asserts to:
assert!(
    stats.contains("Am langsamsten war charlie mit 1300ms übrig"),
    "expected slowest remaining ms, got: {stats}"
);
assert!(
    !stats.contains(" mit 58s"),
    "slowest must not use whole seconds, got: {stats}"
);
// 58_700 elapsed → 60_000 - 58_700 = 1_300
```

Keep `tracker_1337_does_not_post_slowest_before_final_ten_seconds` asserting absence of `Am langsamsten`.

Add persistence tests (mirror leaderboard sub-second test style):

```rust
#[tokio::test]
async fn tracker_1337_persists_qualifying_personal_worst() {
    // Setup at 13:35, advance_to_1337_window, inject charlie at +58_700ms,
    // advance to 13:38, expect_say, then read hall_of_shame.ron — must contain
    // charlie and 58700 (elapsed). alice at +49_999 must NOT appear.
}

#[tokio::test]
async fn tracker_1337_personal_worst_updates_only_when_slower() {
    // Seed hall_of_shame with alice ms=55_000 date D1 via TestBotBuilder.
    // Inject alice at +56_000 → file becomes 56000 / today.
    // Next day (or second session): inject alice at +55_500 → file stays 56000.
    // Equal 56000 also must not change date (optional assert on date string).
}
```

Use `TMI_TS_13_37_BERLIN + offset` like existing tests. For multi-day, either two bot spawns with seeded file after first run, or one session is enough for “updates when slower” + a second test that seeds 56000 and injects 55500 expecting no change.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core -- tracker_1337_posts_slowest_only_for_final_ten_seconds tracker_1337_persists_qualifying_personal_worst`

Expected: FAIL — still `58s` / no file.

- [ ] **Step 3: Plumb Arc end-to-end and update tracker**

1. `Services` + `SpawnDeps` + destructure in `run_bot` / `spawn_handlers`: add `hall_of_shame` next to `leaderboard`.
2. `run_1337_handler` signature — add parameter after `leaderboard`:

```rust
hall_of_shame: Arc<tokio::sync::RwLock<HashMap<String, PersonalWorst>>>,
```

3. Daily slowest suffix:

```rust
let remaining = remaining_ms_in_minute(slowest_ms);
message.push_str(&format!(
    " | Am langsamsten war {slowest_user} mit {remaining}ms übrig"
));
```

4. After the leaderboard update block (or in a sibling write lock), upsert shame:

```rust
{
    let users = total_users.lock().await;
    let mut shame = hall_of_shame.write().await;
    for (username, ms) in users.iter() {
        if *ms >= SLOWEST_CHATTER_MIN_MS && beats_own_worst(username, *ms, &shame) {
            shame.insert(
                username.clone(),
                PersonalWorst {
                    ms: *ms,
                    date: today,
                },
            );
        }
    }
    save_hall_of_shame(&shame, &data_dir).await;
}
```

5. Do **not** touch `CommandHandlerConfig` yet — Task 3 adds the field when `!sb` registers (keeps `-D warnings` clean).

6. `main.rs`: load beside leaderboard:

```rust
let hall_of_shame = Arc::new(RwLock::new(
    load_hall_of_shame(&get_data_dir()).await,
));
// put on Services { ..., hall_of_shame, ... }
// do NOT pass into WebState
```

7. `TestBotBuilder`:

```rust
seeded_hall_of_shame: Option<HashMap<String, PersonalWorst>>,
// with_seeded_hall_of_shame(...)
// in spawn: write hall_of_shame.ron like leaderboard
// Services.hall_of_shame: Arc::new(RwLock::new(load_hall_of_shame(...).await))
```

8. Optional helper in `tests/common/mod.rs`:

```rust
pub fn seed_hall_of_shame(
    entries: &[(&str, u64, NaiveDate)],
) -> HashMap<String, PersonalWorst> { /* mirror seed_leaderboard */ }
```

- [ ] **Step 4: Run tracker tests**

Run: `cargo nextest run -p twitch-1337-core -- tracker_1337`

Expected: PASS (all tracker_1337 tests).

- [ ] **Step 5: Commit**

```bash
git add \
  crates/core/src/lib.rs \
  crates/core/src/twitch/handlers/spawn.rs \
  crates/core/src/twitch/handlers/tracker_1337.rs \
  crates/twitch-1337/src/main.rs \
  crates/core/tests/common/test_bot.rs \
  crates/core/tests/common/mod.rs \
  crates/core/tests/tracker_1337.rs
git commit -m "$(cat <<'EOF'
feat(tracker): slowest kings now get their eternal L carved in ron

Wire hall_of_shame through Services/SpawnDeps, persist qualifying
personal worsts at 13:38, and announce remaining ms übrig instead of
whole seconds.

EOF
)"
```

---

### Task 3: `!sb` command

**Files:**
- Create: `crates/core/src/commands/hall_of_shame.rs`
- Modify: `crates/core/src/commands/mod.rs`
- Modify: `crates/core/src/twitch/handlers/commands.rs`
- Modify: `crates/core/src/twitch/handlers/spawn.rs` (pass Arc into `CommandHandlerConfig`)
- Create: `crates/core/tests/hall_of_shame.rs`
- Test: `crates/core/tests/hall_of_shame.rs`

**Interfaces:**
- Consumes: `Arc<RwLock<HashMap<String, PersonalWorst>>>`, `remaining_ms_in_minute`
- Produces: `HallOfShameCommand` registered as `"!sb"`

- [ ] **Step 1: Write failing integration tests**

Create `crates/core/tests/hall_of_shame.rs`:

```rust
use twitch_1337_core as twitch_1337;
mod common;

use std::collections::HashMap;
use std::time::Duration;

use chrono::NaiveDate;
use common::TestBotBuilder;
use twitch_1337::PersonalWorst;

#[tokio::test]
async fn sb_returns_slowest_personal_worst() {
    let mut seeded = HashMap::new();
    seeded.insert(
        "alice".into(),
        PersonalWorst {
            ms: 58_791,
            date: NaiveDate::from_ymd_opt(2026, 4, 17).unwrap(),
        },
    );
    seeded.insert(
        "bob".into(),
        PersonalWorst {
            ms: 55_000,
            date: NaiveDate::from_ymd_opt(2026, 4, 16).unwrap(),
        },
    );

    let mut bot = TestBotBuilder::new()
        .with_seeded_hall_of_shame(seeded)
        .spawn()
        .await;

    bot.send("alice", "!sb").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(out.contains("alice"), "missing alice: {out}");
    assert!(
        out.contains("1209ms übrig"),
        "expected remaining ms, got: {out}"
    );
    assert!(out.contains("17.04.2026"), "missing date: {out}");
    assert!(!out.contains("bob"), "only worst should show: {out}");

    bot.shutdown().await;
}

#[tokio::test]
async fn sb_empty_state_message() {
    let mut bot = TestBotBuilder::new().spawn().await;
    bot.send("alice", "!sb").await;
    let out = bot.expect_say(Duration::from_secs(2)).await;
    assert!(
        out.contains("Noch keine Einträge vorhanden"),
        "expected empty-state, got: {out}"
    );
    bot.shutdown().await;
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core -- hall_of_shame`

Expected: FAIL — `!sb` unknown / silent / wrong reply.

- [ ] **Step 3: Implement and register `!sb`**

`crates/core/src/commands/hall_of_shame.rs` — mirror `leaderboard.rs`:

```rust
// HallOfShameCommand { hall_of_shame: Arc<RwLock<HashMap<String, PersonalWorst>>> }
// name: "!sb"
// execute:
//   max_by_key(|(_, pw)| pw.ms)
//   format!(
//     "Der langsamste 1337 ist {username} mit {}ms übrig am {date}",
//     remaining_ms_in_minute(pw.ms),
//   )
//   else "Noch keine Einträge vorhanden"
```

Use `crate::twitch::handlers::tracker_1337::{PersonalWorst, remaining_ms_in_minute}` (or re-export `remaining_ms_in_minute` from crate root if cleaner — re-export is optional; path import is fine).

`commands/mod.rs`: `pub mod hall_of_shame;`

`CommandHandlerConfig` + destructure + register next to leaderboard:

```rust
Box::new(commands::hall_of_shame::HallOfShameCommand::new(
    hall_of_shame.clone(),
)),
```

`spawn.rs`: pass `hall_of_shame` into `CommandHandlerConfig` (clone before moving into tracker if both need ownership — `hall_of_shame.clone()` for tracker, move/clone for commands).

- [ ] **Step 4: Run tests**

Run: `cargo nextest run -p twitch-1337-core -- hall_of_shame`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add \
  crates/core/src/commands/hall_of_shame.rs \
  crates/core/src/commands/mod.rs \
  crates/core/src/twitch/handlers/commands.rs \
  crates/core/src/twitch/handlers/spawn.rs \
  crates/core/tests/hall_of_shame.rs
git commit -m "$(cat <<'EOF'
feat(chat): !sb serves the ultimate L board and we are so back

Add the hall-of-shame command mirroring !lb: global max personal worst
shown as remaining ms übrig from the end of the 13:37 minute.

EOF
)"
```

---

### Task 4: `!pw` command

**Files:**
- Create: `crates/core/src/commands/pw.rs`
- Modify: `crates/core/src/commands/mod.rs`
- Modify: `crates/core/src/twitch/handlers/commands.rs`
- Create: `crates/core/tests/pw.rs`
- Test: `crates/core/tests/pw.rs`

**Interfaces:**
- Consumes: same Arc as `!sb`, `normalize_username`, `remaining_ms_in_minute`
- Produces: `PwCommand` registered as `"!pw"`

- [ ] **Step 1: Write failing integration tests**

Create `crates/core/tests/pw.rs` mirroring `pb.rs`:

```rust
// seeded_alice: PersonalWorst { ms: 58_791, date: 2026-04-17 }
// remaining display: 1209

// pw_returns_callers_own_pw — "!pw" → "Dein PW" + "1209" + "übrig" + date
// pw_returns_other_user_pw — "!pw alice" → "PW von alice"
// pw_handles_at_prefix_and_case — "!pw @ALICE"
// pw_empty_state_self — "Du hast noch keinen PW"
// pw_empty_state_other — "bob hat noch keinen PW"
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core -- pw::`

Expected: FAIL.

- [ ] **Step 3: Implement and register `!pw`**

Mirror `commands/pb.rs` with `PersonalWorst` + copy from the spec:

| | Self | Other |
|---|---|---|
| Hit | `Dein PW ist {remaining}ms übrig am {date}` | `PW von {user}: {remaining}ms übrig am {date}` |
| Miss | `Du hast noch keinen PW` | `{user} hat noch keinen PW` |

Register after `!sb`:

```rust
Box::new(commands::pw::PwCommand::new(hall_of_shame)),
```

- [ ] **Step 4: Run tests**

Run: `cargo nextest run -p twitch-1337-core -- pw::`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add \
  crates/core/src/commands/pw.rs \
  crates/core/src/commands/mod.rs \
  crates/core/src/twitch/handlers/commands.rs \
  crates/core/tests/pw.rs
git commit -m "$(cat <<'EOF'
feat(chat): !pw lets you flex your intentional skill issue

Personal-worst lookup mirroring !pb, reporting remaining ms übrig for
self or another user from hall_of_shame.ron.

EOF
)"
```

---

### Task 5: Docs + CI gate

**Files:**
- Modify: `CLAUDE.md` (feature list line)
- Modify: `README.md` (13:37 ritual bullet)
- Modify: `CONTRIBUTING.md` (`system:tracker` row)

**Interfaces:** none

- [ ] **Step 1: Update docs**

`CLAUDE.md` features line — add hall of shame (`!sb`, `!pw`) next to leaderboard (`!lb`).

`README.md` ritual bullet — extend to mention slowest / `!sb` (one short clause; keep the paragraph readable).

`CONTRIBUTING.md`:

```markdown
| `system:tracker` | 1337 tracker + leaderboard (`!lb`) + hall of shame (`!sb`) |
```

Also ensure `CLAUDE.md` Data dir file list mentions `hall_of_shame.ron` beside `leaderboard.ron`.

- [ ] **Step 2: Full CI gate**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

Expected: all green.

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md README.md CONTRIBUTING.md
git commit -m "$(cat <<'EOF'
docs(tracker): tell the people about the shame board before it becomes lore

Document !sb / !pw and hall_of_shame.ron alongside the existing 1337
leaderboard surface in CLAUDE, README, and CONTRIBUTING labels.

EOF
)"
```

---

## Spec coverage checklist

| Spec requirement | Task |
|---|---|
| `PersonalWorst` + `hall_of_shame.ron` | 1 |
| Load/save atomic RON + gitignore | 1 |
| `remaining_ms_in_minute` / persist elapsed | 1 |
| `beats_own_worst` strict `>` | 1 |
| Services / SpawnDeps / main / TestBot plumbing | 2 |
| Daily announce `ms übrig` | 2 |
| Persist all qualifying users at 13:38 | 2 |
| No shame achievements | 2 (omit) |
| No WebState | 2 (omit) |
| `!sb` copy + empty state | 3 |
| `!pw` copy + normalize | 4 |
| Integration tests | 2–4 |
| Docs | 5 |

## Placeholder / consistency self-review

- No TBD steps; types named `PersonalWorst` / `hall_of_shame` consistently.
- Commands import `remaining_ms_in_minute` from tracker module (same helper as announce).
- Task 2 deliberately does **not** add `CommandHandlerConfig.hall_of_shame` until Task 3 to keep each task compiling under `-D warnings`.
