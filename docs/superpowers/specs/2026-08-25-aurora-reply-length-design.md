# Aurora Reply-Length Settings — Design

Status: implemented
Date: 2026-08-25
Branch: `feature/aurora-reply-length`

## Goal

Give the owner three live dashboard knobs that control only Aurora's (`!ai`)
chat-reply length: the target the model is told, the soft limit that triggers
one rewrite, and the hard limit that trims with an ellipsis. Grok keeps today's
baked prompt copy and the 490/490 reword-clamp.

## Non-goals

- No Grok dashboard knobs and no `{reply_*}` tokens in `grok.md`.
- No change to Helix/IRC byte policy (issue #417). Agent replies still skip the
  480-byte IRC clamp; Helix still 422s over 500 characters.
- No dreamer or media reply-length controls.
- No `config.toml` field and no migration sentinel — born in `settings.ron`
  with `Option` overrides falling through to compile-time defaults.

## Background

Three numbers already exist, but they are not independent:

| Layer | Aurora | Grok |
|---|---|---|
| Prompt (baked) | "Ziel: höchstens 480. 500 ist die harte Grenze." | "Stay at or under 480. 500 is the hard limit." |
| Reword trigger | `AGENT_REPLY_CHAR_LIMIT = 490` | same constant, shared `prepare_agent_reply` |
| Clamp | same 490 | same 490 |

The prompt's "hard limit" of 500 is never enforced. This change splits the
shared constant so Aurora can be tuned without moving Grok.

## Settings model

Three fields on `AiBehavior`:

```rust
pub struct AiBehavior {
    // existing fields…
    pub reply_target_chars: usize, // default 480 — interpolated into system.md
    pub reply_soft_chars: usize,   // default 490 — rewrite once if over this
    pub reply_hard_chars: usize,   // default 490 — clamp if still over
}
```

- Path: `ai.behavior.reply_*_chars` in `settings.ron`
- Override layer: `Option<usize>` on `AiBehaviorOverrides`; missing keys keep
  the compile-time defaults (existing installs unchanged)
- Hot-reload via `SettingsHandle` — next `!ai` turn, no restart
- Behavior-card reset already clears the whole `ai.behavior` override block

**Defaults keep today's clamp.** Hard defaults to 490, not the prompt's 500, so
a failed rewrite of 491–500 characters still trims until the owner raises it.

## Validation

All three in `1..=500` (Helix character ceiling). Ordering:
`reply_target_chars <= reply_soft_chars <= reply_hard_chars`.

## Runtime

`prepare_agent_reply` takes a `ReplyLimits { soft, hard }` instead of a single
constant:

```rust
pub struct ReplyLimits {
    pub soft: usize, // reword if chars > soft
    pub hard: usize, // clamp if still > hard
}

impl ReplyLimits {
    pub const GROK: Self = Self { soft: 490, hard: 490 };
}
```

Flow: sanitize → if `len <= soft` return → one no-tools rewrite asked to fit
in **soft** characters → if result `> hard`, `clamp_chars` to **hard**. A
rewrite that lands between soft and hard is sent as-is (no second rewrite).

`AiCommand::execute` copies the three Aurora numbers (and builds `ReplyLimits`)
before `drop(snap)`. `@grok` passes `ReplyLimits::GROK` and does not interpolate
length tokens.

## Prompt

`system.md` replaces the hardcoded German numbers with `{reply_target}` and
`{reply_hard}` (same meaning as today's "Ziel / harte Grenze"). Soft stays out
of the prompt — it is post-processing.

Tokens are added to `SubstitutionVars` / `substitute`. Interpolating them into
the system message is a cache-prefix change only when the owner saves the card,
same class as `{model}`. `grok.md` is left baked at 480/500.

## UI

Three `num_row_bounded` rows on the existing **AI · Behavior** card
(`settings/cards/ai_behavior.html`), min 1, max 500:

- **reply_target_chars** — what Aurora is told to aim for
- **reply_soft_chars** — reword once if the reply is longer than this
- **reply_hard_chars** — trim with an ellipsis if still longer after reword

Form fields: `ai_behavior_reply_target_chars`, `ai_behavior_reply_soft_chars`,
`ai_behavior_reply_hard_chars`. Same plumbing as `max_turn_rounds`.

## Testing

**Reply path**

- Over soft → one rewrite; between soft and hard after rewrite is sent without
  clamp; over hard after failed rewrite clamps to hard
- `ReplyLimits::GROK` stays 490/490

**Settings**

- Omitted override keys keep 480/490/490
- Validation rejects out-of-range and `target > soft` / `soft > hard`

**Prompt**

- `{reply_target}` / `{reply_hard}` substitute; unknown tokens left literal
- `grok.md` still contains the baked 480/500 copy

**Web**

- Behavior card renders the three inputs; save/reset round-trip

## Success criteria

1. Owner can set Aurora's target, reword threshold, and trim ceiling from
   `/settings` without a bot restart.
2. Existing installs keep 480 / 490 / 490 until those fields are saved.
3. `@grok` replies still reword and clamp at 490 regardless of Aurora's knobs.
4. Helix never sees an Aurora line over `reply_hard_chars` (capped at 500).
