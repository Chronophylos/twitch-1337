# AI tarot draw tool (`draw_tarot`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an Aurora-only `draw_tarot` AI tool that deals a weighted three-card spread with reversals and returns card meanings plus a reading guide.

**Architecture:** A small engine under `crates/core/src/tarot/` owns the `'static` 78-card deck, weighted draw, spread labels, and `include_str!` reading guide. A thin `ai/tarot_tool.rs` wraps it as a `ToolDefinition` + executor. `command.rs` registers and dispatches the tool on `!ai` turns only (not `@grok`).

**Tech Stack:** Rust workspace (`twitch-1337-core`, `llm` tool types), `rand` 0.10 (`Rng` / `RngExt`), `serde` / `serde_json`, existing `TestBotBuilder` AI integration tests.

**Spec:** `docs/superpowers/specs/2026-07-22-ai-tarot-tool-design.md`

## Global Constraints

- Spread v1: only `three_card` (default when omitted); other values → `unsupported_spread`.
- Positions: Past / Present / Future in that order; model may reinterpret in chat.
- Deck: full 78 RWS-style cards as Rust `'static` data in `tarot/deck.rs` — no TOML, no `twitch-1337-data` tarot assets.
- Major weight `1.5`, minor weight `1.0`; sample without replacement; orientation ~50/50 per card.
- Meanings: Waite-flavored original one-liners (not verbatim PKT, not commercial LWB).
- Always-on for Aurora `!ai` only — never register for `@grok`.
- No settings knob, no chat `!tarot`, no system-prompt appendix, no dreamer use.
- Work in `.worktrees/feature-ai-tarot-tool` on branch `feature/ai-tarot-tool`.
- After Rust edits: `cargo fmt --all`, then `cargo clippy --workspace --all-targets -- -D warnings`, then `cargo nextest run --workspace`.
- Commits: Conventional Commits with stupid subject + serious body (repo `git-commits.mdc`).

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/core/src/tarot/mod.rs` | Module root; re-exports engine API used by the tool |
| `crates/core/src/tarot/deck.rs` | `Card`, `Arcana`, `Suit`, `DECK`, `card_weight` |
| `crates/core/src/tarot/spreads.rs` | `SpreadKind`, position labels for `three_card` |
| `crates/core/src/tarot/draw.rs` | Weighted draw + orientation; injectable RNG |
| `crates/core/src/tarot/guide.rs` | `READING_GUIDE` via `include_str!` |
| `crates/core/src/tarot/reading_guide.md` | Guide body (templates-as-code) |
| `crates/core/src/ai/tarot_tool.rs` | `draw_tarot` definition + `execute_draw_tarot` |
| `crates/core/src/ai/mod.rs` | `pub mod tarot_tool` |
| `crates/core/src/lib.rs` | `pub mod tarot` |
| `crates/core/src/ai/command.rs` | Aurora-only register + `V2Executor` dispatch |
| `crates/core/tests/grok.rs` or `crates/core/tests/ai.rs` | Aurora has tool / grok does not |

---

### Task 1: Deck types + full 78-card `'static` table

**Files:**
- Create: `crates/core/src/tarot/mod.rs`
- Create: `crates/core/src/tarot/deck.rs`
- Modify: `crates/core/src/lib.rs` — add `pub mod tarot;`

**Interfaces:**
- Produces:
  - `pub enum Arcana { Major, Minor }`
  - `pub enum Suit { Wands, Cups, Swords, Pentacles }`
  - `pub struct Card { pub id: &'static str, pub name: &'static str, pub arcana: Arcana, pub suit: Option<Suit>, pub upright: &'static str, pub reversed: &'static str }`
  - `pub static DECK: &[Card]` — exactly 78 entries
  - `pub fn card_weight(card: &Card) -> f64` — `1.5` major, `1.0` minor
  - `pub const MAJOR_WEIGHT: f64 = 1.5;`
  - `pub const MINOR_WEIGHT: f64 = 1.0;`

