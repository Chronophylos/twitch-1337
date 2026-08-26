# CLAUDE.md

Guide Claude Code work repo.

## Project

Rust Twitch IRC bot. Features: 1337 tracker (13:37 Berlin), leaderboard (`!lb`), hall of shame (`!sb`, `!pw`), ping system (`!p`, `!<ping>`), scheduled messages (config.toml), latency monitor, AI (`!ai`, provider profiles managed at `/ai-profiles`), flight tracker (`!track`, `!untrack`, `!flight`, `!flights`), aviation lookups (`!up`, `!fl`), shipment tracking (AI tool, DHL), feedback (`!fb`), version (`!v`).

Single persistent IRC connection, broadcast channel routes to independent handler tasks.

## Crate layout

| Crate | Role |
|---|---|
| `crates/core` | Bot logic: IRC handlers, aviation, AI memory, pings, schedules, config/settings types |
| `crates/data` (`twitch-1337-data`) | Baked assets: location CSVs, emotes, prompts, flight-plan DB generation |
| `crates/random-flight` | In-tree geo + route selection + flight-plan math (depends on `twitch-1337-data`) |
| `crates/llm` | LLM backends, agent loop, tool calling |
| `crates/web` | Axum dashboard, routes, templates, auth |
| `crates/twitch-1337` | Binary entrypoint (`main.rs`), builds `Services`, calls `run_bot` |

## Commands

```bash
# Dev
cargo build | cargo run | cargo check | cargo test
RUST_LOG=debug cargo run   # all IRC + handler activity
RUST_LOG=trace cargo run   # every ServerMessage

# Pre-commit (CI gate — required, in order)
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
cargo audit                # optional locally; CI job fails on advisories

# Deploy (Justfile, podman local → docker.homelab SSH)
just build | just push | just restart | just deploy
```

## CI & branch policy

`main` is branch-protected (admin enforced). Direct pushes rejected — use a PR.
Linear history required, force-push + delete blocked, conversations must resolve.

**Branch naming:** `feature/` features, `spec/` specs, `fix/` bug fixes, `refactor/` refactors, `build/` CI / dependencies / Docker. Slug after slash, kebab-case.

**Required status checks (9, must all pass to merge):**

| Check | Workflow | Purpose |
|---|---|---|
| `fmt` | ci.yml | cargo fmt --check |
| `clippy` | ci.yml | cargo clippy --workspace --all-targets -- -D warnings |
| `test` | ci.yml | cargo llvm-cov nextest --workspace (lcov artifact; no threshold gate) |
| `cargo audit` | ci.yml | RustSec CVE scan of Cargo.lock via rustsec/audit-check |
| `hadolint (Dockerfile)` | sast.yml | Dockerfile lint, SARIF → Security tab |
| `trivy config (IaC)` | sast.yml | IaC misconfig scan, HIGH/CRITICAL only |
| `actionlint (workflows)` | sast.yml | Workflow YAML + shell lint |
| `zizmor (workflows)` | sast.yml | Workflow security (injection, perms, pinning) |
| `gitleaks (secrets)` | sast.yml | Full-history secret scan |

`Docker` (build + push `ghcr.io/chronophylos/twitch-1337:latest` and
`:b<run_number>`) triggers on push to `main` with paths-ignore for
docs. Not a required check.
`GHCR retention` runs Sundays 04:00 UTC; prunes old image versions
keeping the last 30 plus `:latest`. Not a required check.
`Data refresh` runs Sundays 03:00 UTC; opens a `chore/data-refresh` PR.

**Native GitHub security (repo settings):** secret_scanning, push_protection,
dependabot_security_updates — all enabled. Push-protection blocks commits
containing known provider tokens at the server.

**Dependabot** (`.github/dependabot.yml`): weekly PRs for `cargo`, `github-actions`,
`docker`. Cargo minor+patch grouped as `rust-minor-patch`; GitHub Actions minor+patch
grouped as `actions-minor-patch`. Docker ecosystem bumps both tag AND sha256 digest in
Dockerfile.

