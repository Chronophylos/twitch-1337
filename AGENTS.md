# AGENTS.md

Guide for coding agents working in this repo. Claude Code loads it through `CLAUDE.md` (`@AGENTS.md`); edit this file, not that one.

## Project

Rust Twitch IRC bot. Features: 1337 tracker (13:37 Berlin), leaderboard (`!lb`), hall of shame (`!sb`, `!pw`), ping system (`!p`, `!<ping>`), scheduled messages (config.toml), latency monitor, AI (`!ai`, provider profiles managed at `/ai-profiles`), flight tracker (`!track`, `!untrack`, `!flight`, `!flights`), aviation lookups (`!up`, `!fl`), shipment tracking (AI tool, DHL), network lookups (AI tool, RDAP + optional ipinfo), game servers (`!servers [name]`, AMP), joke stock picks (`!aktien`/`!stocks`, Finnhub), feedback (`!fb`), spoiler check (`!spoiler`, decision model), alerts (earthquake + solar flare + Earth-directed CME monitors, passive, configured at `/settings → Alerts`), version (`!v`), help (`!help [befehl]`, aliases `!commands`/`!hilfe`; one catalog entry, or a link to the public `/commands` page when the dashboard is on; cooldown `cooldowns.help`), public command list for chat-client autocomplete (`/commands.json`, see `docs/commands-json.md`, and the AI `list_commands` tool; every `Command` must supply `description`/`usage`/`permission`, aliases go in `aliases()`; the catalog's `lookup` is the one trigger/alias resolver).

Single persistent IRC connection, broadcast channel routes to independent handler tasks.

## Crate layout

| Crate | Role |
|---|---|
| `crates/core` | Bot logic: IRC handlers, aviation, AI memory, pings, schedules, config/settings types |
| `crates/data` (`twitch-1337-data`) | Baked assets: location CSVs, emotes, prompts, flight-plan DB generation |
| `crates/random-flight` | In-tree geo + route selection + flight-plan math (depends on `twitch-1337-data`) |
| `crates/settings-derive` (`twitch-1337-settings-derive`) | `#[derive(Knobs)]` proc macro for the settings knob table; used only by core |
| `crates/llm` | LLM backends, agent loop, tool calling |
| `crates/web` | Axum dashboard, routes, templates, auth |
| `crates/twitch-1337` | Binary entrypoint (`main.rs`), builds `Services`, calls `run_bot` |

Where things go:

- New IRC handler → `crates/core/src/twitch/handlers/`, spawned from `spawn.rs::spawn_handlers` (see § Adding handler)
- New chat command → handler + `commands.rs` registration pattern
- New dashboard route/page → `crates/web/src/`
- LLM provider/tool changes → `crates/llm/`
- Baked CSV/TOML/prompt assets → `crates/data/assets/` (exported from `twitch-1337-data`)
- Flight-plan geo/selection math → `crates/random-flight/`

Shared deps flow through `Services` — don't reach across crate boundaries with ad-hoc globals.

## Commands

```bash
# Dev
cargo build | cargo run | cargo check
RUST_LOG=debug cargo run   # all IRC + handler activity
RUST_LOG=trace cargo run   # every ServerMessage

# CI gate — run in order before claiming Rust work done; fix failures before committing
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace   # `cargo test --workspace` if nextest is missing
cargo deny check                # optional locally; run when touching deps, CI fails on advisories/bans/sources

# Deploy (Justfile, podman local → $DEPLOY_HOST over SSH, set in .deploy.env)
just build | just push | just restart | just deploy
```

Local dev: prefer `just` recipes over hand-rolled cargo invocations.

| Command | Use |
|---|---|
| `just dev` | Full bot + dashboard, `DATA_DIR=./dev-data`, debug logging, `dev-login` OAuth bypass |
| `just dev-web` | Dashboard only (no IRC) on `:8761` — ideal for worktree UI work |
| `just dev-web-watch` | Same with `cargo watch` + livereload |
| `just e2e` | Browser e2e (chromedriver + version-matched Chrome) |

In a worktree run `just dev-web bind=127.0.0.1:8762` (or another port) beside the main checkout; both may share `./dev-data` (atomic tmp+rename, last writer wins). Open `/_dev/login` once to mint a mod session (`dev-login` feature; production builds omit it).

## Agent workflow

**Branches and worktrees.** Never commit feature work on `main`, never push `main`. Every change lands via PR from a new branch, worked in a git worktree under `.worktrees/` (gitignored; check with `git check-ignore -q .worktrees`):

```bash
git fetch origin main
git worktree add .worktrees/fix-pings-data-dir -b fix/pings-data-dir origin/main
```

Name the folder after the branch. Tiny follow-ups in an already-open worktree branch are fine; anything new gets a worktree.

**Commits.** Commit on your own after each logical task, fix, or review item — one commit each, `cargo fmt` changes included in the same commit. [Conventional Commits](https://www.conventionalcommits.org/) `type(scope): subject` (`feat`, `fix`, `refactor`, `test`, `docs`, `chore`, `ci`, `perf`), written as a **feral subject, serious body** bit:

- **Subject**: maximum unhinged gen-z / brainrot. Lowercase, slang pile-up, memes, sleep-deprived group chat that just discovered git. If a sober engineer would write it, rewrite it dumber. Emoji encouraged, optional, may replace words (`died` → 💀, `fire` → 🔥, 🧢).
- **Body**: crisp professional engineering prose — what changed, why, trade-offs, complete sentences. Zero slang; no emoji unless quoting user-facing copy.

```bash
git commit -m "$(cat <<'EOF'
fix(chat): bestie the websocket literally ate shit and 💀

The handler assumed every inbound message included a user ID. Anonymous
viewer messages omitted the field and caused a panic in the parser.
Guard the optional field and skip processing when absent.
EOF
)"
```

More subjects: `feat(overlay): we cooked so hard the badge renderer is giving sigma grindset fr fr 🔥`, `refactor(db): yeeted duplicate query into one function no thoughts head empty 🧠❌`, `chore(deps): bumped tokio bc living in 2023 is literally not the vibe anymore`, `fix(ping): templates were catching bodies with the cr lf split no 🧢`. Too tame, rewrite: `feat(tracker): add hall of shame commands`, `fix(auth): improve token refresh handling`, anything a changelog bot would write.

No `--no-verify`, no secrets, no amend unless explicitly allowed.

**Pull requests.** Run the CI gate, push the branch (`git push -u origin HEAD`), `gh pr create` with **Summary** and **Test plan** sections. The title is a commit subject in the same brainrot style — squash merge uses it as the commit message. Labels via `gh pr edit`: exactly one `type:` (`feat`, `bug`, `refactor`, `security`, `docs`; `security` supersedes `bug`) plus any matching `system:` (`ai`, `aviation`, `ping`, `tracker`, `schedule`, `irc`, `config`); full tables in [CONTRIBUTING.md](CONTRIBUTING.md). Merge with `gh pr merge --squash` once checks are green (§ CI & branch policy).

**Never commit** (all gitignored — never force-add; check `git status` before committing): `.env*`, local `config.toml`, `token*.ron` and other runtime `*.ron`, `emotes.db*`, `./dev-data/`, `./prod-data/`, `/data/`, `.worktrees/`, new files under `docs/superpowers/plans/`.

**Plans are scratch.** A working plan may live under `docs/superpowers/plans/` while implementing; keep it untracked and delete it once the feature lands or the PR is open. Durable decisions go to `docs/superpowers/specs/` or `docs/adr/`. Plans already on `main` stay.

## CI & branch policy

`main` is protected by the `main` repository ruleset (no bypass actors, admins included). Direct pushes rejected — use a PR.
Linear history required, force-push + delete blocked, squash-only, conversations must resolve, no approving review required.

**Branch naming:** `feature/` features, `spec/` specs, `fix/` bug fixes, `refactor/` refactors, `build/` CI / dependencies / Docker. Slug after slash, kebab-case.

**Required status checks (10, must all pass to merge):**

| Check | Workflow | Purpose |
|---|---|---|
| `fmt` | ci.yml | cargo fmt --check |
| `clippy` | ci.yml | cargo clippy --workspace --all-targets -- -D warnings, then `cargo doc` (broken intra-doc links) and `cargo machete` (unused deps) |
| `test` | ci.yml | cargo llvm-cov nextest --workspace (lcov artifact; no threshold gate) |
| `e2e` | ci.yml | web crate WebDriver tests (`--features e2e`, setup-chrome) |
| `cargo deny` | ci.yml | `deny.toml`: RustSec advisories, banned crates (aws-lc-sys), allowed sources; also weekly on schedule |
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

**Native GitHub security (repo settings):** repo is public. Secret scanning, push
protection, private vulnerability reporting (see `SECURITY.md`) and Dependabot
alerts + security updates are enabled. hadolint/trivy/zizmor upload SARIF to the
Security tab; hadolint and trivy also fail their job on findings, so they still
block merges. gitleaks is the secret gate in CI; known false positives are listed
by fingerprint in `.gitleaksignore`. Workflow runs from outside contributors need
approval.

**Dependabot** (`.github/dependabot.yml`): weekly PRs for `cargo`, `github-actions`,
`docker`. Cargo minor+patch grouped as `rust-minor-patch`; GitHub Actions minor+patch
grouped as `actions-minor-patch`. Docker ecosystem bumps both tag AND sha256 digest in
Dockerfile.

**Action pinning:** every `uses:` is pinned to a full-length **commit SHA** with a
version comment (`@<sha> # vX.Y.Z`); the repo setting *Require actions to be pinned
to a full-length commit SHA* is on, so a tag or branch ref fails the workflow.
Dependabot bumps SHA pins. Tool installers take the tool as an input rather than a
ref (`taiki-e/install-action@<sha>` + `with: tool: nextest`). Trivy stays pinned
for its own reason too (Mar 2026 supply-chain incident).

**Typical PR flow:**
1. branch → commit → push → `gh pr create`
2. wait for 10 checks green; rebase on main if `strict` blocks merge
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
5. Rollback: `ssh "$DEPLOY_HOST"`, edit the compose file to pin
   `:b<N-1>`, `docker compose up -d`. The previous image is still
   present until the weekly GHCR retention prune (keeps last 30).

There are no `vX.Y.Z` tags, no GitHub Releases, no `CHANGELOG.md`. `git
log` on `main` is the release history. The workspace version stays at
`0.0.0` permanently — never bump it. When changing Docker/CI build args,
preserve the `GIT_SHA` / build-number injection (Justfile `build` recipe).

**When `cargo deny` fails:**
- Check open Dependabot PRs first (weekly); a bump may already be queued.
- Transitive vulns: `cargo tree -i <crate>` to find the parent; bump the parent.
- If two majors of one crate coexist in Cargo.lock (e.g. rustls-webpki 0.101 + 0.103),
  both must be resolved — usually by bumping the dep pulling in the old major.
- Last resort: add the id to `[advisories] ignore` in `deny.toml` with a `reason`.
- New git dependency: add its repo to `[sources] allow-git` in `deny.toml`.

**When Dependabot cargo PRs break `clippy` or `test`:**
- Breaking-API bump; adapt code on the dependabot branch and force-push
  (`git push origin dependabot/cargo/<name>-<ver>`). CI reruns, then merge.
- Example: rand 0.10 moved `.random::<T>()` from `Rng` to `RngExt` — generic bounds
  become `impl rand::RngExt`.

## Config

`config.toml` (copy `config.toml.example`). Bootstrap sections only: `[twitch]` (secrets + channel/username), `[ai]` (`[ai.keys]` named credentials only, optional), `[aviationstack]` (api_key only, optional), `[dhl]` (api_key only, optional), `[ipinfo]` (token only, optional), `[amp]` (url/username/password, optional), `[finnhub]` (api_key only, optional), `[langfuse]` (public/secret keys, optional), `[web]` (bootstrap fields only). Schema + defaults in `config.toml.example` — treat as source of truth. Everything else lives in `settings.ron` managed via the dashboard.

Adding a knob: a secret, or something that must exist before the dashboard, goes in `config.toml.example` + loader; anything runtime-tunable goes in the settings schema + a dashboard card. Never both. Legacy hoisted keys were migrated once via sentinels (`.ai_migrated_v2`, `.config_migrated_v3`, `.schedules_migrated_v3`); edits to migrated legacy keys are ignored.

Runtime knobs are declared once, on the effective struct in `crates/core/src/settings/`, via `#[derive(Knobs)]` (crate `crates/settings-derive`, runtime in `settings/knob.rs`). The derive generates the sparse `*Overrides` mirror, resolve, merge, dotted-path form parsing, and the `KnobMeta` table. A new field needs: the struct field (plus `#[knob(min = .., max = ..)]` for a constant range, `#[knob(restart)]` if it is read only at startup), its default, and a row in the card template calling a `_macros.html` row macro with `self.knob("<dotted.path>")`. The form `name` is the dotted path; structs with bool fields render a hidden `<path>.__shown` sentinel. Cross-field and semantic checks go in `validation.rs`. A route test fails if a form knob has no template row or a row posts a name the table does not know.

`[twitch]` holds OAuth credentials (`refresh_token`, `client_id`, `client_secret`), `channel`, `username`, and `owner` (optional Twitch user ID with full dashboard access). `owner` is bootstrap-only and lives here because it gates the settings page — it must not be editable from the thing it controls. Permission lists (`hidden_admins`, `viewer_allowlist`), the `admin_channel` pointer, and `expected_latency` live in `settings.ron`, managed via `/settings → Twitch · Permissions` and `Twitch · Channels`. `admin_channel` changes require a bot restart. Mods always pass the dashboard auth check; `viewer_allowlist` grants read-only access to non-mods.

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
lives in `settings.ron`: `ai.aurora.persona_name` (default `"Aurora"`) and
`ai.grok.persona_name` (default `"Grok"`). Aurora's chat-reply length
(`ai.aurora.reply_target_chars` / `reply_soft_chars` / `reply_hard_chars`,
defaults 480/490/490) is live-tunable on `/settings → Agents · Aurora`; Grok
keeps a baked 480/500 prompt and a 490/490 reword-clamp.

`[ai]` carries only credentials: an `[ai.keys]` table of named keys. A profile
stores a key *name*, never the secret, so switching providers from the dashboard
never touches config.toml. A leftover `[ai].api_key` is ignored (startup warn);
move it into `[ai.keys]` and set the profile's key name. Provider, base URL,
model, timeout, reasoning effort and service tier live in **profiles** — a
named list (`ai.profiles`) CRUD'd at `/ai-profiles` (owner only — a profile
carries `base_url`/`key`, the same credential surface `/settings` keeps
owner-only). `ai.aurora.profile` names the profile `!ai` uses; Aurora, Grok,
Dreamer, and Media each have a required profile dropdown at `/settings`.
Recap's profile dropdown is optional, with empty meaning off. Agents do not
override model / effort / tier on top of a profile — pick a different profile
instead. Dreamer and media keep their own timeout knobs. Every chat AI client is
resolved per call through `LlmRegistry` (`crates/core/src/llm_factory.rs`), so
changing an agent's profile selection or editing a profile applies on the next
turn — no restart. Each profile declares an
`AiProvider` (`open_router`, `open_ai`, `open_ai_compatible`, `ollama`) whose
`caps()` table decides which `reasoning_effort` values it accepts and whether
`service_tier` applies — see `crates/core/src/settings/provider.rs`. Memory
caps, dreamer schedule, web/emote/media tool toggles, history caps, and every
other runtime knob live in the dashboard (`/settings`, owner only) and
persist to `$DATA_DIR/settings.ron`. A leftover legacy `[ai]` table in
`config.toml` is ignored — runtime AI knobs are dashboard-managed only.

`[aviationstack]` keeps only `api_key` in config.toml. `enabled`, `base_url`,
and `timeout_secs` live in `settings.ron` (restart-required for connection
changes), managed via `/settings → Aviationstack`.

`!spoiler <url> [game]` is configured by the `spoiler` section (`profile`,
`default_game`, `confidence_floor`) and `cooldowns.spoiler` in `settings.ron`,
managed via `/settings`. AI profiles have a `kind` (Chat by default, or
Decision, ADR-0010). Decision profiles are OpenRouter-only and skip
`reasoning_effort` / `service_tier`. They are called through `llm::decision`
per call, outside `LlmRegistry`; only the key comes from
`LlmRegistry::secret_for`. A profile without a model or without a key in `[ai.keys]`
makes the command answer "nicht konfiguriert".

Alert monitors live entirely in `settings.ron` — there is no config.toml
section and no API key, because all three feeds (EMSC seismicportal.eu, NOAA
SWPC, and CCMC DONKI WSA-ENLIL) are free and keyless. `alerts.quake` and
`alerts.solar` each carry `enabled` + `poll_secs`; quake adds
`min_mag_tenths` (integer tenths, so `Settings` keeps its `Eq` derive) and
solar adds `min_class`. `alerts.cme` has `enabled` + `poll_secs` only —
Earth-directed means a parseable shock-arrival time from the ENLIL forecast,
not flare class. The shared `alerts.max_age_secs` is the catch-up cutoff.
All apply live — no restart. The Germany bounding box is a const in
`crates/core/src/alerts/quake.rs`, deliberately not a setting.

`[dhl]` holds only `api_key` and is optional. Its presence enables Aurora's
`track_shipment` tool; absent, the tool is never registered with the model.
There is no `settings.ron` section and no dashboard toggle — AI tools have no
per-tool toggles, unlike Aviationstack, which backs commands. Adding or
removing the key needs a bot restart.

`[ipinfo]` holds only `token` (`SecretString`) and is optional; an empty
token fails startup validation. Aurora's `lookup_network` tool (IP, AS
number and domain ownership) runs on keyless RDAP via `rdap.org` and is
registered on every Aurora turn whether or not the section exists; Grok never
gets it. With a token, IP lookups also carry ipinfo's city/region/country,
ASN/org and hostname; without it they return RDAP data only. Non-public input
(private, loopback, link-local, CGNAT, multicast, documentation and reserved
ranges, private-use ASNs, special-use TLDs) is refused before any request,
using the same `util::net::is_non_public_ip` classifier as the web fetcher's
SSRF guard. No `settings.ron` section, no dashboard toggle. Adding or removing
the section needs a bot restart.