- [ ] **Step 1: Write the failing tests in `deck.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn deck_has_78_unique_ids() {
        assert_eq!(DECK.len(), 78);
        let ids: HashSet<_> = DECK.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), 78);
    }

    #[test]
    fn every_card_has_nonempty_meanings_and_name() {
        for c in DECK {
            assert!(!c.id.is_empty(), "empty id");
            assert!(!c.name.is_empty(), "{}", c.id);
            assert!(!c.upright.is_empty(), "{}", c.id);
            assert!(!c.reversed.is_empty(), "{}", c.id);
            match c.arcana {
                Arcana::Major => assert!(c.suit.is_none(), "{}", c.id),
                Arcana::Minor => assert!(c.suit.is_some(), "{}", c.id),
            }
        }
    }

    #[test]
    fn weights_match_arcana() {
        let major = DECK.iter().find(|c| matches!(c.arcana, Arcana::Major)).unwrap();
        let minor = DECK.iter().find(|c| matches!(c.arcana, Arcana::Minor)).unwrap();
        assert_eq!(card_weight(major), MAJOR_WEIGHT);
        assert_eq!(card_weight(minor), MINOR_WEIGHT);
    }

    #[test]
    fn deck_has_22_majors_and_56_minors() {
        let majors = DECK.iter().filter(|c| matches!(c.arcana, Arcana::Major)).count();
        let minors = DECK.iter().filter(|c| matches!(c.arcana, Arcana::Minor)).count();
        assert_eq!(majors, 22);
        assert_eq!(minors, 56);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core tarot::deck --lib`

Expected: FAIL (module / `DECK` missing)

- [ ] **Step 3: Implement `mod.rs`, wire `lib.rs`, and author full `DECK`**

`lib.rs` — add next to other `pub mod` lines:

```rust
pub mod tarot;
```

`tarot/mod.rs` (Task 1 only):

```rust
pub mod deck;

pub use deck::{Arcana, Card, Suit, DECK, card_weight, MAJOR_WEIGHT, MINOR_WEIGHT};
```

Later tasks add `draw` / `guide` / `spreads` modules and expand re-exports — do not declare missing modules here.

