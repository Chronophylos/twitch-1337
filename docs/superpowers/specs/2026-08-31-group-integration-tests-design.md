# Group Integration Tests — Design

**Date:** 2026-08-31
**Status:** Approved (brainstorming)
**Branch:** `refactor/group-integration-tests`

## Problem

Cargo compiles each `tests/*.rs` file as its own crate. Almost every core
integration test does `mod common;`, so `TestBotBuilder` and the fakes compile
~28 times; web route tests do the same with `tests/helpers/` (~22 binaries).
`--all-targets` timings on 2026-08-31 (rustc 1.98, 16 cores):

- Cold debug: 68s wall, 463 units
- `twitch-1337-core` lib-for-test: 30.7s
- Individual core itest crates: 11–15s each
- CI `test` job (`llvm-cov nextest --workspace`): ~160–200s mid-August, **267–373s**
  in the last week of August

The growth driver is first-party `core` (~62k LOC, 28 itest files) plus llvm-cov
instrumenting every test binary — not `Cargo.lock` package count (peaked at 440
in May and is 415 now).

This is the tax the [Cargo book](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#integration-tests)
documents, and the workaround it recommends: one (or a few) integration-test
targets with modules, not one file per crate.

## Goals

- Cut rustc invocations for integration tests from ~50 binaries to ~7, without
  turning “I edited `ping.rs`” into a rebuild of every AI test.
- Keep `TestBotBuilder` / web `helpers` next to the suites that use them (no
  new workspace crate — see `.out-of-scope/shared-test-support-crate.md`).
- Preserve nextest filters (`cargo nextest run -p twitch-1337-core --test ai`)
  and the gated web `e2e` target.
- No behavior change. Same `#[test]` functions, same public-API-only itests.

## Non-Goals

- Extracting `tests/common` into a `*-test-support` crate.
- Splitting `twitch-1337-core` into smaller libraries.
- `[lib] test = false` (skips the 30.7s unit-test binary of `core`). Follow-up
  if a later timings HTML still shows that unit as the wall-clock hog.
- Merging all itests into a single binary per package.
- Changing what llvm-cov measures, or making e2e a required check.

## Approach

Cargo-native only: `autotests = false` plus explicit `[[test]]` targets.
Each target is a directory with `main.rs` that `mod`s the former top-level
files and pulls in shared fixtures once via `#[path]`.

This is the layout from matklad’s “Delete Cargo Integration Tests” and from
Cargo PR #5022, split into a few domain binaries so local incremental test
edits stay cheap. nextest already runs binaries in parallel, so the win is
**compile / llvm-cov**, not runtime scheduling.

No new crates.io dependency (`libtest-mimic`, rstest, …). Those solve custom
harnesses, not this tax.

## Grouping

### `twitch-1337-core` (4 binaries)

| `[[test]]` name | Modules (today’s `tests/<name>.rs`) |
|---|---|
| `ai` | `ai`, `ai_mouth`, `ai_followup`, `ai_channel`, `ai_profile_switching`, `grok`, `helix_mouth`, `memory_v2` |
| `aviation` | `aviation`, `flight_tracker`, `tracker_orchestration` |
| `tracker` | `tracker_1337`, `pb`, `leaderboard`, `hall_of_shame`, `pw` |
| `irc` | `ping`, `announce`, `schedules_engine`, `suspend`, `haiku`, `news`, `doener`, `feedback`, `smoke`, `web_smoke`, `emote_lookup`, `emote_usage` |

`tests/common/` stays where it is. Each crate root does:

```rust
#[path = "../common/mod.rs"]
mod common;
```

Moved files replace `mod common;` with `use super::common;` so `common::…`
paths keep working.

### `twitch-1337-web` (2 + existing e2e)

| `[[test]]` name | Modules |
|---|---|
| `auth` | `auth_signed_cookies`, `auth_next`, `auth_viewer_tier`, `auth_owner`, `auth_routes`, `auth_mod_check`, `auth_viewer_guard`, `auth_session`, `auth_csrf`, `helix_moderator_check` |
| `routes` | `settings_route`, `ai_profiles_route`, `assets_smoke`, `memory_write`, `memory_read`, `emotes_routes`, `sidebar_smoke`, `flights_route`, `schedules_route`, `pings_routes`, `leaderboard_route`, `healthz` |
| `e2e` | unchanged (`tests/e2e.rs`, `required-features = ["e2e"]`) |

`tests/helpers/` stays. `auth` and `routes` crate roots `#[path]` it in.
`e2e_support/harness.rs` already uses `#[path = "../helpers/mod.rs"]`; leave
that path valid (helpers directory does not move).

## Layout

```
crates/core/tests/
  common/                 # unchanged
  ai/main.rs
  ai/ai.rs                # was tests/ai.rs
  ai/grok.rs
  …
  aviation/main.rs
  tracker/main.rs
  irc/main.rs

crates/web/tests/
  helpers/                # unchanged
  e2e.rs                  # unchanged
  e2e_support/            # unchanged
  auth/main.rs
  auth/auth_routes.rs
  …
  routes/main.rs
```

`Cargo.toml` (core, abbreviated):

```toml
[package]
autotests = false

[[test]]
name = "ai"
path = "tests/ai/main.rs"

[[test]]
name = "aviation"
path = "tests/aviation/main.rs"

[[test]]
name = "tracker"
path = "tests/tracker/main.rs"

[[test]]
name = "irc"
path = "tests/irc/main.rs"
```

Web keeps the existing e2e `[[test]]` and adds `autotests = false` plus `auth`
and `routes`.

## Docs

Update living pointers only (dated specs in `docs/superpowers/specs/` stay
as historical archives):

- `CLAUDE.md` § Adding handler — tests still live under `crates/core/tests/`,
  now grouped; `TestBotBuilder` path unchanged.
- `.cursor/rules/testing.mdc` — point flight-tracker tests at
  `crates/core/tests/aviation/flight_tracker.rs`; web helpers still
  `tests/helpers/mod.rs`.
- Comment on `crates/web/tests/helpers/mod.rs` that currently says every
  `tests/*.rs` binary includes the module.

## Verification

- `cargo nextest run -p twitch-1337-core -p twitch-1337-web` (no `e2e` feature)
  — same tests pass; binary count is 4 + 2 instead of 28 + 22.
- `cargo nextest run -p twitch-1337-core --test ai` still selects only the AI
  modules.
- Cold `cargo build --workspace --all-targets --timings` compared to the
  2026-08-31 baseline (68s / 463 units). Expect fewer dirty units and less
  CPU-time on itest crates; wall clock on a 16-core machine may move less
  than CI llvm-cov.
- `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings`.

## Adding a new integration test

Put the file in the matching directory (`tests/ai/`, `tests/aviation/`,
`tests/tracker/`, `tests/irc/`, `tests/auth/`, `tests/routes/`) and `mod` it
from that directory’s `main.rs`. Do not add a new top-level `tests/foo.rs`
— `autotests = false` means Cargo will ignore it. New domain → new `[[test]]`
+ directory, not a 29th auto-discovered crate.
