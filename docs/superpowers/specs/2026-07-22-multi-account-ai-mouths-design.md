# Multi-account AI mouths (Helix speak-only) — Design Spec

Date: 2026-07-22
Branch: `spec/multi-account-ai-mouths`

## Goal

Give Aurora (`!ai`) and Grok (`@grok`) distinct Twitch account identities for
outbound chat, while the utility bot keeps the single IRC connection for
inbound traffic and all non-AI sends. Improve chat presence, keep mod/staff
powers on the utility account only, and make agent identity accurate in chat
history and transcripts.

## Background

Today the bot is one IRC login. Both AI agents `reply()` through that client,
then history remaps lines to `{persona_name} (self)` using a single
`bot_login` / Aurora `persona_name`. Viewers cannot tell agents apart; Grok
inherits Aurora’s display identity in the model window.

Helix `POST /helix/chat/messages` can send as another user (with that user’s
token) and supports `reply_parent_message_id`, so secondary accounts do not
need IRC joins.

## Decisions (locked in discovery)

| Choice | Decision |
|--------|----------|
| Scope of value | Chat UX + mod isolation + model history (all three) |
| Account count | Three: utility + Aurora + Grok |
| Topology | Speak-only AI mouths; utility alone reads chat |
| Privileges | Mod the utility bot only; AI mouths are normal users (VIP optional ops) |
| Send path | Helix chat send for AI mouths (not secondary IRC) |
| Token refresh | Reuse `FileBasedTokenStorage` + `RefreshingLoginCredentials`; only the token file path differs |
| HTTP shape | Thin chat-send helper (mirror `HelixWhisperSender`); no general Helix client |
| Mouth OAuth scopes | User access token with **only** `user:write:chat` |
| Echo filter | History/transcript: Helix `message_id` primary + mouth-identity backstop; commands/1337/emote: mouth login/`user_id` |

## Scope

In:

- `AgentMouth` seam + Helix adapters for Aurora/Grok; Utility adapter wraps
  existing IRC `ChatSender`.
- Optional `[twitch.mouths.aurora]` / `[twitch.mouths.grok]` bootstrap config.
- Per-mouth token files under `$DATA_DIR`, via path-parameterized
  `FileBasedTokenStorage` + existing `RefreshingLoginCredentials` (no
  parallel refresh stack).
- AI success-reply path routes through `speak`; history/transcript identity
  per agent; hybrid echo filter (`message_id` primary + mouth-identity backstop
  for history/transcript, mouth identity for commands/1337/emote).
- Startup degrade when a mouth fails; Utility IRC fallback when a mouth is
  absent.

Out:

- Secondary IRC connections for AI accounts.
- A second OAuth refresh implementation for mouths (path-only difference on
  existing storage/credentials).
- A general-purpose Helix client in core, reuse of `web::helix::HelixClient`
  for chat send, or adding the `twitch_api` crate for this feature.
- Dashboard editing of mouth refresh tokens.
- Live add/remove mouths without restart.
- Mid-turn auto-fallback to Utility after a Helix failure (would risk
  double-speak ambiguity).
- Mouth health UI on the dashboard.
- Migrating or re-tagging historical chat-history / prefill rows.
- Changing dashboard OAuth, mod checks, or non-AI command send paths.

## Architecture

```text
                    ┌─────────────────────┐
  Twitch IRC  ───►  │ Utility connection  │ ──► router ──► broadcast ──► handlers
                    │ (read + utility say)│
                    └─────────────────────┘
                              │
                    non-AI say/reply (ChatSender)
                              │
                         ┌────┴────┐
                         │ AgentMouth │
                         └────┬────┘
              ┌───────────────┼───────────────┐
              ▼               ▼               ▼
         Utility IRC     Helix Aurora     Helix Grok
         (ChatSender)    (user token)     (user token)
```

**Deep module:** `AgentMouth` — callers know agent, text, channel ids, optional
reply parent. They do not know Helix vs IRC, tokens, or refresh.

