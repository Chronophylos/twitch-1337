# AI agent settings flatten

Status: approved
Date: 2026-08-26
Branch: `cursor/ai-agent-settings-flatten-b7de`

## Goal

Kill the “one active profile, everyone else inherits” model. Each agent
picks a provider profile from its own dropdown. `/settings` is agent-first:
Aurora, Grok, Recap, Dreamer, and Media are peer cards, not overrides hanging
off a generic AI stack. Shared knobs (history buffers, prefill, memory,
emotes, web) sit in a second group. `settings.ron` keys match the cards.

## Non-goals

- A chat command to switch profiles.
- Per-profile enable/disable. Delete the profile or don't.
- New agent knobs (Grok reply-length, per-agent history caps, etc.).
- Rewriting `settings.ron` on boot. Fold at resolve; persist the new shape
  on the next dashboard save.
- Deleting the leftover override fields in this PR. They are read-only and
  marked `MIGRATION(ai-agents)` for a later cleanup once every env has saved
  as schema 7.
- Changing `/ai-profiles` CRUD besides dropping Activate and tightening
  delete/rename against the five agent pointers.
- Moving profiles CRUD onto `/settings`.

## Why this shape

`ai.active_profile` made Aurora the implicit default and every other agent
an optional override. The dashboard copied that: a Connection signpost, a
Behavior card that was really Aurora, and empty `<select>` options that
meant “whatever is active.” Recap lived inside History because it happened
to share a struct.

Agents are the thing operators tune. Shared cards are infrastructure.
Empty-inherit was the whole problem; required dropdowns (Recap still
empty = off) are the fix.

## Data model

`SCHEMA_VERSION` 6 → **7**.

Resolved `AiSettings`:

```rust
pub struct AiSettings {
    pub profiles: Vec<AiProfile>,
    // agents
    pub aurora: AiAurora,       // was AiBehavior, plus required profile
    pub grok: AiGrok,           // profile: String (required)
    pub recap: AiRecap,         // lifted out of history
    pub dreamer: AiDreamer,     // profile: String (required)
    pub media: AiMedia,         // profile: String (required)
    // shared
    pub history: AiHistory,     // buffer fields only
    pub memory: AiMemory,
    pub prefill: Option<AiPrefill>,
    pub web: Option<AiWeb>,
    pub emotes: Option<AiEmotes>,
}

pub struct AiAurora {
    pub profile: String,        // required, names a row in profiles
    pub max_turn_rounds: usize,
    pub max_writes_per_turn: usize,
    pub persona_name: String,
    pub min_response_delay_ms: u64,
    pub reply_target_chars: usize,
    pub reply_soft_chars: usize,
    pub reply_hard_chars: usize,
}

pub struct AiGrok {
    pub profile: String,        // required
    pub persona_name: String,   // dashboard-editable (was baked-default only)
}

pub struct AiDreamer {
    pub enabled: bool,
    pub profile: String,        // required
    pub run_at: String,
    pub timeout_secs: u64,
    pub max_rounds: usize,
    pub max_writes_per_turn: usize,
}

pub struct AiMedia {
    pub profile: String,        // required
    pub timeout: u64,
    pub max_image_size: ByteSize,
    pub max_pdf_size: ByteSize,
    pub max_audio_size: ByteSize,
    pub max_video_size: ByteSize,
    pub max_text_size: ByteSize,
}

pub struct AiRecap {
    pub profile: Option<String>, // None / empty = Recap off. Never inherits.
    pub batch_size: u64,
    pub max_notes: u64,
    pub merge_width: u64,
}

pub struct AiHistory {
    pub length: u64,
    pub ai_channel_length: u64,
    pub n: u64,
    pub ai_channel_n: u64,
}
```

Compiled defaults: Aurora / Grok / Dreamer / Media `profile = "default"`.
Recap `profile = None` (off). Grok `persona_name = "Grok"`. Remaining
numeric defaults unchanged from today's Behavior / Dreamer / Media / History
recap knobs.

`AiSettings::active()` and `resolved_profile_in` go away.

### Overrides (disk)

`AiOverrides` gains `aurora: AiAuroraOverrides` and `recap: AiRecapOverrides`.
Grok/Dreamer/Media `profile` stay `Option<Option<String>>` on disk so a
sparse file can still omit them; resolve fills a missing pointer from the
fold (below). After a v7 save they are always `Some(Some(name))`.

`AiGrokOverrides` gains `persona_name: Option<String>` so the dashboard
row actually persists. Today resolve hard-codes `defaults.grok.persona_name`.

Keep, **read-only**, never written by a v7 save, commented
`MIGRATION(ai-agents)`:

- `AiOverrides::active_profile`
- `AiOverrides::behavior` (the whole `AiBehaviorOverrides` block)
- `AiHistoryOverrides::{recap_profile, batch_size, max_notes, merge_width}`

Same pattern as `AiOverrides::connection` (`MIGRATION(ai-profiles)`). Do
not delete them in this PR.

`migrate_legacy_ai` (config.toml v1 → settings) keeps writing
`out.behavior` and does not grow an `aurora` writer. The schema-7 fold
consumes it, the way it already consumes `out.connection`.

## Resolve-time fold

On every `resolve_ai`, after profiles are assembled:

1. Let `legacy_active` be `overrides.active_profile` if non-blank,
   else `"default"`.
2. For each required agent (aurora, grok, dreamer, media):
   - If the new override (`aurora.profile` / `grok.profile` / …) is
     `Some(Some(name))` with a non-blank name, use it.
   - Else if the old optional override is `Some(Some(name))` with a
     non-blank name (grok/dreamer/media v6), use it.
   - Else use `legacy_active`.
3. Aurora knobs: `overrides.aurora.*` wins; missing keys fall through to
   `overrides.behavior.*`; then compiled defaults.
4. Recap: `overrides.recap.*` wins; missing keys fall through to
   `overrides.history.{recap_profile,batch_size,max_notes,merge_width}`;
   Recap profile empty stays off (v6 `history.recap_profile` None is off,
   not `legacy_active`).
5. History resolved struct has only the four buffer fields.

A v6 file with `active_profile = "cheap"` and empty agent profiles
becomes aurora/grok/dreamer/media all `"cheap"`, recap still off. A v6
file that already pinned grok to `"local"` keeps `"local"`; the others
take `"cheap"`.

The next dashboard save writes `aurora` / `recap` / required profiles and
omits `active_profile`, `behavior`, and the recap keys under `history`.

## Dashboard

### Page order

Nav groups and card order:

1. **Twitch** — Permissions, Channels
2. **Agents** — Aurora, Grok, Recap, Dreamer, Media
3. **Shared** — History, Prefill, Memory, Emotes, Web
4. **Commands** — Cooldowns, Pings, Suspend
5. **Emote Tracker** — Potat attribution
6. **Flight Tracking** — Aviationstack
7. **System** — Dashboard

The old **AI** parent group and the **Connection** card go away. No
leftover “manage profiles” signpost card; each agent profile `<select>`
is enough. `/ai-profiles` stays in the sidebar as today.

### Agent cards

- **Aurora** — today’s Behavior fields plus a required profile `<select>`
  (no empty option). Section id `ai_aurora`. Reset: `SettingsSection::AiAurora`.
- **Grok** — required profile `<select>` plus `persona_name` (the row that
  was never wired). Section id `ai_grok`.
- **Recap** — profile `<select>` with empty = off, plus `batch_size`,
  `max_notes`, `merge_width`. Move the “recap parked” badge here from
  History. Section id `ai_recap`. Reset: `SettingsSection::AiRecap`.
- **Dreamer / Media** — same knobs as today; profile `<select>` is
  required (no “(active profile)” empty option).

Form names follow the new paths (`ai_aurora_profile`,
`ai_aurora_persona_name`, `ai_recap_profile`, …). History POST no longer
carries recap fields.

### `/ai-profiles`

- Unmount `POST /ai-profiles/{name}/activate`. Tests expect 404. Remove
  the Activate button and ACTIVE chip.
- Rename: rewrite every agent pointer that still names the old row
  (aurora, grok, dreamer, media, recap) — the same rewrite
  `active_profile` gets today, over five slots.
- Delete: refuse if any of those five still points at the name (Recap
  only counts when it is on). Last remaining profile stays undeletable.
  Do not clear a required pointer and invent a replacement.

## Runtime resolution

`LlmRegistry::resolve(Option<&str>)` currently means “this name, else
`active_profile`.” That fallback goes away.

Call sites pass the agent’s own required name via `resolve_named`:

- Aurora `!ai` — `snap.ai.aurora.profile`
- Grok `@grok` — `snap.ai.grok.profile`
- Dreamer ritual — `snap.ai.dreamer.profile`
- Media pass — `snap.ai.media.profile`
- Recap batcher — if `snap.ai.recap.profile` is None/empty, Recap is off;
  otherwise `resolve_named`

A missing name is a warn + skip that turn/ritual/media pass/recap tick.
No hop to another profile. Empty model on the named profile keeps the
existing registry skip.

Startup log that prints `snapshot.ai.active()` prints Aurora’s profile
instead.