**Action pinning:** security-critical actions pinned to **commit SHA** with version
comment: `rustsec/audit-check`, `gitleaks/gitleaks-action`, `zizmorcore/zizmor-action`,
`actions/delete-package-versions`, `aquasecurity/trivy-action` (Mar 2026 supply-chain
incident — always SHA-pin trivy). Others pinned to major tags; Dependabot keeps them
current.

**Typical PR flow:**
1. branch → commit → push → `gh pr create`
2. wait for 9 checks green; rebase on main if `strict` blocks merge
3. `gh pr merge --squash`

**Release flow (rolling):**
1. Merge a PR into `main`. `docker.yml` triggers on every push to `main`;
   a `dorny/paths-filter` gate (`.github/path-filters.yml`) skips the build
   when only docs, workflows, or other non-image paths changed. Use
   **Actions → Docker → Run workflow** (`workflow_dispatch`) to force a
   rebuild when needed (rollback verification, cache bust).
2. CI builds the musl static binary and pushes the image to
   `ghcr.io/chronophylos/twitch-1337` with tags `latest` and
   `b<github.run_number>` (e.g. `b1234`).
3. The homelab webhook fires on GHCR push; the server pulls `:latest`
   and `docker compose up -d` restarts the bot.
4. The dashboard sidebar and the startup log line `Build info` show
   `BUILD_NUM` + `GIT_SHA` — that is the source of truth for "what is
   running right now".
5. Rollback: `ssh docker.homelab`, edit the compose file to pin
   `:b<N-1>`, `docker compose up -d`. The previous image is still
   present until the weekly GHCR retention prune (keeps last 30).

There are no `vX.Y.Z` tags, no GitHub Releases, no `CHANGELOG.md`. `git
log` on `main` is the release history. The workspace version stays at
`0.0.0` permanently.

**When `cargo audit` fails:**
- Check open Dependabot PRs first (weekly); a bump may already be queued.
- Transitive vulns: `cargo tree -i <crate>` to find the parent; bump the parent.
- If two majors of one crate coexist in Cargo.lock (e.g. rustls-webpki 0.101 + 0.103),
  both must be resolved — usually by bumping the dep pulling in the old major.
- Last resort: ignore in `.cargo/audit.toml` with a written-down reason.

**When Dependabot cargo PRs break `clippy` or `test`:**
- Breaking-API bump; adapt code on the dependabot branch and force-push
  (`git push origin dependabot/cargo/<name>-<ver>`). CI reruns, then merge.
- Example: rand 0.10 moved `.random::<T>()` from `Rng` to `RngExt` — generic bounds
  become `impl rand::RngExt`.

## Config

`config.toml` (copy `config.toml.example`). Bootstrap sections only: `[twitch]` (secrets + channel/username), `[ai]` (`[ai.keys]` named credentials only, optional), `[aviationstack]` (api_key only, optional), `[dhl]` (api_key only, optional), `[langfuse]` (public/secret keys, optional), `[web]` (bootstrap fields only). Schema + defaults in `config.toml.example` — treat as source of truth. Everything else lives in `settings.ron` managed via the dashboard.

`[twitch]` holds OAuth credentials (`refresh_token`, `client_id`, `client_secret`), `channel`, `username`, and `owner` (optional Twitch user ID with full dashboard access). `owner` is bootstrap-only and lives here because it gates the settings page — it must not be editable from the thing it controls. Permission lists (`hidden_admins`, `viewer_allowlist`), channel pointers (`admin_channel`, `ai_channel`), and `expected_latency` live in `settings.ron`, managed via `/settings → Twitch · Permissions` and `Twitch · Channels`. `admin_channel` and `ai_channel` changes require a bot restart. Mods always pass the dashboard auth check; `viewer_allowlist` grants read-only access to non-mods.