```rust
// Conceptual interface (names may shift in implementation)
speak(agent, SpeakRequest {
    broadcaster_id,
    channel_login,
    text,
    reply_parent_message_id: Option<…>,
}) -> Result<SpokenMessage { message_id, agent, login, display_name }>
```

**Adapters (v1):**

| Agent | Transport | Used for |
|-------|-----------|----------|
| `Utility` | existing IRC `ChatSender` | commands, schedules, tracker, staff-facing errors |
| `Aurora` | Helix send + Aurora user token | `!ai` final replies |
| `Grok` | Helix send + Grok user token | `@grok` final replies |

Seam lives in `crates/core` (chat send), not `crates/web`’s dashboard Helix
trait. Trait is fakeable for tests (wiremock / in-memory).

**HTTP: thin send helper, not a Helix client.** Chat send follows
`HelixWhisperSender` (`crates/core/src/twitch/whisper.rs`): a small type that
holds `reqwest::Client`, `LoginCredentials`, `client_id`, and the mouth
`user_id`, calls `get_credentials()`, and `POST`s
`/helix/chat/messages`. One job — send and map errors.

Do **not**:

- Extend or depend on `crates/web`’s `HelixClient` (users/mods/avatars,
  utility-bot token provider).
- Add a multi-endpoint “Twitch API” facade or the `twitch_api` crate for v1.
- Invent a second OAuth/HTTP stack beside the whisper pattern.

Both mouths may share one `reqwest::Client` (connection reuse); credentials
stay per-mouth. That helper is the Helix adapter behind `AgentMouth`, not a
separate platform SDK.

## Config & credentials

Bootstrap secrets in `config.toml` (Deserialize-only, `SecretString`):

```toml
[twitch]
# existing utility fields unchanged…

[twitch.mouths.aurora]   # optional
username = "aurora_bot"
refresh_token = "…"

[twitch.mouths.grok]     # optional
username = "grok_bot"
refresh_token = "…"
```

- Share `[twitch]` `client_id` / `client_secret` (one Twitch app, three user
  auths).
- Omit a mouth section → that agent falls back to Utility IRC (current
  behavior). Roll out one mouth at a time.

### OAuth scopes (locked)