`[amp]` holds `url`, `username`, `password` (`SecretString`) for the homelab
CubeCoders AMP controller and is optional. Its presence registers `!servers`;
absent, the command does not exist. `url` must be `http://` or `https://`
(validated at startup). The per-user cooldown is `cooldowns.servers` in
`settings.ron` (default 30s, `/settings → Cooldowns`). Use a dedicated
AMP user whose role allows only `Instances.*` (all else denied). The bot reads,
never starts or stops. The bot reaches the controller over the LAN,
no ZeroTier needed. Game instances need their own
login through the ADS proxy (`ADSModule/Servers/{id}/API/...`); the controller
session is rejected there. Adding or removing the section needs a bot restart.

`[finnhub]` holds only `api_key` (`SecretString`, free tier) and is optional.
Its presence registers `!aktien` (alias `!stocks`); absent, the command does
not exist. The command quotes a baked ticker pool in
`crates/core/src/commands/aktien.rs` (US listings only, since the free tier
quotes nothing else, so DAX names go through their ADRs) and caches the pick
for 5 minutes (an error reply for 45 seconds), keeping it far below the 60 calls/min limit. The per-user
cooldown is `cooldowns.aktien` in `settings.ron` (default 60s,
`/settings → Cooldowns`). Adding or removing the section needs a bot restart.

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
managed via `/settings → Web · Sessions`. Every `mod_check_refresh` a Mod
session is re-verified with the moderator's own Twitch refresh token (kept in
`$DATA_DIR/sessions.ron`), never the bot token: Twitch's moderators endpoint
only answers the broadcaster's token, and the bot is a different account. The
recheck fails closed: if Twitch errors or the token is rejected, the session is
dropped and the user logs in again.