`deck.rs` skeleton + weight helper:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arcana {
    Major,
    Minor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suit {
    Wands,
    Cups,
    Swords,
    Pentacles,
}

#[derive(Debug, Clone, Copy)]
pub struct Card {
    pub id: &'static str,
    pub name: &'static str,
    pub arcana: Arcana,
    pub suit: Option<Suit>,
    pub upright: &'static str,
    pub reversed: &'static str,
}

pub const MAJOR_WEIGHT: f64 = 1.5;
pub const MINOR_WEIGHT: f64 = 1.0;

pub fn card_weight(card: &Card) -> f64 {
    match card.arcana {
        Arcana::Major => MAJOR_WEIGHT,
        Arcana::Minor => MINOR_WEIGHT,
    }
}

pub static DECK: &[Card] = &[
    // --- 22 majors (ids major_00_fool … major_21_world) ---
    Card {
        id: "major_00_fool",
        name: "The Fool",
        arcana: Arcana::Major,
        suit: None,
        upright: "A fresh leap — trust the open road more than the map.",
        reversed: "Hesitation or recklessness; look before the leap, or take it already.",
    },
    // … author ALL remaining majors and all 56 minors in this same shape …
];
```

**Required inventory (every id must appear once):**

Majors: `major_00_fool` … `major_21_world` (standard RWS names: Fool, Magician, High Priestess, Empress, Emperor, Hierophant, Lovers, Chariot, Strength, Hermit, Wheel of Fortune, Justice, Hanged Man, Death, Temperance, Devil, Tower, Star, Moon, Sun, Judgement, World).

Minors per suit (`wands`, `cups`, `swords`, `pentacles`): `01` Ace through `10`, then `page`, `knight`, `queen`, `king` — ids like `wands_01`, `wands_page`, `cups_king`, etc.

Meaning quality bar: one chatty sentence each for upright/reversed; Waite-flavored original wording; no pasted guidebook text.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p twitch-1337-core tarot::deck --lib`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/tarot/mod.rs crates/core/src/tarot/deck.rs crates/core/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(tarot): the whole deck just got baked into rust no toml mid 🃏🔥

Add the tarot engine module with a typed 78-card static table and
major/minor draw weights. Meanings are original Waite-flavored
one-liners living next to the engine per ADR-0002/0006 guidance.

EOF
)"
```

---

### Task 2: Reading guide (`include_str!`)

**Files:**
- Create: `crates/core/src/tarot/reading_guide.md`
- Create: `crates/core/src/tarot/guide.rs`
- Modify: `crates/core/src/tarot/mod.rs` — `pub mod guide;` + re-export

**Interfaces:**
- Produces: `pub const READING_GUIDE: &str` — non-empty, includes the instructional bullets from the spec

- [ ] **Step 1: Write the failing test in `guide.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::READING_GUIDE;

    #[test]
    fn reading_guide_mentions_positions_and_no_inventing() {
        assert!(!READING_GUIDE.is_empty());
        let lower = READING_GUIDE.to_lowercase();
        assert!(lower.contains("past"));
        assert!(lower.contains("present"));
        assert!(lower.contains("future"));
        assert!(lower.contains("reversed") || lower.contains("upright"));
        assert!(
            lower.contains("invent") || lower.contains("do not invent") || lower.contains("don't invent")
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core tarot::guide --lib`

Expected: FAIL (module missing)

- [ ] **Step 3: Add `reading_guide.md` and `guide.rs`**

`reading_guide.md`:

```markdown
# How to read this draw

- Cards are ordered Past → Present → Future (left to right).
- Upright leans toward the card's upright meaning; reversed leans toward its reversed meaning.
- You may remap the three labels to fit the asker's question, but do not change which card sits in which slot.
- Do not invent cards, swaps, or extra pulls beyond this draw.
- Keep the Twitch reply concise and in-character; entertainment tone is fine.
```

`guide.rs`:

```rust
pub const READING_GUIDE: &str = include_str!("reading_guide.md");
```

Wire `pub mod guide;` in `tarot/mod.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p twitch-1337-core tarot::guide --lib`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/tarot/reading_guide.md crates/core/src/tarot/guide.rs crates/core/src/tarot/mod.rs
git commit -m "$(cat <<'EOF'
feat(tarot): reading guide got include_str'd and its giving oracle core ✨📜

Bake the model-facing how-to-read instructions as markdown-as-code beside
the engine so successful tool results can attach a stable guide string.

EOF
)"
```

---

### Task 3: Spreads + weighted draw

**Files:**
- Create: `crates/core/src/tarot/spreads.rs`
- Create: `crates/core/src/tarot/draw.rs`
- Modify: `crates/core/src/tarot/mod.rs` — modules + re-exports

**Interfaces:**
- Consumes: `DECK`, `card_weight`, `Card`
- Produces:
  - `pub enum SpreadKind { ThreeCard }` with `pub fn parse_spread(raw: Option<&str>) -> Result<SpreadKind, SpreadError>`
  - `pub const THREE_CARD_LABELS: [&str; 3] = ["Past", "Present", "Future"];`
  - `pub enum Orientation { Upright, Reversed }`
  - `pub struct DrawnCard { pub card: &'static Card, pub orientation: Orientation, pub label: &'static str }`
  - `pub struct ThreeCardDraw { pub positions: [DrawnCard; 3] }`
  - `pub fn draw_three_card(rng: &mut impl rand::Rng) -> ThreeCardDraw`
  - `DrawnCard::meaning(&self) -> &'static str` — picks upright/reversed blurb

- [ ] **Step 1: Write the failing tests**

In `spreads.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_three_card_ok() {
        assert!(matches!(parse_spread(None).unwrap(), SpreadKind::ThreeCard));
        assert!(matches!(
            parse_spread(Some("three_card")).unwrap(),
            SpreadKind::ThreeCard
        ));
    }

    #[test]
    fn unknown_spread_errors() {
        assert!(parse_spread(Some("celtic_cross")).is_err());
        assert!(parse_spread(Some("")).is_err());
    }

    #[test]
    fn three_card_labels_order() {
        assert_eq!(THREE_CARD_LABELS, ["Past", "Present", "Future"]);
    }
}
```

In `draw.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::SmallRng;
    use std::collections::HashSet;

    #[test]
    fn draw_has_three_distinct_cards_and_labels() {
        let mut rng = SmallRng::seed_from_u64(42);
        let draw = draw_three_card(&mut rng);
        let ids: HashSet<_> = draw.positions.iter().map(|p| p.card.id).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(draw.positions[0].label, "Past");
        assert_eq!(draw.positions[1].label, "Present");
        assert_eq!(draw.positions[2].label, "Future");
    }

    #[test]
    fn meaning_tracks_orientation() {
        let card = &crate::tarot::DECK[0];
        let up = DrawnCard {
            card,
            orientation: Orientation::Upright,
            label: "Past",
        };
        let rev = DrawnCard {
            card,
            orientation: Orientation::Reversed,
            label: "Past",
        };
        assert_eq!(up.meaning(), card.upright);
        assert_eq!(rev.meaning(), card.reversed);
    }

    #[test]
    fn majors_appear_more_often_than_uniform_share() {
        // Uniform major share = 22/78 ≈ 0.282. With 1.5 vs 1.0 weights,
        // expected major probability per pick from a full deck is
        // (22*1.5)/(22*1.5 + 56*1.0) = 33/89 ≈ 0.371.
        // Over many first-card draws from a fresh deck, majors should land
        // clearly above the uniform baseline.
        let mut rng = SmallRng::seed_from_u64(7);
        const N: usize = 3000;
        let mut major_first = 0usize;
        for _ in 0..N {
            let draw = draw_three_card(&mut rng);
            if matches!(draw.positions[0].card.arcana, crate::tarot::Arcana::Major) {
                major_first += 1;
            }
        }
        let rate = major_first as f64 / N as f64;
        assert!(
            rate > 0.32,
            "expected soft major bias on first card, got rate={rate}"
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core tarot::spreads tarot::draw --lib`

Expected: FAIL

- [ ] **Step 3: Implement spreads + draw**

`spreads.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpreadKind {
    ThreeCard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpreadError;

pub const THREE_CARD_LABELS: [&str; 3] = ["Past", "Present", "Future"];

pub fn parse_spread(raw: Option<&str>) -> Result<SpreadKind, SpreadError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(SpreadKind::ThreeCard),
        Some("three_card") => Ok(SpreadKind::ThreeCard),
        Some(_) => Err(SpreadError),
    }
}
```

`draw.rs` (weighted sample without replacement + 50/50 orientation):

```rust
use rand::RngExt as _;
use rand::{Rng, seq::IndexedRandom};

use super::deck::{Card, DECK, card_weight};
use super::spreads::THREE_CARD_LABELS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Upright,
    Reversed,
}

#[derive(Debug, Clone, Copy)]
pub struct DrawnCard {
    pub card: &'static Card,
    pub orientation: Orientation,
    pub label: &'static str,
}

impl DrawnCard {
    pub fn meaning(&self) -> &'static str {
        match self.orientation {
            Orientation::Upright => self.card.upright,
            Orientation::Reversed => self.card.reversed,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThreeCardDraw {
    pub positions: [DrawnCard; 3],
}

pub fn draw_three_card(rng: &mut impl Rng) -> ThreeCardDraw {
    let mut pool: Vec<&'static Card> = DECK.iter().collect();
    let mut positions = [DrawnCard {
        card: DECK[0],
        orientation: Orientation::Upright,
        label: THREE_CARD_LABELS[0],
    }; 3];

    for (i, slot) in positions.iter_mut().enumerate() {
        let idx = pool
            .choose_weighted(rng, |c| card_weight(c))
            .expect("non-empty weighted pool")
            .id;
        // choose_weighted returns &&Card; locate index to remove
        let pos = pool.iter().position(|c| c.id == idx).expect("drawn card in pool");
        let card = pool.swap_remove(pos);
        let orientation = if rng.random::<bool>() {
            Orientation::Upright
        } else {
            Orientation::Reversed
        };
        *slot = DrawnCard {
            card,
            orientation,
            label: THREE_CARD_LABELS[i],
        };
    }

    ThreeCardDraw { positions }
}
```

If `choose_weighted` + id lookup is awkward under clippy, prefer index-based selection:

```rust
let weights: Vec<f64> = pool.iter().map(|c| card_weight(c)).collect();
let total: f64 = weights.iter().sum();
let mut roll = rng.random::<f64>() * total;
let mut pick = weights.len() - 1;
for (i, w) in weights.iter().enumerate() {
    if roll < *w {
        pick = i;
        break;
    }
    roll -= *w;
}
let card = pool.swap_remove(pick);
```

Use whichever compiles cleanly under `-D warnings`. Fix `IndexedRandom` import path to match rand 0.10 in this workspace if the sketch drifts.

Update `tarot/mod.rs` re-exports to match the **Interfaces** list above.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p twitch-1337-core tarot --lib`

Expected: PASS (deck + guide + spreads + draw)

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/tarot/spreads.rs crates/core/src/tarot/draw.rs crates/core/src/tarot/mod.rs
git commit -m "$(cat <<'EOF'
feat(tarot): weighted pulls are lowkey cooking with soft major bias ⚖️🃏

Implement three_card spread parsing and without-replacement weighted
draws with independent reversals, injectable RNG for tests, and Past /
Present / Future labels.

EOF
)"
```

---

### Task 4: `draw_tarot` tool wrapper

**Files:**
- Create: `crates/core/src/ai/tarot_tool.rs`
- Modify: `crates/core/src/ai/mod.rs` — `pub mod tarot_tool;`

**Interfaces:**
- Consumes: `draw_three_card`, `parse_spread`, `READING_GUIDE`, `DrawnCard::meaning`
- Produces:
  - `pub const TAROT_TOOL_NAME: &str = "draw_tarot";`
  - `pub fn tarot_tool() -> ToolDefinition`
  - `pub fn execute_draw_tarot(call: &ToolCall) -> ToolResultMessage` (sync is fine; match doener’s async signature only if needed for call-site uniformity — prefer `async fn` returning immediately like a thin wrapper if `V2Executor` awaits)

- [ ] **Step 1: Write the failing tests in `tarot_tool.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use llm::ToolCall;
    use serde_json::json;

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: TAROT_TOOL_NAME.into(),
            arguments: args,
            arguments_parse_error: None,
        }
    }

    #[test]
    fn tool_def_name() {
        assert_eq!(tarot_tool().name, TAROT_TOOL_NAME);
    }

    #[tokio::test]
    async fn empty_args_returns_three_positions_and_guide() {
        let msg = execute_draw_tarot(&call(json!({}))).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["spread"], "three_card");
        assert!(v.get("question").is_none());
        assert_eq!(v["positions"].as_array().unwrap().len(), 3);
        assert_eq!(v["positions"][0]["label"], "Past");
        assert!(v["guide"].as_str().unwrap().contains("Past"));
        assert!(v["positions"][0]["meaning"].as_str().unwrap().len() > 0);
    }

    #[tokio::test]
    async fn question_is_echoed() {
        let msg = execute_draw_tarot(&call(json!({"question": "pass the exam?"}))).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["question"], "pass the exam?");
    }

    #[tokio::test]
    async fn unsupported_spread_errors() {
        let msg = execute_draw_tarot(&call(json!({"spread": "celtic_cross"}))).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["error"], "unsupported_spread");
    }

    #[tokio::test]
    async fn malformed_args_invalid_arguments() {
        let mut c = call(json!({}));
        c.arguments = json!("nope");
        // Force deserialize failure via parse_args path: set arguments to a
        // non-object that DrawTarotArgs rejects, or use arguments_parse_error.
        c.arguments = json!({"spread": 123});
        let msg = execute_draw_tarot(&c).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["error"], "invalid_arguments");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p twitch-1337-core ai::tarot_tool --lib`

Expected: FAIL

- [ ] **Step 3: Implement `tarot_tool.rs`**

```rust
use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use rand::rng;
use serde::Deserialize;
use serde_json::json;

use crate::tarot::{
    Orientation, READING_GUIDE, SpreadKind, draw_three_card, parse_spread,
};

pub const TAROT_TOOL_NAME: &str = "draw_tarot";

#[derive(Debug, Deserialize)]
struct DrawTarotArgs {
    #[serde(default)]
    spread: Option<String>,
    #[serde(default)]
    question: Option<String>,
}

pub fn tarot_tool() -> ToolDefinition {
    ToolDefinition {
        name: TAROT_TOOL_NAME.into(),
        description: "Use when a viewer wants a tarot reading or card pull. Draws a spread and \
            returns cards, meanings, and a short how-to-read guide. Prefer this over inventing cards."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "spread": {
                    "type": "string",
                    "description": "Named spread. v1 supports only \"three_card\" (default)."
                },
                "question": {
                    "type": "string",
                    "description": "Optional asker question; echoed into the result and does not change the shuffle."
                }
            }
        }),
    }
}

pub async fn execute_draw_tarot(call: &ToolCall) -> ToolResultMessage {
    let args = match call.parse_args::<DrawTarotArgs>() {
        Ok(a) => a,
        Err(e) => {
            return ToolResultMessage::for_call(
                call,
                json!({"error": "invalid_arguments", "details": e.to_string()}).to_string(),
            );
        }
    };

    let spread = match parse_spread(args.spread.as_deref()) {
        Ok(SpreadKind::ThreeCard) => SpreadKind::ThreeCard,
        Err(_) => {
            return ToolResultMessage::for_call(
                call,
                json!({"error": "unsupported_spread"}).to_string(),
            );
        }
    };
    let _ = spread;

    let draw = draw_three_card(&mut rng());
    let positions: Vec<_> = draw
        .positions
        .iter()
        .map(|p| {
            json!({
                "label": p.label,
                "id": p.card.id,
                "card": p.card.name,
                "orientation": match p.orientation {
                    Orientation::Upright => "upright",
                    Orientation::Reversed => "reversed",
                },
                "meaning": p.meaning(),
            })
        })
        .collect();

    let mut body = json!({
        "spread": "three_card",
        "positions": positions,
        "guide": READING_GUIDE,
    });
    if let Some(q) = args
        .question
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        body["question"] = json!(q);
    }

    ToolResultMessage::for_call(call, body.to_string())
}
```

Add `pub mod tarot_tool;` to `ai/mod.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p twitch-1337-core ai::tarot_tool --lib`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/ai/tarot_tool.rs crates/core/src/ai/mod.rs
git commit -m "$(cat <<'EOF'
feat(ai): draw_tarot tool just unlocked and chat can get read fr 🔮💀

Add the Aurora-facing tool definition and executor that shuffles via the
tarot engine and returns JSON positions, meanings, and the reading guide.

EOF
)"
```

---

### Task 5: Wire into `command.rs` + Aurora/grok registration test

**Files:**
- Modify: `crates/core/src/ai/command.rs` — tool list + `V2Executor::execute`
- Modify: `crates/core/tests/grok.rs` — add asymmetry test (or new test in `crates/core/tests/ai.rs` if preferred)

**Interfaces:**
- Consumes: `tarot_tool`, `TAROT_TOOL_NAME`, `execute_draw_tarot`
- Produces: Aurora tool list includes `draw_tarot`; grok list does not; executor handles the name

- [ ] **Step 1: Write the failing integration test**

Append to `crates/core/tests/grok.rs` (same cooldown sidestep pattern as `grok_registers_the_read_memory_tool_and_ai_does_not`):

```rust
#[tokio::test]
async fn aurora_registers_draw_tarot_and_grok_does_not() {
    let bot = TestBotBuilder::new()
        .with_ai()
        .with_settings(|o| {
            o.ai.history.length = Some(0);
        })
        .spawn()
        .await;
    bot.llm.push_tool_message("checked");
    bot.llm.push_tool_message("ok");

    let mut bot = bot;
    bot.send("alice", "@grok check this").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;
    bot.send("bob", "!ai sag hallo").await;
    let _ = bot.expect_reply(Duration::from_secs(2)).await;

    let calls = bot.llm.tool_calls();
    let grok_tools: Vec<_> = calls[0].tools.iter().map(|t| t.name.as_str()).collect();
    let ai_tools: Vec<_> = calls[1].tools.iter().map(|t| t.name.as_str()).collect();

    assert!(
        ai_tools.contains(&"draw_tarot"),
        "aurora should expose draw_tarot: {ai_tools:?}"
    );
    assert!(
        !grok_tools.contains(&"draw_tarot"),
        "grok must not get draw_tarot: {grok_tools:?}"
    );

    bot.shutdown().await;
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p twitch-1337-core aurora_registers_draw_tarot`

Expected: FAIL (assertion — tool missing on Aurora)

- [ ] **Step 3: Wire registration + dispatch**

In `command.rs` where tools are assembled (~537–548), **after** memory tools / doener, only for Aurora:

```rust
        let mut tools = if grok_alias {
            grok_chat_turn_tools()
        } else {
            chat_turn_tools()
        };
        tools.push(crate::ai::doener_tool::doener_tool());
        if !grok_alias {
            tools.push(crate::ai::tarot_tool::tarot_tool());
        }
        if self.web.is_some() {
            tools.extend(content::ai_tools());
        }
        // … emotes unchanged …
```

In `V2Executor::execute`, dispatch before memory fallback (alongside doener):

```rust
        if call.name == crate::ai::tarot_tool::TAROT_TOOL_NAME {
            return crate::ai::tarot_tool::execute_draw_tarot(call).await;
        }
```

Update the `V2Executor` doc comment to mention `draw_tarot`.

No new fields on `V2Executor` — tarot needs no client.

- [ ] **Step 4: Run tests to verify they pass**

Run:

```bash
cargo nextest run -p twitch-1337-core aurora_registers_draw_tarot grok_registers_the_read_memory
cargo nextest run -p twitch-1337-core tarot ai::tarot_tool --lib
```

Expected: PASS

- [ ] **Step 5: Full CI gate + commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

```bash
git add crates/core/src/ai/command.rs crates/core/tests/grok.rs
git commit -m "$(cat <<'EOF'
feat(ai): aurora got the tarot hookup and grok stays unenlightened 🔮❌

Register and dispatch draw_tarot on !ai turns only, and lock the
Aurora/grok tool asymmetry with an integration test.

EOF
)"
```

---

## Spec coverage checklist

| Spec requirement | Task |
|---|---|
| Engine under `crates/core/src/tarot/` | 1–3 |
| Rust `'static` 78-card deck, no data-crate TOML | 1 |
| `include_str!` reading guide | 2 |
| `three_card` only + Past/Present/Future | 3 |
| Major weight 1.5 / minor 1.0, w/o replacement, reversals | 3 |
| `draw_tarot` tool JSON contract + errors | 4 |
| Aurora-only registration + executor wiring | 5 |
| Unit + integration tests | 1–5 |
| Non-goals (no settings, no grok, no `!tarot`, no appendix) | honored by omission |

## Plan self-review notes

- No `deck.toml` / data-crate paths remain.
- Types (`DrawnCard`, `Orientation`, `SpreadKind`, `TAROT_TOOL_NAME`) are consistent across tasks 3–5.
- `choose_weighted` sketch may need a rand-0.10 import tweak — Task 3 allows the cumulative-weight fallback.
- Full 78 meanings are authored in Task 1 (inventory listed; blurbs are implementation content, not deferred to a later task).