Auth mode for mouths: **user access token** (not app access token). Twitch
[Send Chat Message](https://dev.twitch.tv/docs/api/reference/#send-chat-message)
requires, for that mode, exactly:

| Account | Required scopes | Why |
|---------|-----------------|-----|
| Aurora mouth | `user:write:chat` | Helix chat send; `sender_id` must match the token’s user |
| Grok mouth | `user:write:chat` | same |
| Utility bot | unchanged (`chat:read`, `chat:edit`, plus existing whisper/dashboard needs) | still IRC; not part of this change |

**Explicitly not required** on mouth tokens (do not request):

| Scope | Why omitted |
|-------|-------------|
| `user:bot` | Only for the **app** access-token send path (with `channel:bot` / mod) |
| `channel:bot` | Broadcaster grant for the app-token path; we use user tokens |
| `chat:read` / `chat:edit` | IRC scopes; mouths do not join IRC |
| `moderator:manage:chat_messages` | Only if `pin: true` on send — we do not pin |
| `user:manage:whispers` / whisper scopes | Mouths do not whisper |
| `user:read:chat` | EventSub chat receive; mouths are speak-only |

Startup `user_id` resolve via Helix Get Users needs a valid access token but
**no additional scope** for id/login/display_name.

Ops: authorize each mouth account against the shared app with scope
`user:write:chat` only; put the resulting refresh token in
`[twitch.mouths.*.refresh_token]`.

`$DATA_DIR` token files:

| File | Account |
|------|---------|
| `token.ron` | utility (unchanged) |
| `token-aurora.ron` | Aurora mouth |
| `token-grok.ron` | Grok mouth |

**Token refresh — reuse only; path differs.** Mouths must not grow a parallel
OAuth refresh stack. Use the existing `FileBasedTokenStorage` +
`RefreshingLoginCredentials` path the utility bot already uses:

- Parameterize `FileBasedTokenStorage` with an explicit path (today
  `new(refresh_token)` hardcodes `token.ron`). Utility keeps
  `…/token.ron`; Aurora/Grok pass `…/token-aurora.ron` and
  `…/token-grok.ron` with their config `refresh_token`.
- Same load/update semantics (config refresh-token wins when it diverges
  from the file; atomic RON persist; missing file → fresh from config).
- Same credential type: construct `RefreshingLoginCredentials` per mouth
  (`init_with_username` + shared `client_id`/`client_secret` + that
  storage). Helix send calls `get_credentials()` for the access token;
  no bespoke refresh HTTP.
- Runtime 401 handling: rely on that credential refresh (same as IRC),
  not a mouth-specific refresh helper.

Startup resolves each configured mouth’s `user_id` after the first
credential fetch and holds `{login, user_id, credentials}` inside the
adapter. Adding/removing mouths requires restart.

Non-secret display names:

- Aurora: existing `ai.behavior.persona_name` in `settings.ron`.
- Grok: `ai.grok.persona_name` (default `"Grok"`). Dashboard field optional for
  v1 if the default is baked.

## Data flow (AI reply path)

Inbound unchanged. On a **successful** agent turn (replaces today’s
`ctx.sender.reply` + history push for the final AI text):

1. Map trigger → `Agent::Aurora` (`!ai`) or `Agent::Grok` (`@grok`).
2. `mouths.speak(agent, SpeakRequest { … reply_parent: privmsg.message_id })`.
3. On `Ok(spoken)`: push history + transcript with spoken identity.
4. On `Err`: log; optional short Utility IRC notice to the invoker; do not
   record history/transcript as if the mouth spoke.

Adapter selection:

- Mouth configured and available → Helix `POST /helix/chat/messages` with that
  user’s token (`sender_id` = mouth `user_id`).
- Mouth missing/unavailable → same `speak` call delegates to Utility IRC
  `reply`. History still tags the **logical** agent persona so the model
  learns the right `(self)` during migration.

Unchanged: cooldown/suspend/usage errors, empty model output, provider errors
→ Utility `reply` (staff voice). All non-AI features → Utility only.

Mouths may speak in primary, `admin_channel`, and `ai_channel` wherever
`!ai`/`@grok` already run — Helix needs that room’s `broadcaster_id`, not a
second IRC join.

## History / identity

**Recording** after successful `speak`:

| Field | Value |
|-------|--------|
| `username` | mouth login (or utility login on fallback) |
| `display_name` | agent persona setting |
| agent tag | Aurora / Grok / Utility (extend `ChatHistorySource` or add a field) |

**Formatting** (`format_entry_line`):

- Current turn Aurora → only Aurora lines get `Aurora (self): …`
- Grok lines on an Aurora turn render as `Grok: …` (not self)
- Symmetric for Grok turns
- Utility lines never get `(self)` on an AI turn

**Echo filter (concrete):** Helix sends as Aurora/Grok arrive on the utility
IRC connection as ordinary `Privmsg` (Twitch does not echo *your own* IRC
sends, but it does deliver other users’ messages — mouths are other users).
IRC `PrivmsgMessage.message_id` matches Helix’s returned `message_id`.

Two filters, by purpose:

**1. History + transcript — pending `message_id` (precise echo suppress)**

After a successful Helix `speak` that returns a non-empty `message_id` and
`is_sent: true`:

1. Insert that id into a shared short-lived pending set (TTL + hard cap; drop
   oldest on overflow).
2. Write the synthetic bot/transcript row as today.
3. On inbound `Privmsg`, if `privmsg.message_id` is in the set → **skip**
   history `push_user*` and transcript append, then remove the id.
4. **Backstop:** a `Privmsg` from a configured mouth identity (login or
   `user_id`) whose id is *not* in the pending set is still skipped for
   history/transcript. Mouths are speak-only, so every mouth line is an echo of
   a `speak` — none is legitimate user history. This covers a missed register
   or a Helix-vs-IRC `id` divergence.

`message_id` stays the **primary** suppress (precise, and it frees the id from
the set so it stays small); the mouth-identity backstop only fires when the
primary misses. Confirm Helix `message_id` == IRC `id` once against live
Twitch; the backstop keeps a mismatch from producing double rows regardless.

Do **not** dedupe by message text. If Helix returns empty id / `is_sent:
false`, do not record a pending id (and do not synthetic-push a “sent”
line). Race is fine: we await Helix and register the id before the IRC
fan-out is observed on our connection.

**2. Commands / 1337 / emote — mouth identity (bot suppress)**

Build at startup from every **configured** mouth (login always; `user_id`
when credential fetch succeeded):

```text
MouthIdentityFilter { logins: HashSet<lowercase login>, user_ids: HashSet<id> }
```

Match `sender.id` first, else case-insensitive login.

| Ingest point | On mouth-identity match |
|--------------|-------------------------|
| Command dispatch | **Skip** — mouths are speak-only |
| 1337 tracker | **Ignore** like `is_clanker` |
| Emote usage | **Flag** via `should_flag_usage` (append mouth logins to its `deny_list` arg — no signature change) |

Mouth identity is the **backstop** for history/transcript (behind the
`message_id` primary) and the **sole** filter for commands/1337/emote. Either
way, a mouth line never runs commands or farms 1337 even if it slips the
pending-id set (missed register, id divergence).

Utility-IRC fallback path needs no echo filter for the speaking account:
Twitch does not re-deliver the utility bot’s own IRC `PRIVMSG` to itself.

**Transcript:** same login/display identity as history (not always utility
`bot_username`).

**Prefill:** no agent discrimination required; live `speak` path owns tags.
No migration of old buffer entries.

## Errors & degrade

| Case | Behavior |
|------|----------|
| Utility IRC auth failure | Fatal (unchanged) |
| Mouth refresh / user-id resolve failure at startup | Log, mark mouth unavailable, continue |
| Both mouths down | AI via Utility IRC fallback; warn once |
| Runtime 401 | `get_credentials()` / existing refresh via `RefreshingLoginCredentials`, then retry send once; then fail the speak |
| 403 / 5xx / timeout / rate limit | Log with `?error`; no history write; optional short Utility notice to invoker |
| Helix fail mid-success-path | Do **not** also send the same text via Utility IRC |
| Only one mouth configured | That agent Helix; the other Utility fallback with logical persona tags |

Rate-limit queueing is out of v1: fail the turn if Helix rejects.

## Testing

- Unit: fake `AgentMouth` — routing, `reply_parent`, history identity,
  pending-`message_id` history skip, mouth-identity command/1337 skip.
- Wiremock: Helix success, 401→refresh→retry, hard 403, rate limit.
- Integration (`TestBotBuilder`): `!ai` / `@grok` with fake mouth; inject
  inbound PRIVMSG with the spoken `message_id` after speak and assert no
  double history row; inject a PRIVMSG from the mouth login with a *different*
  id and assert it produces no history row (backstop) and is ignored by
  commands/1337.

## Implementation notes (non-binding)

- Prefer introducing the seam and history agent tags before wiring real Helix,
  so fallback path already improves Grok vs Aurora representation.
- Chat-send HTTP: copy the `HelixWhisperSender` shape (credentials + one
  endpoint), not `web`’s Helix trait. Optional shared `reqwest::Client` across
  mouths.
- First implementation step for credentials: add a path argument to
  `FileBasedTokenStorage` (keep `token.ron` as the utility default) so mouths
  are a one-line path change, not a fork of persist/refresh logic.
- Update `config.toml.example` and CLAUDE.md config section when implementing;
  no semver / changelog (rolling release).

## Open points (resolve in implementation plan)

- Exact Rust type names and module path under `crates/core/src/twitch/`.
- Whether `ai.grok.persona_name` ships with a dashboard card in the same PR or
  a baked default first.
- German copy for the optional Utility failure notice to the invoker.
- Empirically confirm Helix `message_id` == IRC `id` tag against live Twitch
  (primary filter assumes equality; mouth-identity backstop covers divergence,
  so this is a verify-not-block item).
- Pending-id set TTL + hard-cap values.