`[twitch.mouths.aurora]` and `[twitch.mouths.grok]` are optional bootstrap
sections giving the two AI agents their own Twitch accounts for outbound chat.
Each holds `username` + `refresh_token` (`SecretString`, `Deserialize`-only like
the rest of config.toml). Both use the bot's `client_id`/`client_secret`, so the
tokens must be authorized against the same Twitch app, with scope
`user:write:chat` and nothing else. Refreshed tokens persist per mouth to
`$DATA_DIR/token-aurora.ron` and `$DATA_DIR/token-grok.ron`. The router is built
once at startup, so adding or removing a mouth needs a bot restart. A mouth that
is absent or fails to build is skipped and that agent replies through the
utility bot's IRC connection instead. The persona name each agent speaks under
lives in `settings.ron`: `ai.behavior.persona_name` (default `"Aurora"`) and
`ai.grok.persona_name` (default `"Grok"`). Aurora's chat-reply length
(`ai.behavior.reply_target_chars` / `reply_soft_chars` / `reply_hard_chars`,
defaults 480/490/490) is live-tunable on `/settings → AI · Behavior`; Grok
keeps a baked 480/500 prompt and a 490/490 reword-clamp.

`[ai]` carries only credentials: an `[ai.keys]` table of named keys. A profile
stores a key *name*, never the secret, so switching providers from the dashboard
never touches config.toml. A leftover `[ai].api_key` is ignored (startup warn);
move it into `[ai.keys]` and set the profile's key name. Provider, base URL,
model, timeout, reasoning effort and service tier live in **profiles** — a
named list (`ai.profiles`) CRUD'd at `/ai-profiles` (owner only — a profile
carries `base_url`/`key`, the same credential surface `/settings` keeps
owner-only). `ai.active_profile`
names the profile `!ai` uses; dreamer, grok, media and recap each take an
optional profile picker at `/settings` (recap: empty = off; the others fall
back to the active profile). Sub-agents do not override model / effort / tier
on top of a profile — pick a different profile instead. Dreamer and media keep
their own timeout knobs. Every AI client is resolved per call through `LlmRegistry`
(`crates/core/src/llm_factory.rs`), so switching `active_profile` or editing a
profile applies on the next turn — no restart. Each profile declares an
`AiProvider` (`open_router`, `open_ai`, `open_ai_compatible`, `ollama`) whose
`caps()` table decides which `reasoning_effort` values it accepts and whether
`service_tier` applies — see `crates/core/src/settings/provider.rs`. Memory
caps, dreamer schedule, web/emote/media tool toggles, history caps, and every
other runtime knob live in the dashboard (`/settings`, owner only) and
persist to `$DATA_DIR/settings.ron` (schema v2). On first v2 launch any
legacy hoisted `[ai]` keys in `config.toml` are migrated into
`settings.ron` once (sentinel: `$DATA_DIR/.ai_migrated_v2`); subsequent
edits to those legacy keys are ignored.

`[aviationstack]` keeps only `api_key` in config.toml. `enabled`, `base_url`,
and `timeout_secs` live in `settings.ron` (restart-required for connection
changes), managed via `/settings → Aviationstack`.

`[dhl]` holds only `api_key` and is optional. Its presence enables Aurora's
`track_shipment` tool; absent, the tool is never registered with the model.
There is no `settings.ron` section and no dashboard toggle — AI tools have no
per-tool toggles, unlike Aviationstack, which backs commands. Adding or
removing the key needs a bot restart.

`[langfuse]` holds `public_key` + `secret_key` and an optional `host`
(default `https://cloud.langfuse.com`, EU). Presence enables first-party
OTLP export of LLM Turns (all backends, nested tool spans). Init or export
failure logs and disables tracing; the bot stays up. No dashboard card.
Turn off OpenRouter's Langfuse broadcast for the same project to avoid
duplicate traces. Adding or removing keys needs a bot restart.

`[suspend]` is removed from config.toml entirely. `default_duration_secs`
lives in `settings.ron`, managed via `/settings → Suspend`.

`[web]` bootstrap fields (`enabled`, `bind_addr`, `public_url`, `session_secret`)
stay in config.toml; `session_ttl` and `mod_check_refresh` live in `settings.ron`,
managed via `/settings → Web · Sessions`.

