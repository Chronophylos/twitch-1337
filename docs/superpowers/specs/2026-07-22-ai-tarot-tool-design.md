# AI tarot draw tool (`draw_tarot`)

**Status:** Spec  
**Date:** 2026-07-22  
**Branch:** `feature/ai-tarot-tool`

## Goal

Give the Aurora `!ai` agent a `draw_tarot` tool that deals a random three-card
spread (with reversals), returns short card meanings, and includes a reading
guide so the model can produce a grounded chat reading instead of inventing
cards.

## Non-goals (v1)

- `@grok` registration
- Dashboard / settings toggle (always on when AI runs for Aurora)
- Dreamer / ritual use
- Spreads beyond `three_card` (the arg enum exists for later growth)
- Card images, overlay, or dashboard UI
- Chat command (`!tarot`) — tool only
- Persisting draws, daily-card streaks, or history
- System-prompt appendix for tarot (guide lives only in the tool result)
- Claiming official Rider–Waite / US Games affiliation
- Verbatim modern guidebook or little-white-book copy

## Decisions

| Topic | Choice |
|---|---|
| Spread | Named `spread` arg; v1 allows only `three_card` (default) |
| Positions | Fixed labels Past / Present / Future; model may reinterpret for the asker |
| Deck | Full 78-card RWS-style deck |
| Major weight | Soft bias: each Major weight `1.5`, each Minor `1.0` |
| Reversals | Yes — independent ~50/50 upright/reversed per card |
| Sampling | Without replacement within a draw |
| Availability | Always on for `!ai` only |
| Args | Optional `question`, optional `spread` |
| Meanings | Waite-flavored original one-liners (not PD verbatim, not modern LWB) |
| Deck storage | Rust `'static` table in `tarot/deck.rs` (not TOML / data-crate asset) |
| Reading guide | `include_str!` markdown next to the engine (ADR-0002 spirit) |
| Architecture | Small tarot engine + thin AI tool wrapper |

## Architecture

```
!ai turn (Aurora)
   │
   ├─ tools.push(draw_tarot)          // not registered for @grok
   ▼
model may call draw_tarot(spread?, question?)
   ▼
V2Executor → ai/tarot_tool.rs
   ▼
crates/core/src/tarot/   (engine)
   ├─ deck      &'static [Card; 78] + upright/reversed blurbs
   ├─ draw      weighted sample w/o replacement + orientation
   ├─ spreads   three_card → Past / Present / Future
   └─ guide     include_str!(reading_guide.md)
   ▼
ToolResultMessage (JSON string: cards + meanings + guide)
   ▼
model writes short in-character chat reply from that payload only
```

Domain logic stays in `crates/core`. The `llm` crate remains primitives-only
(`ToolDefinition` / `ToolCall` / `run_agent`). No `twitch-1337-data` tarot
assets — this table is tiny and domain-local (fat-asset bake / data-crate
centralization does not apply).

**Format rationale:** [ADR-0006](../../adr/0006-persistence-media-defaults.md)
covers runtime on-disk persistence only and explicitly excludes compile-time
embeds; its TOML row is bootstrap config, not baked card tables. Model-facing
static prose follows [ADR-0002](../../adr/0002-prompt-templates-as-code.md):
repo constants via `include_str!`, single source of truth in git.

### Module layout

```
crates/core/src/tarot/
  mod.rs
  deck.rs            # Card, Arcana, Suit; pub static DECK: &[Card]
  draw.rs            # weighted draw + orientation
  spreads.rs         # SpreadKind, position labels
  guide.rs           # pub const READING_GUIDE: &str = include_str!(…)
  reading_guide.md   # guide body (edited as markdown, baked as code)

crates/core/src/ai/tarot_tool.rs
  # ToolDefinition, args, execute_draw_tarot → ToolResultMessage
```

### Wiring

Mirror `doener_index` / `search_emotes`, with an Aurora-only gate:

1. `ai/mod.rs` — `pub mod tarot_tool;`
2. `ai/command.rs` — push `tarot_tool()` into the tool list on Aurora turns only
3. `V2Executor::execute` — dispatch `draw_tarot` before memory fallback
4. No settings resolve path; no `Services` HTTP client

