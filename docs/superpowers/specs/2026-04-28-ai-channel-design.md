# AI Channel — Design Spec

Date: 2026-04-28
Branch: `feature/multi-channel`

## Goal

Add a third operating channel — `ai_channel` — dedicated to `!ai` usage. Move
AI-command spam off the primary channel without splitting AI state.

## Background

Bot currently joins:

- `twitch.channel` — primary, all features.
- `twitch.admin_channel` — optional, broadcaster-only command surface for
  testing.

`!ai` runs in both. Heavy use clutters the primary channel. We want a public
channel where viewers can use `!ai` freely while leaving the primary channel
clean.

## Scope

In:

- New optional `twitch.ai_channel` config field.
- Join the channel on startup.
- In `ai_channel`, only `!ai` is dispatched. Every other command is ignored.
- 1337 tracker, pings, flight tracker, scheduled messages do not act on
  messages from `ai_channel`.
- Chat history (used by `!ai` for context) continues to record only the
  primary channel.

Out:

- Per-channel AI memory or per-channel chat history.
- Per-channel cooldowns (already keyed by user only — works as-is).
- Generalized `[[channels]]` config — explicit named fields preserve the
  existing pattern.
- Any change to `admin_channel` behavior.

## Channel role matrix

| Feature                       | `channel` | `admin_channel`     | `ai_channel`   |
|-------------------------------|-----------|---------------------|----------------|
| 1337 tracker                  | yes       | no                  | no             |
| `!lb`                         | yes       | current behavior    | no             |
| Pings (`!p`, `!<ping>`)       | yes       | current behavior    | no             |
| Flight tracker commands       | yes       | current behavior    | no             |
| Scheduled messages            | yes       | no                  | no             |
| `!ai`                         | yes       | broadcaster only    | anyone         |
| `!up`, `!fl`, `!fb`, others   | yes       | current behavior    | no             |
| Admin commands (`!suspend` …) | current   | current             | no             |
| Chat history → AI context     | yes       | no                  | no             |
| AI memory                     | shared (global) | shared        | shared         |

`ai_channel` = exactly one reachable command (`!ai`); AI sees no channel
difference because history and memory remain global.

## Config

`src/config.rs`, `TwitchConfig`:

```rust
pub struct TwitchConfig {
    pub channel: String,
    pub admin_channel: Option<String>,
    pub ai_channel: Option<String>,   // new
    /* … */
}
```

Validation in `validate_config` (mirrors `admin_channel`):

- If set: trim non-empty; distinct from `channel`; distinct from
  `admin_channel`.

`config.toml.example` updated with a commented example.

## Join

`src/twitch/setup.rs`: insert `ai_channel` into the `channels: HashSet<String>`
when `Some`, with an `info!` log line equivalent to the admin one.

## Dispatcher

`src/twitch/handlers/commands.rs::run_command_dispatcher`:

- Add `ai_channel: Option<String>` to the parameter list (plumbed from
  `Services` like `admin_channel`).
- New guard placed before the admin-channel and chat-history branches:

  ```text
  if privmsg.channel_login == ai_channel:
      parse invocation
      if trigger != "ai": continue        // skip everything else
      skip chat-history recording
      run command (existing path)
      continue
  ```

- Admin-channel branch unchanged.
- Primary-channel branch unchanged.

`!ai` registration is unchanged; the dispatcher decides reachability.

## Other handlers

Each non-command handler must process only the primary channel.

- `tracker_1337.rs`: filter `privmsg.channel_login == config.twitch.channel`
  before recording.
- Ping handler: same filter on the listener path that triggers stored ping
  templates.
- Flight tracker command listeners (already reachable only via dispatcher —
  the dispatcher ignore in `ai_channel` is sufficient; no extra change).
- Scheduled-messages handler: already targets `config.twitch.channel` for
  `say()` — unchanged.

Add an explicit `channel_login` check rather than relying on incidental
filtering, so adding `ai_channel` does not regress these features.

## AI

No code change. `!ai` command, chat history wiring, and memory are channel-
agnostic at the call site; the dispatcher gates reachability.

The bot replies in the channel where the command was issued (existing
`client.say(channel, …)` pattern via `CommandContext`).

## Tests

Add cases in `tests/` using `TestBotBuilder`:

- `!ai` in `ai_channel` → AI command runs, reply targets `ai_channel`.
- `!lb`, `!p`, `!<ping>`, `!track`, `!up`, `!fl`, `!fb` in `ai_channel` →
  ignored (no reply, no state change).
- `1337` message at 13:37 in `ai_channel` → not recorded by 1337 tracker.
- `!ai` in primary channel → unchanged baseline.
- Config validation: `ai_channel == channel` → error;
  `ai_channel == admin_channel` → error; empty string → error.

## Risk / blast radius

- Existing handlers may currently assume single-channel input. Adding
  explicit `channel_login` filters in 1337 tracker + ping listener is
  defensive and matches the architecture invariant in CLAUDE.md ("All time
  ops Berlin", "Handlers independent").
- `ai_channel` defaults to `None`. With it unset, behavior matches current
  main exactly.

## Files touched

- `src/config.rs` — field + validation.
- `config.toml.example` — example block.
- `src/twitch/setup.rs` — join.
- `src/twitch/handlers/commands.rs` — dispatcher guard + plumbing.
- `src/twitch/handlers/spawn.rs` — pass `ai_channel` into dispatcher.
- `src/lib.rs` — `Services` plumbing if needed.
- `src/twitch/handlers/tracker_1337.rs` — channel filter.
- `src/twitch/handlers/` (ping listener path) — channel filter.
- `tests/common/` + new integration test file.