On first v3 launch, `[twitch]` permission/channel keys and `[aviationstack]`/`[suspend]`/`[web]`
non-secret keys are migrated into `settings.ron` once (sentinel: `$DATA_DIR/.config_migrated_v3`);
subsequent edits to those legacy keys in config.toml are ignored.

Schedules live in `settings.ron`, managed via `/schedules` (mod-gated). Each
row carries name, message, hh:mm interval, optional ISO 8601 date range, optional
HH:MM active-time window, and an enabled toggle. Validation runs on save
(unique non-blank names, parseable interval, paired active-time fields).
Changes apply within ~30s via a `SettingsHandle` change-`Notify` signal that
the `ScheduleCache` sync task subscribes to. On first launch any legacy
`[[schedules]]` in config.toml are migrated into `settings.ron` once
(separate sentinel: `$DATA_DIR/.schedules_migrated_v3`); subsequent edits to
the legacy section are ignored.

AI profile edits (provider, base URL, model, timeout, reasoning_effort,
service_tier) apply live via `SettingsHandle` and `LlmRegistry` — no restart,
same as behavior limits including Aurora reply target/soft/hard character
caps, history caps, memory byte budgets, dreamer schedule, web tools,
emotes, and media. Model fields are plain text inputs — type the
upstream model id exactly.

OAuth credentials + AI API key wrapped in `SecretString` (secrecy crate). Config structs are `Deserialize`-only; do NOT add `Serialize` derive (closes credential-leak via debug dump).

## Data dir

All runtime-persistent files live under `$DATA_DIR` (default `/var/lib/twitch-1337`, Docker sets `/data`). Use `get_data_dir().join(...)` — never hardcode relative paths. Files: `token.ron`, `token-aurora.ron`, `token-grok.ron` (per-mouth, only when that mouth is configured), `pings.ron`, `leaderboard.ron`, `hall_of_shame.ron`, `flights.ron`, `feedback.txt`, `emotes.db` (+ `-wal`/`-shm` when WAL mode) — emote tracker catalog, set audit, usage events, and sibling `emote_meanings` for AI grounding (see ADR-0008). On first open, legacy `emote_tracker.sqlite` (+ wal/shm) is renamed to `emotes.db` when the new name is absent (`MIGRATION(emote-meanings)` bridge — dual gitignore until all envs migrated). Memory tree: `memories/SOUL.md`, `memories/LORE.md`, `memories/users/<id>.md`, `memories/scratch/<slug>.md`, `memories/transcripts/today.md` (line-buffered, rotated nightly), `transcripts/<YYYY-MM-DD>.md`. Prompt templates are *not* here — they're baked into the binary from `crates/data/assets/prompts/` (`include_str!` via `twitch-1337-data`, single source of truth; see `docs/ai-prompts.md`).

Atomic persistence pattern: write tmp + rename. See `ping.rs`, `memory.rs`, `flight_tracker.rs`.

## Embedded data

`twitch-1337-data` owns all baked assets under `crates/data/assets/`:

- `location/` — `plz.csv`, `airports.csv`, `airlines.csv` (refreshed weekly by `data-refresh.yml`)
- `prompts/` — AI prompt templates
- `flight/` — aircraft profiles + OurAirports CSVs for flight-plan DB generation (OurAirports CSVs refreshed by `data-refresh.yml` / `just data-refresh`; `build.rs` bakes from committed files only — no network at compile time)

Lookup airports and PLZ are trimmed and packed in `build.rs` as flat little-endian blobs (`location_airports.bin`, `plz.bin` via `include_bytes!`) to avoid PIE reloc bloat from `&str`-heavy static structs; flight-plan tables still emit generated Rust slices. Airlines CSV and prompts stay `include_str!`. Zero runtime data-file dep for prod image semantics (important for `FROM scratch` musl image); emote meanings live in `$DATA_DIR/emotes.db` (`emote_meanings`).

## Architecture invariants