Schedules live in `settings.ron`, managed via `/schedules` (mod-gated). Each
row carries name, message, an interval (hours + minutes inputs), optional ISO 8601 date range, optional
HH:MM active-time window, an optional ping name (fired as a second
message after the schedule message, respects the ping's cooldown, a missing
ping just warns and skips), and an enabled toggle. Validation runs on save
(unique non-blank names, parseable interval, paired active-time fields).
Changes apply within ~30s via a `SettingsHandle` change-`Notify` signal that
the `ScheduleCache` sync task subscribes to. A leftover legacy `[[schedules]]`
in config.toml is ignored.

AI profile edits (provider, base URL, model, timeout, reasoning_effort,
service_tier) apply live via `SettingsHandle` and `LlmRegistry` — no restart,
same as behavior limits including Aurora reply target/soft/hard character
caps, history caps, memory byte budgets, dreamer schedule, web tools,
emotes, and media. Model fields are plain text inputs — type the
upstream model id exactly.

OAuth credentials + AI API key wrapped in `SecretString` (secrecy crate). Config structs are `Deserialize`-only; do NOT add `Serialize` derive (closes credential-leak via debug dump).

## Data dir

All runtime-persistent files live under `$DATA_DIR` (default `/var/lib/twitch-1337`, Docker sets `/data`). Use `get_data_dir().join(...)` — never hardcode relative paths. Files: `token.ron`, `token-aurora.ron`, `token-grok.ron` (per-mouth, only when that mouth is configured), `pings.ron`, `leaderboard.ron`, `hall_of_shame.ron`, `flights.ron`, `alerts.ron`, `feedback.txt`, `emotes.db` (+ `-wal`/`-shm` when WAL mode) — emote tracker catalog, set audit, usage events, and sibling `emote_meanings` for AI grounding (see ADR-0008). Memory tree: `memories/SOUL.md`, `memories/LORE.md`, `memories/users/<id>.md`, `memories/scratch/<slug>.md`, `memories/transcripts/today.md` (line-buffered, rotated nightly), `transcripts/<YYYY-MM-DD>.md`. Prompt templates are *not* here — they're baked into the binary from `crates/data/assets/prompts/` (`include_str!` via `twitch-1337-data`, single source of truth). The chat turn is assembled from `preset.ron`: an ordered list of named blocks, each with a slot (System/User/Assistant) and a minijinja body that decides its own inclusion. See `docs/ai-prompts.md`.

