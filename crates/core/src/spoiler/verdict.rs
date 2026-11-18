//! Question template and mapping from a Decision answer to a chat verdict.

use std::collections::BTreeMap;

use indexmap::IndexMap;
use llm::decision::{Answer, Question, StatePart};
use serde::Deserialize;
use twitch_1337_data::prompts::SPOILER_QUESTION;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Unrelated,
    Marketing,
    News,
    Gameplay,
    Story,
    Insufficient,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Unrelated,
        Category::Marketing,
        Category::News,
        Category::Gameplay,
        Category::Story,
        Category::Insufficient,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Category::Unrelated => "unrelated",
            Category::Marketing => "marketing",
            Category::News => "news",
            Category::Gameplay => "gameplay",
            Category::Story => "story",
            Category::Insufficient => "insufficient",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.key() == key)
    }

    fn label(self) -> &'static str {
        match self {
            Category::Unrelated => "Off-Topic",
            Category::Marketing => "Marketing",
            Category::News => "News",
            Category::Gameplay => "Gameplay",
            Category::Story => "Story",
            Category::Insufficient => "Unklar",
        }
    }
}

#[derive(Deserialize)]
struct Template {
    instructions: String,
    criteria: IndexMap<String, String>,
}

fn parse_template() -> Template {
    // Baked asset; a parse failure is a build-time bug caught by tests.
    ron::from_str(SPOILER_QUESTION).expect("spoiler question.ron is valid")
}

/// The baked question. It refers to the `game` and `page` keys of the
/// state built by [`state`].
pub fn build_question() -> Question {
    let tpl = parse_template();
    Question::Choice {
        instructions: tpl.instructions,
        criteria: tpl.criteria,
    }
}