- Bot scheduling / display use `Europe/Berlin`. `chrono-tz` is built with the full IANA database (no `CHRONO_TZ_TIMEZONE_FILTER`) so Aviationstack airport timezones can reinterpret local wall times stamped as `+00:00`.
- 1337 tracker: messages containing "1337" or "DANKIES" at exactly 13:37:xx. Ignore "supibot", "potatbotat". Dedupe via HashSet (max 10k). Leaderboard = fastest sub-1s PB.
- Broadcast channel capacity 100. Lagging handlers get `RecvError::Lagged`, continue.
- Handlers independent. Errors in one don't crash others (`bail!` on startup only).
- Ping templates: reject control chars on create/edit (CR/LF would split PRIVMSG).
- Aviation client init failure: log + disable `!up`/`!fl`/flight tracker + track commands. Don't abort.
- DHL tracking: coarse tier only — `recipientPostalCode` is never sent, so DHL
  withholds address, sender/recipient and transit cities. Tracking numbers are
  guessable and the bot speaks in public chat; the postal code challenge is
  DHL's own mitigation and the tool stands behind it. No rate limiter: the free
  tier is 250 calls/day at 1 call per 5s and a single channel reaches neither.
- Latency monitor: PING/PONG every 5min, EMA alpha=0.2, shared `Arc<AtomicU32>`. Read by 1337 handler for precise wake-up.
- Flight tracker: `Arc<mpsc::Sender<TrackerCommand>>` from commands to long task. Adaptive poll 30/60/120s based on phase mix. adsb.lol v2; fallback aggregators in memory `reference_adsb_aggregators.md`.
- AI memory (v2): per-user character sheets + chat LORE + bot SOUL as markdown under $DATA_DIR/memories/. Single-loop !ai turn drives the model with write_file/write_scratch/delete_scratch tools (run_agent in the llm crate); the model's final assistant text is sent to chat verbatim. Daily dreamer ritual rewrites files from yesterday's transcript at the dashboard-configured `ai.dreamer.run_at` (Berlin local). Per ADR-0003, Memory (SOUL/LORE/users) and Scratch (`memories/scratch/`) are siblings, not tiers of one thing; see CONTEXT.md. Bodies are byte-capped (SOUL/user 4 KiB, LORE 12 KiB, scratch 2 KiB by default; tunable via the dashboard `AI · Memory` card). `@grok` uses a separate baked prompt (`grok.md`) instead of `system.md` and gets no injected memory bodies: its system message carries a paths-only manifest plus a grok-only `read_memory(path)` tool, allowlisted against the manifest paths. SOUL.md stays dreamer-only for both agents: it's never in grok's manifest, and no chat turn on either alias can write it. `users/<id>.md` ids are never minted by the model: `write_file` refuses an id that is neither the speaker's, in this turn's chat history, nor an existing sheet (the dreamer additionally accepts ids carried by the transcript, whose lines are `HH:MM:SS  login#<user-id>: text`). Without that guard the dreamer invented plausible ids for logins it read in the transcript, producing duplicate sheets under ids no Twitch account owns. Recap is not Memory — paraphrased chat notes live in `$DATA_DIR/chat-recap.ron` and are injected as a system suffix after Memory; see `docs/superpowers/specs/2026-08-25-ai-chat-recap-design.md`.
- AI clients are resolved per call through `LlmRegistry` (`crates/core/src/llm_factory.rs`), keyed by (provider, base_url, key name). Editing a profile's model or switching `active_profile` takes effect on the next turn; changing a provider or base URL builds a new cached client, still no restart. Secrets never leave config.toml — a profile stores a key name, not the key.
- AI mouths speak over Helix `POST /helix/chat/messages` and never join IRC, so their messages come back in over the normal broadcast channel. `EchoGuard` (`crates/core/src/twitch/echo.rs`) keeps them out of the bot's own input. Two mechanisms, both needed: the Helix `message_id` of every sent reply is registered and peeked on the way back (30s TTL, 512 cap, peeked not removed — the transcript tap and the command dispatcher each check the same id), plus a mouth login/user-id identity backstop for when the id doesn't match. History and transcript writes use `is_echo` (both mechanisms); command dispatch, the 1337 tracker, and emote counting use `is_mouth` (identity only) so a viewer can never be filtered by a stale pending id. Neither is dead code. Chat-turn prompts name the mapping via a runtime Konten/Accounts system appendix; a reply parent from a Mouth is labeled `(self)` / the other agent, not an untrusted viewer.
- Scheduled messages: list loaded from `settings.ron` into `ScheduleCache`; the `run_schedule_settings_sync` task subscribes to `SettingsStore`'s change-`Notify` and updates the cache when the dashboard saves. The message handler polls the cache every 30s. On Ctrl+C, main notifies `Arc<Notify>`; children finish in-flight `say()` then exit; main awaits handler with 5s timeout.

