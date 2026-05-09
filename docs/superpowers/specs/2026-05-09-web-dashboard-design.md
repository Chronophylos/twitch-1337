# Web Dashboard

**Status:** Spec
**Date:** 2026-05-09
**Author:** Nikolai Zimmermann (with Claude)

## Problem

The bot exposes ping templates, AI memory (SOUL/LORE/per-user/state notes), leaderboard, flight tracker, and other state purely through chat commands and on-disk files. Editing AI memory or ping templates today means SSHing to `docker.homelab`, finding the right file under `$DATA_DIR/memories/` or `pings.ron`, and editing it by hand — risking corrupted RON, blown byte caps, or torn writes against the dreamer ritual.

Operating the bot from a browser would be faster, safer, and (with Cloudflare Tunnel + Twitch OAuth) reachable from anywhere.

## Goal

Add a small web dashboard, embedded in the same binary, that gives moderators of `twitch.channel` (and `twitch.hidden_admins`) a curated set of read and write surfaces backed by the bot's existing in-process state.

**v1 scope (this spec):**
- Pings: list, create, edit, delete templates
- AI memory: read all files; edit SOUL.md, LORE.md, user sheets; full CRUD on state notes

Everything else (leaderboard view, flight tracker, latency, transcripts, chat tail, schedules, suspensions) is explicitly out of scope and will be added in follow-up specs once the foundation is in place.

## Non-goals

- Public, unauthenticated read views
- Live chat tail / WebSocket streams
- Flight map / charts / live latency graph
- Editing schedules from the UI (`config.toml` remains source of truth, hot-reloaded)
- Mobile-optimized layout (functional on small screens; no dedicated design)
- Persisting sessions across restarts
- Audit log file separate from tracing output
- Diff/3-way merge UX for memory conflicts

## Decisions (from brainstorming)

| Axis                  | Decision                                                                 |
|-----------------------|--------------------------------------------------------------------------|
| Stack                 | Axum + Askama (server-rendered) + HTMX for small interactive bits         |
| Deploy                | Same scratch container; Cloudflare Tunnel exposes the bind port publicly  |
| Auth                  | Twitch OAuth (Authorization Code), session cookie, mod-gated middleware  |
| Allow list            | Hidden admins ∪ broadcaster of `twitch.channel` ∪ helix moderators       |
| Layout                | Persistent sidebar (Pings, Memory tree)                                   |
| Edit conflicts        | mtime/ETag check; 409 with submitted draft + current body side-by-side    |
| Crate topology        | Split bot into `crates/core` (lib) + new `crates/web` + `crates/twitch-1337` (bin) |

## Architecture

### Crate topology

The current `crates/twitch-1337` is a combined lib + bin. To break the prospective cycle (`bin → web → lib` would otherwise be `bin → web → bin`), split:

```
crates/
  core/              # Cargo package: twitch-1337-core (lib only)
  llm/               # unchanged
  web/               # Cargo package: twitch-1337-web (lib only); depends on core
  twitch-1337/       # bin only; depends on core + web
```

Directory `core/` is named bluntly per user preference; the Cargo package keeps `twitch-1337-core` so `use core::...` does not collide with std's `core`.

The split is mechanical: move all `src/*.rs` (current lib) into `core/`, leave `main.rs` in `twitch-1337/`, update workspace members, retarget `cargo build -p twitch-1337` paths. Visibility audit may require promoting a small number of internal types (`PingManager` API, `MemoryStore` API, `Configuration`) to `pub`; most already are.

### Module layout (`crates/web/`)

```
web/
  src/
    lib.rs            # pub fn run_web(deps, shutdown) -> Result<()>
    state.rs          # WebState struct
    auth/
      mod.rs          # OAuth handlers, mod check, session middleware
      session.rs      # in-memory session table, cookie sign/verify
      csrf.rs         # double-submit cookie helpers
    error.rs          # WebError enum + IntoResponse
    routes/
      pings.rs
      memory.rs
      health.rs
    templates/
      base.html
      pings/{list,form,row}.html
      memory/{tree,editor,state_list,conflict}.html
      auth/{login,denied}.html
    assets/           # embedded via rust-embed
      htmx.min.js
      pico.css        # subset
      app.css
  Cargo.toml
```

Templates use Askama with layout inheritance (`{% extends "base.html" %}`). Static assets are served from `/assets/*` via `rust-embed` so the binary stays self-contained and `FROM scratch` continues to work.

### Run hook (`crates/core/src/lib.rs::run_bot`)

If `config.web.enabled`, build a `WebDeps` struct from the same `Arc`s already passed to handlers and spawn `web::run_web(deps, shutdown_notify.subscribe())` alongside other handlers. Web shutdown participates in the existing graceful-shutdown protocol (`Arc<Notify>` + 5s timeout). Bind failure → `bail!` at startup, identical to IRC connect failure.