## Tool contract

| Field | Value |
|---|---|
| Name | `draw_tarot` |
| Description | Use when a viewer wants a tarot reading or card pull. Draws a spread and returns cards, meanings, and a short how-to-read guide. Prefer this over inventing cards. |
| Args | `{ "spread"?: "three_card", "question"?: string }` |
| Default | `spread = three_card` when omitted |

### Success result (JSON string)

```json
{
  "spread": "three_card",
  "question": "Will I pass the exam?",
  "positions": [
    {
      "label": "Past",
      "id": "major_16_tower",
      "card": "The Tower",
      "orientation": "reversed",
      "meaning": "…"
    },
    {
      "label": "Present",
      "id": "cups_02",
      "card": "Two of Cups",
      "orientation": "upright",
      "meaning": "…"
    },
    {
      "label": "Future",
      "id": "wands_07",
      "card": "Seven of Wands",
      "orientation": "upright",
      "meaning": "…"
    }
  ],
  "guide": "<READING_GUIDE body>"
}
```

Omit the `question` key when the model did not pass one.

### Error results

Same JSON-error style as `doener_index`:

| `error` | When |
|---|---|
| `invalid_arguments` | Args fail schema / parse |
| `unsupported_spread` | `spread` present but not `three_card` |

## Deck data & meanings

`deck.rs` holds a `'static` slice of 78 `Card` values. Each entry:

- Stable `id` (e.g. `major_16_tower`, `wands_03`)
- Display `name`
- `arcana`: `major` | `minor`
- `suit` for minors: `wands` | `cups` | `swords` | `pentacles` (`None` for majors)
- `upright` / `reversed`: short Waite-flavored one-liners written for chat
  (tone of the tradition, original wording — not a pasted *Pictorial Key*
  excerpt and not a commercial LWB)

Module docs note: original paraphrases for bot use; “Rider–Waite” used
descriptively only. No runtime parse step — compile-time typed data.

## Draw rules

1. Build a weight table: Major `1.5`, Minor `1.0`
2. Sample three distinct cards without replacement (weighted)
3. For each card, flip orientation upright/reversed with equal probability
4. Map onto spread positions in order
5. Attach the orientation-appropriate meaning blurb
6. Attach the static reading guide

RNG: workspace `rand` usage consistent with the rest of the repo. Tests inject
a seeded RNG (or equivalent seam) so draws are deterministic where needed.

## Reading guide (content goals)

`reading_guide.md` beside the engine, exposed as `READING_GUIDE` via
`include_str!` (same “templates as code” posture as ADR-0002). Content covers:

- Read positions left → right (Past → Present → Future)
- Upright vs reversed
- Model may remap labels to fit the asker’s question
- Do not invent cards beyond the draw
- Keep Twitch replies concise

Shipped only inside successful tool results (not a permanent system appendix).

## Testing

### Engine unit tests

- `DECK` has 78 unique ids; every card has non-empty upright + reversed text
- Weighting: majors appear more often than uniform (seeded / weight-table assert)
- No duplicate card ids in one spread
- Orientation roughly 50/50 over many trials, or forced via injectable RNG
- `three_card` labels are Past / Present / Future in order

### Tool wrapper tests

- Default / empty args → valid three-card JSON + guide
- `spread: "celtic_cross"` → `unsupported_spread`
- Malformed args → `invalid_arguments`
- Optional `question` echoed when present; absent when omitted

### Integration (light)

- Aurora `!ai` tool list includes `draw_tarot`
- Grok tool list does **not** (same pattern as `read_memory` asymmetry tests)

No settings/dashboard tests.

## Chat UX

The model turns the JSON into a short, in-character reading. It must not invent
cards beyond the draw. Entertainment tone is fine; the tool supplies the
grounding.

## Future extensions (out of scope now)

- Additional `SpreadKind` variants (e.g. `single`, `celtic_cross`)
- Settings toggle
- `@grok` opt-in
- Stronger/weaker major weight as a setting
- Optional chat `!tarot` command reusing the engine