Shared cards do not change runtime behavior — only their nav home.

## Validation

In `ValidationContext`, replacing the `active_profile` / optional-agent
checks:

- `ai.aurora.profile`, `ai.grok.profile`, `ai.dreamer.profile`,
  `ai.media.profile` non-blank after trim and must name a row in
  `profiles`.
- `ai.recap.profile`, when set and non-blank, must name a row. Empty is
  legal (off).
- `ai.aurora.persona_name` and `ai.grok.persona_name` non-blank after trim
  (aurora already has this as `ai.behavior.persona_name`).
- Recap numeric bounds stay as they are today on History.
- No `ai.active_profile` check.

Field paths: `ai.aurora.profile`, `ai.recap.profile`, `ai.grok.persona_name`,
…

## Error handling

- Stale required profile (deleted out from under an agent, or a v6 file
  whose `active_profile` named a missing row): `resolve_named` warns and
  skips. Bot stays up.
- Recap with a missing name: skip that tick (treat as off); keep the
  parked badge if the batcher already parked.
- v6 file with no `active_profile` either: required agents get
  `"default"`. If that row does not exist, skip at the call site until
  the operator picks one.
- Save validation is the hard gate — the dashboard cannot persist a
  required agent pointing at nothing.

## Testing

Resolve fold:

- v6 overrides, `active_profile = "cheap"`, empty agent profiles →
  aurora/grok/dreamer/media all `"cheap"`; recap stays off.
- v6 with grok already pinned to `"local"` keeps `"local"`; others take
  active.
- v6 `history.recap_profile = "tiny"` lands on `ai.recap.profile`.
- v6 `behavior.persona_name` lands on `ai.aurora.persona_name`.
- Next save does not write `active_profile`, `behavior`, or
  `history.{recap_profile,batch_size,max_notes,merge_width}`.

Dashboard:

- Nav/card order as above; no Connection card; no AI parent group.
- Aurora/Grok/Dreamer/Media `<select>` has no empty option.
- Recap empty = off still round-trips.
- Grok `persona_name` round-trips.
- Reset `ai_aurora` restores compiled Aurora defaults including profile
  `"default"`; reset `ai_recap` restores Recap-off.

`/ai-profiles`:

- Activate route unmounted (404).
- Delete refused when aurora (etc.) still names that profile.
- Rename rewrites all five pointers.
- Delete last profile still refused.

Integration:

- Changing `ai.aurora.profile` changes the next `!ai` model.
- Grok does not follow aurora.
- Recap off when profile empty.

## Docs

Update in the same PR:

- `CLAUDE.md` — `active_profile`, Behavior card, sub-agent fallback copy.
- `docs/ai-prompts.md` — Behavior → Aurora for reply-length knobs.
- `.cursor/rules/config-settings.mdc` only if a mention of `active_profile`
  appears by then (none today).

Historical specs under `docs/superpowers/specs/` stay as written.

## Rollout

One PR. `settings.ron` is rewritten on the first dashboard save; until
then the legacy fields carry the deployment.

Rollback:

- v7 binary, unsaved: fold is read-only, disk still v6 → image pin back
  to v6 is safe.
- After a v7 save: rollback needs the previous `settings.ron` as well as
  the image. Note in the PR body.

## Files (implementation map)

| Area | Paths |
|---|---|
| Schema | `crates/core/src/settings/ai.rs`, `overrides.rs`, `resolve.rs`, `validation.rs`, `mod.rs` (`SCHEMA_VERSION`, `SettingsSection`), `store/` reset + merge |
| Registry | `crates/core/src/llm_factory.rs` — drop `resolve` fallback; call sites use `resolve_named` |
| Call sites | `crates/core/src/ai/command.rs`, `memory/ritual.rs`, `content/executor.rs`, `recap_batcher.rs`, `twitch/handlers/commands.rs` |
| Settings UI | `crates/web/templates/settings/index.html`, `cards/ai_*.html` (rename behavior → aurora, new recap card, drop connection), `_macros.html` if nav group count changes |
| Settings routes | `crates/web/src/routes/settings/{mod,forms,view}.rs` |
| Profiles page | `crates/web/src/routes/ai_profiles.rs`, `templates/ai_profiles/_card.html` |
| Tests | `crates/core/src/settings/{resolve,validation,ai}.rs` unit tests; `crates/core/tests/{ai_profile_switching,grok}.rs`; `crates/web/tests/{settings_route,ai_profiles_route,assets_smoke}.rs` |
| Docs | `CLAUDE.md`, `docs/ai-prompts.md`, `.cursor/rules/config-settings.mdc` |