### Dependencies

Workspace additions: `axum`, `askama` (with `with-axum`), `tower`, `tower-http` (trace, cookie middleware), `tower-cookies`, `rust-embed`, `oauth2`, `url`. All pure Rust / rustls — musl static build and `FROM scratch` image are preserved.

## Auth

### OAuth flow

1. **`GET /login`** → 302 to `https://id.twitch.tv/oauth2/authorize` with `client_id`, `redirect_uri = <web.public_url>/auth/callback`, `response_type=code`, `scope=user:read:email`, and a CSRF state value stored in a short-lived signed cookie (`tw1337_oauth_state`, HttpOnly, 10-min Max-Age).
2. **`GET /auth/callback?code&state`** — verify state cookie, exchange code at `https://id.twitch.tv/oauth2/token` for a user access token, then `GET helix/users` to retrieve the caller's login + numeric id.
3. **Mod check** (cheap → expensive):
   1. `user_id ∈ twitch.hidden_admins` → admit
   2. `user_id == broadcaster_id_of(twitch.channel)` → admit (broadcaster id resolved once at startup via helix and cached)
   3. `user_id ∈ helix moderators of twitch.channel` (called using the bot's existing refreshed access token from `token.ron`) → admit
   4. otherwise → render `auth/denied.html` with HTTP 403
4. **Issue session** — random 32-byte id (`rand::rngs::OsRng`); cookie `tw1337_sid` (HttpOnly, Secure, SameSite=Lax, no Max-Age = browser session). Server-side `Arc<RwLock<HashMap<SessionId, Session>>>`. Session = `{ user_id, user_login, issued_at, last_seen, last_mod_check }`.
5. **`POST /logout`** — drop session entry + clear cookie.

### Session lifetime

- TTL = `web.session_ttl` (default `"7d"`), measured from `last_seen`. Sliding refresh on every authenticated request.
- Sessions are in-memory only. Restart = re-login. No persistence file.
- Mod check refreshed on session use older than `web.mod_check_refresh` (default `"5m"`). Helix call failures during refresh are logged and the session is admitted (avoid lockout on transient outages); failures during initial login are propagated as 502.

### CSRF for write actions

Double-submit cookie. On session creation, set `tw1337_csrf` cookie (random 32-byte hex, HttpOnly=false so JS-light templates can read it via DOM if needed; SameSite=Lax). Every form rendered server-side includes a hidden `_csrf` input populated from the same value held in `WebState`. POST/DELETE handlers compare cookie vs form field; mismatch → 403.

**HTMX-driven mutations:** standard `<form hx-post=...>` submissions auto-serialize the hidden `_csrf` input. Isolated buttons (e.g. inline ping delete, state-note delete) render without an enclosing form, so they must attach the token explicitly. Two acceptable patterns; the codebase picks one and uses it consistently:

- Per-element: `<button hx-post=".../delete" hx-vals='{"_csrf":"{{ csrf }}"}'>Delete</button>` — explicit, easy to grep.
- Global hook: an `htmx:configRequest` listener registered once in `app.js` reads the `tw1337_csrf` cookie and adds it to every non-GET request as a header `X-Csrf-Token`; the server middleware then accepts either form field `_csrf` or header `X-Csrf-Token`.

The global-hook variant is preferred — it eliminates the per-button copy-paste hazard and means the CSRF concern lives in one file instead of every template that renders a button. The middleware accepting either source keeps plain HTML forms (no JS needed) working as a fallback.

## Routes

```text
Public (no auth middleware):
  GET  /login                         → redirect to Twitch authorize
  GET  /auth/callback                 → token exchange + mod check + set session
  GET  /healthz                       → 200 if IRC connected, 503 otherwise
  GET  /assets/*                      → embedded static files

Authed (mod-gated):
  GET  /                              → 302 /pings
  POST /logout                        → drop session
  GET  /pings                         → list table + new-ping button
  GET  /pings/new                     → empty form
  POST /pings                         → create
  GET  /pings/:name                   → edit form
  POST /pings/:name                   → update
  POST /pings/:name/delete            → delete
  GET  /memory                        → tree (counts: SOUL, LORE, users(N), state(N))
  GET  /memory/soul                   → editor for SOUL.md
  GET  /memory/lore                   → editor for LORE.md
  GET  /memory/users                  → user list with name search
  GET  /memory/users/:user_id         → editor for users/<id>.md
  GET  /memory/state                  → state list + new-state button
  GET  /memory/state/new              → blank state form
  GET  /memory/state/:slug            → editor for state/<slug>.md
  POST /memory/soul                   → save SOUL.md (body, mtime_token, _csrf)
  POST /memory/lore                   → save LORE.md (body, mtime_token, _csrf)
  POST /memory/users/:user_id         → save users/<id>.md
  POST /memory/state                  → create state note (slug + body + _csrf)
  POST /memory/state/:slug            → save state/<slug>.md
  POST /memory/state/:slug/delete     → delete state/<slug>.md
```

Delete is exposed only for state notes by design — no route accepts deletes for SOUL, LORE, or user sheets.

### `WebState`

```rust
pub struct WebState {
    pub ping_manager: Arc<RwLock<PingManager>>,
    pub memory_store: Arc<MemoryStore>,
    pub sessions: Arc<RwLock<HashMap<SessionId, Session>>>,
    pub oauth: Arc<OAuthClient>,
    pub helix: Arc<dyn HelixClient>,         // new thin client, mirrors AviationClient pattern; boxed for tests
    pub csrf_secret: [u8; 32],
    pub irc_connected: Arc<AtomicBool>,
    pub config: Arc<WebConfig>,
    pub clock: Arc<dyn Clock>,
}
```

Same `Arc<RwLock<PingManager>>` and `Arc<MemoryStore>` already used by IRC handlers — no duplicate write paths.

### `MemoryStore::write` extension

`MemoryStore` gains a write method that takes an expected mtime token:

```rust
pub enum WriteOutcome {
    Written { new_mtime: Mtime },
    Conflict { current_body: String, current_mtime: Mtime },
}

pub async fn write_with_guard(
    &self,
    kind: FileKind,                  // existing enum in ai::memory::types
    id: &str,                        // empty for SOUL/LORE; user_id for users; slug for state
    body: &str,
    expected: Option<Mtime>,         // None = unconditional (used by ritual + write_file tool)
) -> Result<WriteOutcome, WriteError>;
```

The ritual and the AI `write_file` tool keep using the existing unconditional path (`expected = None`); the web layer always supplies the `expected` token from the form. Byte caps and validation are enforced in `MemoryStore` (already true today); the web layer adds no parallel rules.

### Healthz

`GET /healthz` returns 200 if the bot's IRC connection is currently alive, else 503. Mounted unconditionally when web is enabled. This is what Cloudflare Tunnel and the Docker `HEALTHCHECK` probe.

The connection-alive signal is a new `Arc<AtomicBool>` set true on successful initial IRC connect and updated by the latency monitor: cleared if `LATENCY_PING_INTERVAL * 3` elapses without a PONG, set again on the next PONG. This is the most direct, side-effect-free signal available; no existing flag exposes it.

The bot binary gains a `--healthcheck` flag that performs `GET http://127.0.0.1:<web.bind_port>/healthz` and exits 0/1. With `web.enabled = false` it exits 0 (skip). Used in the Dockerfile so probes work in `FROM scratch` without curl/wget:

```dockerfile
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD ["/twitch-1337", "--healthcheck"]
```

## Conflict UX

When `MemoryStore::write_with_guard` returns `Conflict`, the editor handler responds with HTTP 409 and re-renders `memory/conflict.html`:

- A read-only block showing the current on-disk body (post-conflict).
- A textarea pre-filled with the user's submitted body.
- A new mtime token reflecting current state.
- A banner: "File changed since you opened it (dreamer ritual or AI tool wrote to it). Your draft is preserved on the right — copy what you need into the textarea and resubmit."

No diff/merge tooling; user reconciles manually. This is acceptable for 4 KiB markdown files.

## Config

New optional `[web]` section in `config.toml`:

```toml
[web]
enabled = false
bind_addr = "127.0.0.1:8080"        # production container: "0.0.0.0:8080"
public_url = "https://bot.example.com"
session_secret = "<32+ bytes hex>"
session_ttl = "7d"                  # accepts "7d", "168h", "1d 12h", etc.
mod_check_refresh = "5m"            # accepts "5m", "300s", "1h", etc.
```

Duration fields use the `humantime-serde` crate (newly added) and deserialize into `std::time::Duration`. The crate accepts forms like `"7d"`, `"300s"`, `"1h 30m"`. This is a new convention for this project — existing `*_secs: u64` fields elsewhere in `config.toml` are intentionally left alone in this change to avoid scope creep; future cleanup may migrate them.

`session_secret` wraps in `SecretString`. Validation at config load (`enabled = true`):
- `session_secret` decoded length ≥ 32 bytes; reject otherwise
- `public_url` parses as `https://...` URL; reject otherwise
- `twitch.client_id` and `twitch.client_secret` already populated (already required for IRC)
- `session_ttl` between 1 hour and 30 days; reject otherwise
- `mod_check_refresh` between 30 seconds and 1 hour; reject otherwise

`config.toml.example` ships the section commented-out.

The mod allow list is derived dynamically from `twitch.hidden_admins`, the broadcaster of `twitch.channel`, and helix moderators. No static web allow list.

## Error handling

Centralized `WebError` enum implementing `axum::response::IntoResponse`:

| Variant                       | HTTP | Rendering                                                  |
|-------------------------------|------|------------------------------------------------------------|
| `Unauthenticated`             | 302  | Redirect to `/login?next=<original-path>`                   |
| `Forbidden`                   | 403  | `auth/denied.html`                                          |
| `CsrfMismatch`                | 403  | terse "Session expired, reload and try again"               |
| `Validation { field, msg }`   | 400  | re-render originating form with inline error                |
| `Conflict { kind, id, ... }`  | 409  | `memory/conflict.html`                                      |
| `OAuthExchange(_)` etc.       | 502  | retry link                                                  |
| `Internal(eyre::Error)`       | 500  | generic page; logged with `?error`                          |

Validation rules (control chars in ping templates, byte cap exceeded in memory bodies, ping name regex) are reused from `PingManager` and `MemoryStore`. The web layer maps their error types into `WebError::Validation` rather than re-implementing checks.

`tower_http::trace::TraceLayer` logs every request at INFO with method, path, status, latency. Auth events (login, mod-check pass/fail, logout) and write actions log at INFO with `user_id` + `user_login` + `action` + `target` + `result`. No separate audit file in v1.

## Testing

### Unit tests (`crates/web/`)

- `auth::session`: cookie sign/verify round-trip, TTL expiry against fake `Clock`, sliding refresh.
- `auth::csrf`: token issue/verify round-trip, mismatch path.
- `auth::mod_check`: hidden admin path, broadcaster path, helix path (with mocked `HelixClient`), denied path.
- `error::IntoResponse`: every variant produces expected status + template name.

### Route tests

Drive `axum::Router` with `tower::ServiceExt::oneshot` (no real network):

- **Pings**: list rendering, create rejects control chars, edit round-trip, delete removes from `PingManager`, name regex enforced.
- **Memory**: read each kind, byte-cap rejection per kind, mtime conflict path returns 409 with current body + draft preserved, state CRUD (create / edit / delete), kind boundary (no delete route mounted for soul/lore/users; routing assertion).
- **Auth**: unauthenticated authed-route hit → 302; authenticated non-mod → 403; mod → 200.
- **Healthz**: 200 when `irc_connected = true`; 503 when false (driven directly).

### Integration test (`crates/twitch-1337/tests/`)

One test that builds `run_bot` via `TestBotBuilder` with `web.enabled = true` and a fake transport, then makes a real TCP `GET /healthz` and asserts 200. Smoke test for the wiring in `core::run_bot`.

### Fakes

- `FakeHelixClient` implementing the project's `HelixClient` trait — returns canned moderator list / user lookup.
- Existing `MemoryStore` already test-friendly.
- Existing `PingManager` already test-friendly.
- Existing fake `Clock` reused for session-TTL tests.

CI: `cargo nextest run` already covers the workspace; the new crate's tests run automatically. `cargo audit` covers axum / tower / oauth2 advisories.

## Build sequence

The implementation plan should land in this order so each step lands as a green PR:

1. **Crate split** (`build/` branch): rename `crates/twitch-1337` → `crates/core` (package `twitch-1337-core`); add `crates/twitch-1337` bin-only crate; update workspace, Justfile, Dockerfile. No behavior change.
2. **Stub web crate** (`feature/`): empty `crates/web/` with `run_web` that binds, serves `/healthz`, exits on shutdown. Wired from `core::run_bot` behind `config.web.enabled`. Adds the `irc_connected: Arc<AtomicBool>` shared with the latency monitor (set/cleared as described under Healthz). `--healthcheck` flag added; Dockerfile `HEALTHCHECK` line added.
3. **Auth** (`feature/`): OAuth flow, session table, CSRF, mod check, login + denied templates, base layout with sidebar shell. Introduces a new minimal `HelixClient` trait + concrete impl (broadcaster id lookup, moderators list, user lookup), modeled on `AviationClient`. Bot's existing access token (`token.ron`) is reused for helix calls.
4. **Pings** (`feature/`): list / new / edit / delete routes + templates.
5. **Memory read** (`feature/`): tree, file viewer routes + templates, no writes.
6. **Memory write** (`feature/`): edit endpoints with mtime guard, state CRUD, conflict template.

Each step keeps the bot fully usable with web disabled or enabled, and CI green (fmt + clippy -D warnings + nextest + audit + SAST).

## Open questions

None blocking. Items deferred to follow-up specs:

- Read-only views for leaderboard / flights / latency / transcripts / chat tail / schedules / feedback inbox
- Manual dreamer trigger, suspend handler controls, config reload
- Audit log file separate from tracing output
- Mobile-optimized layout