## Gotchas

- `twitch_irc::irc!` macro needs standalone `use twitch_irc::irc;` — cannot be in braced `use twitch_irc::{...}`.
- Request/response on broadcast (e.g. PING/PONG): subscribe BEFORE send to avoid race.
- Clippy strict (`-D warnings`) + extra lints in `Cargo.toml [lints.clippy]`. Don't `#[allow]` without one-line reason.
- Log errors with `?error` not `%error` to include backtrace.

## Adding handler

Handlers live in `crates/core/src/twitch/handlers/`. `run_bot` (`crates/core/src/lib.rs`) calls `spawn_handlers` once; that factory owns every `JoinHandle` and the shutdown `tokio::select!`. Each handler is generic over `<T: Transport, L: LoginCredentials>` so integration tests can swap in a fake transport. Shared deps (clock, data_dir, llm, aviation) come from `Services` in `crates/core/src/lib.rs`, moved into `SpawnDeps`.

```rust
// crates/core/src/twitch/handlers/my_handler.rs
pub async fn run_my_handler<T, L>(
    broadcast_tx: broadcast::Sender<ServerMessage>,
    client: Arc<TwitchIRCClient<T, L>>,
    /* other deps from SpawnDeps as needed */
) where T: Transport, L: LoginCredentials { /* subscribe, filter, act */ }

// crates/core/src/twitch/handlers/spawn.rs: add the dep to SpawnDeps,
// spawn into HandlerSet, add an exit arm to the final tokio::select!
```

Integration-testable via `TestBotBuilder` in `crates/core/tests/common/`.

## Key constants

`crates/core/src/twitch/handlers/tracker_1337.rs`: `TARGET_HOUR=13`, `TARGET_MINUTE=37`, `MAX_USERS=10_000`.

`crates/core/src/twitch/handlers/latency.rs`: `LATENCY_PING_INTERVAL=300s`, `LATENCY_EMA_ALPHA=0.2`.

`crates/core/src/aviation/tracker/mod.rs`: `MAX_TRACKED_FLIGHTS=12`, `MAX_FLIGHTS_PER_USER=3`, `TRACKING_LOST_THRESHOLD=300s`, `TRACKING_LOST_REMOVAL=1800s`, `POLL_FAST/NORMAL/SLOW=30/60/120s`.

`crates/core/src/aviation/tracker/debug_journal.rs`: `DEBUG_JOURNAL_KEEP_FILES=30` (daily JSONL files retained under `$DATA_DIR/flight-tracker-debug/`; older pruned on tracker startup + date-rollover).

## Binary / Docker

Musl static build: `cargo build --release --target x86_64-unknown-linux-musl` (~29 MB stripped; works on Alpine/busybox, `FROM scratch` image). `[profile.release] strip = "symbols"` keeps the image ≈ binary size. rustls + **ring** only (no OpenSSL, no aws-lc — see ADR-0007); call `install_crypto_provider()` before any TLS client. Multi-stage Dockerfile with cargo-chef.

Verify static: `ldd target/x86_64-unknown-linux-musl/release/twitch-1337` → "statically linked".

## Agent skills

### Issue tracker

Issues live in GitHub Issues (`Chronophylos/twitch-1337`), managed via the `gh` CLI. External PRs are **not** a triage request surface. See `docs/agents/issue-tracker.md`.

### Triage labels

Five canonical triage roles map to the repo's existing `status:` prefix (`status:needs-triage`, `status:needs-info`, `status:ready-for-agent`, `status:ready-for-human`, `status:wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