Atomic persistence pattern: write tmp + rename. See `ping.rs`, `memory.rs`, `flight_tracker.rs`. Files holding credentials (`token*.ron`, `sessions.ron`) use `util::persist::atomic_save_ron_private_async`, which writes them `0600` on unix.

## Embedded data

`twitch-1337-data` owns all baked assets under `crates/data/assets/`:

- `location/` — `plz.csv`, `airports.csv`, `airlines.csv` (refreshed weekly by `data-refresh.yml`)
- `prompts/` — AI prompt templates
- `flight/` — aircraft profiles + OurAirports CSVs for flight-plan DB generation (OurAirports CSVs refreshed by `data-refresh.yml` / `just data-refresh`; `build.rs` bakes from committed files only — no network at compile time)

Lookup airports and PLZ are trimmed and packed in `build.rs` as flat little-endian blobs (`location_airports.bin`, `plz.bin` via `include_bytes!`) to avoid PIE reloc bloat from `&str`-heavy static structs; flight-plan tables still emit generated Rust slices. Airlines CSV and prompts stay `include_str!`. Zero runtime data-file dep for prod image semantics (important for `FROM scratch` musl image); emote meanings live in `$DATA_DIR/emotes.db` (`emote_meanings`).

## Architecture invariants

- Bot scheduling / display use `Europe/Berlin` (`util::BERLIN`, `util::to_berlin`). Time is `jiff` throughout; it is built with `tzdb-bundle-always` so the musl `FROM scratch` image carries the full IANA database and Aviationstack airport timezones can reinterpret local wall times stamped as `+00:00`. `chrono`/`chrono-tz`/`humantime` were dropped as unmaintained (#609); schedule `every` still reads humantime's `1day` spelling.
- 1337 tracker: messages containing "1337" or "DANKIES" at exactly 13:37:xx. Ignore "supibot", "potatbotat". Dedupe via HashSet (max 10k). Leaderboard = fastest sub-1s PB.
- Broadcast channel capacity 100. Lagging handlers get `RecvError::Lagged`, continue.
- Fault is fail-fast (ADR-0012): any unexpected task exit or panic — one command task's included — ends the whole runtime and the bot restarts. Operational failures (I/O, upstream APIs) are handled inside their task and never panic; `bail!` aborts only at startup.
- Ping templates: reject control chars on create/edit (CR/LF would split PRIVMSG).
- Aviation client init failure: log + disable `!up`/`!fl`/flight tracker + track commands. Don't abort.
- DHL tracking: coarse tier only — `recipientPostalCode` is never sent, so DHL
  withholds address, sender/recipient and transit cities. Tracking numbers are
  guessable and the bot speaks in public chat; the postal code challenge is
  DHL's own mitigation and the tool stands behind it. No rate limiter: the free
  tier is 250 calls/day at 1 call per 5s and a single channel reaches neither.
- Latency monitor: PING/PONG every 5min, EMA alpha=0.2, shared `Arc<AtomicU32>`. Read by 1337 handler for precise wake-up.
- Flight tracker: `Arc<mpsc::Sender<TrackerCommand>>` from commands to long task. Adaptive poll 30/60/120s based on phase mix. adsb.lol v2; fallback aggregators in memory `reference_adsb_aggregators.md`.
- AI memory (v2): per-user character sheets + chat LORE + bot SOUL as markdown under $DATA_DIR/memories/. Single-loop !ai turn drives the model with write_file/write_scratch/delete_scratch tools (run_agent in the llm crate); the model's final assistant text is sent to chat verbatim. Daily dreamer ritual rewrites files from yesterday's transcript at the dashboard-configured `ai.dreamer.run_at` (Berlin local). Per ADR-0003, Memory (SOUL/LORE/users) and Scratch (`memories/scratch/`) are siblings, not tiers of one thing; see CONTEXT.md. Bodies are byte-capped (SOUL/user 4 KiB, LORE 12 KiB, scratch 2 KiB by default; tunable via the dashboard `AI · Memory` card). `@grok` uses a separate baked head (`grok/core.md`) instead of Aurora's (`aurora/core.md`) and gets no injected memory bodies: its system message carries a paths-only manifest plus a grok-only `read_memory(path)` tool, allowlisted against the manifest paths. SOUL.md stays dreamer-only for both agents: it's never in grok's manifest, and no chat turn on either alias can write it. `users/<id>.md` ids are never minted by the model: `write_file` refuses an id that is neither the speaker's, in this turn's chat history, nor an existing sheet (the dreamer additionally accepts ids carried by the transcript, whose lines are `HH:MM:SS  login#<user-id>: text`). Without that guard the dreamer invented plausible ids for logins it read in the transcript, producing duplicate sheets under ids no Twitch account owns. Recap is not Memory — paraphrased chat notes live in `$DATA_DIR/chat-recap.ron` and are injected as a system suffix after Memory; see `docs/superpowers/specs/2026-08-25-ai-chat-recap-design.md`.
- AI clients are resolved per call through `LlmRegistry` (`crates/core/src/llm_factory.rs`), keyed by (provider, base_url, key name). Editing a profile's model or changing an agent's profile selection takes effect on the next turn; changing a provider or base URL builds a new cached client, still no restart. Secrets never leave config.toml — a profile stores a key name, not the key.
- AI mouths speak over Helix `POST /helix/chat/messages` and never join IRC, so their messages come back in over the normal broadcast channel. `EchoGuard` (`crates/core/src/twitch/echo.rs`) keeps them out of the bot's own input. Two mechanisms, both needed: the Helix `message_id` of every sent reply is registered and peeked on the way back (30s TTL, 512 cap, peeked not removed — the transcript tap and the command dispatcher each check the same id), plus a mouth login/user-id identity backstop for when the id doesn't match. History and transcript writes use `is_echo` (both mechanisms); command dispatch, the 1337 tracker, and emote counting use `is_mouth` (identity only) so a viewer can never be filtered by a stale pending id. Neither is dead code. Chat-turn prompts name the mapping via a runtime Konten/Accounts system appendix; a reply parent from a Mouth is labeled `(self)` / the other agent, not an untrusted viewer.
- Scheduled messages: list loaded from `settings.ron` into `ScheduleCache`; the `run_schedule_settings_sync` task subscribes to `SettingsStore`'s change-`Notify` and updates the cache when the dashboard saves. The message handler polls the cache every 30s. On Ctrl+C, main notifies `Arc<Notify>`; children finish in-flight `say()` then exit; main awaits handler with 5s timeout.
- Alerts: one handler task, three feeds, wall-clock due-gates per feed so
  `poll_secs` edits apply within a tick. Seen ids persist to
  `$DATA_DIR/alerts.ron` (FIFO, 500/feed, including `cme`). An unseen event older than
  `max_age_secs` is marked seen **without** being announced — that is what
  keeps a post-outage backlog out of chat. Feed failures skip the tick;
  there is no retry, because the next poll is the retry.

## Gotchas

- `twitch_irc::irc!` macro needs standalone `use twitch_irc::irc;` — cannot be in braced `use twitch_irc::{...}`.
- Request/response on broadcast (e.g. PING/PONG): subscribe BEFORE send to avoid race.
- Clippy strict (`-D warnings`) + extra lints in `Cargo.toml [workspace.lints.clippy]`. Suppress with `#[expect(lint, reason = "...")]` (lint-enforced; stale ones fail). Item only dead outside tests: `#[cfg_attr(not(test), expect(...))]`. Plain `#![allow]` only in test helpers shared across test binaries.
- Panics are lint-denied outside tests (ADR-0012 rules 9–10): `unwrap_used`, `expect_used`, `panic`, `map_err_ignore`, `let_underscore_must_use`, `let_underscore_future`. A surviving site needs `#[expect(clippy::…, reason = "<invariant>")]`, and only for an invariant the surrounding code proves or baked/compile-time data — never external input, I/O, or settings values. Sends to a receiver that may have hung up use `tx.send(x).ok();`. Sync mutexes are `parking_lot` (no poisoning), so `.lock()` needs no `expect`.
- Errors follow ADR-0012. Rejection / operational failure / Bug; typed `thiserror` enum when a caller branches on the cause, `eyre` + `wrap_err` otherwise. `Display` is for logs — Chat, Dashboard, and Model words come from per-Surface `match`es, never from `to_string()` or string matching. Keep `#[source]` chains; no `map_err(|_| …)` on operational causes.
- Log an error once, where it is handled, with `?error` not `%error` (full chain + backtrace). Deliberate ignores match the benign case (`ErrorKind::NotFound`); a bare `.ok()` only on sends to a receiver that may have hung up.
- Signals (logs, traces, metrics) follow `docs/observability.md` and ADR-0013. `info` is a state change and never scales with chat volume. `#[instrument(skip_all, fields(...))]` only. No span on a run-forever loop. RED metrics come from spans, and explicit instruments are for state only. Attribute names come from the doc's name table, semconv first. "Telemetry" means aircraft state only.

## Adding handler

Handlers live in `crates/core/src/twitch/handlers/`. `run_bot` (`crates/core/src/lib.rs`) calls `spawn_handlers` once; that factory owns every `JoinHandle` and the shutdown `tokio::select!`. Each handler is generic over `<T: Transport, L: LoginCredentials>` so integration tests can swap in a fake transport. Every task receives `Arc<Services>` (plus `client` and the broadcast receiver) and destructures the handles it uses at its own top — add new shared state to `Services` in `crates/core/src/lib.rs`, never to a per-handler relay struct. The only struct still threaded through is `CommandConfig` (`commands.rs`), and only the generic-command dispatcher receives it.

```rust
// crates/core/src/twitch/handlers/my_handler.rs
pub async fn run_my_handler<T, L>(
    services: Arc<Services>,
    client: Arc<TwitchIRCClient<T, L>>,
    broadcast_tx: broadcast::Sender<ServerMessage>,
) where T: Transport, L: LoginCredentials {
    let settings = services.settings.clone();
    /* subscribe, filter, act */
}

// crates/core/src/twitch/handlers/spawn.rs: tokio::spawn(run_my_handler(
//     services.clone(), client.clone(), broadcast_tx.clone(),
// )), add the JoinHandle to HandlerSet, add an exit arm to await_shutdown's select!
```

## Testing

- Bot integration: `TestBotBuilder` in `crates/core/tests/common/test_bot.rs`; handlers are generic over `Transport` + `LoginCredentials` for fake transports. Core itests are grouped into `[[test]]` targets (`ai`, `aviation`, `tracker`, `irc`, `autotests = false`); add a file under `crates/core/tests/<group>/` and `mod` it from that group's `main.rs`.
- External HTTP (aviation, etc.): `wiremock`. Flight tracker tests: `crates/core/tests/aviation/flight_tracker.rs`.
- Web: route tests in `crates/web/tests/{auth,routes}/`, helpers in `tests/helpers/mod.rs`; `dev-login` feature bypasses auth.
- E2e: `cargo nextest run -p twitch-1337-web --features e2e` (or `just e2e`); needs chromedriver + version-matched Chrome.
- CI `test` runs `cargo llvm-cov nextest --workspace`; locally plain `cargo nextest run --workspace` unless you need coverage.

## Key constants

`crates/core/src/twitch/handlers/tracker_1337.rs`: `TARGET_HOUR=13`, `TARGET_MINUTE=37`, `MAX_USERS=10_000`.

`crates/core/src/twitch/handlers/latency.rs`: `LATENCY_PING_INTERVAL=300s`, `LATENCY_EMA_ALPHA=0.2`.

`crates/core/src/aviation/tracker/mod.rs`: `MAX_TRACKED_FLIGHTS=12`, `MAX_FLIGHTS_PER_USER=3`, `MAX_PENDING_UNKNOWN_FLIGHTS=6` / `MAX_PENDING_UNKNOWN_PER_USER=1` (flights pending on ADS-B with no Aviationstack schedule hold a slot up to 24h), `TRACKING_LOST_THRESHOLD=300s`, `TRACKING_LOST_REMOVAL=1800s`, `POLL_FAST/NORMAL/SLOW=30/60/120s`.

`crates/core/src/aviation/tracker/debug_journal.rs`: `DEBUG_JOURNAL_KEEP_FILES=30` (daily JSONL files retained under `$DATA_DIR/flight-tracker-debug/`; older pruned on tracker startup + date-rollover).

`crates/core/src/util/log_ring.rs`: `LOG_RING_CAPACITY=2000` (INFO+ tracing events kept in memory for the owner-only `/logs` page; values clamped to 2048 chars; a restart clears it, stdout stays the record).

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