/// Decision state: `game` (already validated) and the page text, as named
/// fields the question refers to. The API docs put the subject in state and
/// keep the question generic.
pub fn state(game: &str, page: &str) -> StatePart {
    StatePart::Fields(BTreeMap::from([
        ("game".to_owned(), game.to_owned()),
        ("page".to_owned(), page.to_owned()),
    ]))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Sure { category: Category, pct: u8 },
    Unsure { top: [(Category, u8); 2] },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerdictError {
    #[error("unknown choice {0:?}")]
    UnknownChoice(String),
    #[error("no probability for choice {0:?}")]
    MissingProbability(&'static str),
    #[error("unsupported answer type")]
    UnsupportedAnswer,
}

fn pct(p: f64) -> u8 {
    (p * 100.0).round().clamp(0.0, 100.0) as u8
}

pub fn verdict(answer: &Answer, floor_pct: u8) -> Result<Verdict, VerdictError> {
    let (choice, probs) = match answer {
        Answer::Refusal => {
            return Ok(Verdict::Sure {
                category: Category::Insufficient,
                pct: 0,
            });
        }
        Answer::Other => return Err(VerdictError::UnsupportedAnswer),
        Answer::Choice {
            choice,
            probabilities,
            ..
        } => (choice, probabilities),
    };
    let category =
        Category::from_key(choice).ok_or_else(|| VerdictError::UnknownChoice(choice.clone()))?;
    let p = pct(*probs
        .get(category.key())
        .ok_or(VerdictError::MissingProbability(category.key()))?);
    if p >= floor_pct {
        return Ok(Verdict::Sure { category, pct: p });
    }
    let mut ranked: Vec<(Category, u8)> = Category::ALL
        .into_iter()
        .map(|c| (c, pct(probs.get(c.key()).copied().unwrap_or(0.0))))
        .collect();
    // Stable sort keeps Category::ALL order on ties.
    ranked.sort_by_key(|a| std::cmp::Reverse(a.1));
    Ok(Verdict::Unsure {
        top: [ranked[0], ranked[1]],
    })
}

/// Chat line without the `@user` prefix.
pub fn render(v: &Verdict, game: &str) -> String {
    match *v {
        Verdict::Sure { category, pct } => match category {
            Category::Unrelated => format!("⬜ scheint nicht über {game} zu sein"),
            Category::Marketing => {
                format!("🟩 sieht nach offiziellem Material aus ({game}, {pct}%)")
            }
            Category::News => format!("🟩 nur News/Gerede ({game}, {pct}%)"),
            Category::Gameplay => format!("🟨 Gameplay-Details ({game}, {pct}%)"),
            Category::Story => format!("🟥 Story-Spoiler ({game}, {pct}%) – nicht klicken"),
            Category::Insufficient => "⬜ kann ich nicht beurteilen".to_string(),
        },
        Verdict::Unsure {
            top: [(a, pa), (b, pb)],
        } => format!(
            "🤔 unsicher ({game}): {} {pa}% / {} {pb}%",
            a.label(),
            b.label()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(c: &str, probs: &[(&str, f64)], confidence: Option<f64>) -> Answer {
        Answer::Choice {
            choice: c.into(),
            probabilities: probs.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            confidence,
        }
    }

    #[test]
    fn ron_keys_match_category_enum() {
        let tpl = parse_template();
        // Same order too: the RON order is the wire order.
        let want: Vec<_> = Category::ALL.iter().map(|c| c.key().to_string()).collect();
        let got: Vec<_> = tpl.criteria.keys().cloned().collect();
        assert_eq!(got, want);
    }

    #[test]
    fn question_refers_to_state_keys() {
        let Question::Choice { instructions, .. } = build_question();
        assert!(instructions.contains("`game`") && instructions.contains("`page`"));
        let StatePart::Fields(f) = state("GTA 6", "text") else {
            panic!("fields state");
        };
        assert_eq!(f["game"], "GTA 6");
        assert_eq!(f["page"], "text");
    }

    #[test]
    fn sure_uses_probability_not_confidence() {
        let a = choice("story", &[("story", 0.87), ("gameplay", 0.1)], Some(0.2));
        assert_eq!(
            verdict(&a, 50),
            Ok(Verdict::Sure {
                category: Category::Story,
                pct: 87
            })
        );
    }

    #[test]
    fn below_floor_is_unsure_with_top_two() {
        let a = choice(
            "story",
            &[
                ("story", 0.46),
                ("gameplay", 0.38),
                ("marketing", 0.1),
                ("unrelated", 0.04),
                ("insufficient", 0.02),
            ],
            None,
        );
        assert_eq!(
            verdict(&a, 50),
            Ok(Verdict::Unsure {
                top: [(Category::Story, 46), (Category::Gameplay, 38)]
            })
        );
    }

    #[test]
    fn refusal_is_insufficient() {
        assert_eq!(
            verdict(&Answer::Refusal, 50),
            Ok(Verdict::Sure {
                category: Category::Insufficient,
                pct: 0
            })
        );
    }

    #[test]
    fn unknown_choice_other_and_missing_prob_are_errors() {
        let spicy = choice("spicy", &[("spicy", 0.9)], None);
        assert!(matches!(
            verdict(&spicy, 50),
            Err(VerdictError::UnknownChoice(_))
        ));
        assert!(verdict(&Answer::Other, 50).is_err());
        let missing = choice("story", &[("gameplay", 0.9)], None);
        assert!(verdict(&missing, 50).is_err());
    }

    #[test]
    fn render_marks_severity_per_category() {
        // The leading colour marker is the at-a-glance safety signal in chat.
        for (category, pct, marker) in [
            (Category::Story, 87, "🟥"),
            (Category::Gameplay, 70, "🟨"),
            (Category::News, 81, "🟩"),
            (Category::Marketing, 80, "🟩"),
            (Category::Unrelated, 90, "⬜"),
            (Category::Insufficient, 0, "⬜"),
        ] {
            let out = render(&Verdict::Sure { category, pct }, "GTA 6");
            assert!(out.starts_with(marker), "{category:?}: {out}");
        }
        let unsure = render(
            &Verdict::Unsure {
                top: [(Category::Story, 46), (Category::Unrelated, 38)],
            },
            "GTA 6",
        );
        assert!(
            unsure.starts_with("🤔") && unsure.contains("46%") && unsure.contains("38%"),
            "{unsure}"
        );
    }
}
