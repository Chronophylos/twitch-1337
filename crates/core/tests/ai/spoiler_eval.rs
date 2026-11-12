//! Live eval for `!spoiler`: classifies hand-written fixtures through the real
//! Decisions endpoint and prints a confusion matrix. Run manually:
//! `cargo nextest run -p twitch-1337-core --test ai spoiler_eval --run-ignored only --no-capture`

use twitch_1337_core as twitch_1337;

use std::collections::BTreeMap;
use std::time::Duration;

use llm::decision::{Answer, DecisionRequest, StatePart, decide, decisions_endpoint};
use twitch_1337::spoiler::verdict::{Category, UNTRUSTED_PREFIX, build_question};

const DEFAULT_MODEL: &str = "nace-ai/drex-max";

const FIXTURES: [(&str, &str); 20] = [
    ("unrelated_1", include_str!("spoiler_eval/unrelated_1.txt")),
    ("unrelated_2", include_str!("spoiler_eval/unrelated_2.txt")),
    ("unrelated_3", include_str!("spoiler_eval/unrelated_3.txt")),
    ("unrelated_4", include_str!("spoiler_eval/unrelated_4.txt")),
    ("marketing_1", include_str!("spoiler_eval/marketing_1.txt")),
    ("marketing_2", include_str!("spoiler_eval/marketing_2.txt")),
    ("marketing_3", include_str!("spoiler_eval/marketing_3.txt")),
    ("marketing_4", include_str!("spoiler_eval/marketing_4.txt")),
    ("gameplay_1", include_str!("spoiler_eval/gameplay_1.txt")),
    ("gameplay_2", include_str!("spoiler_eval/gameplay_2.txt")),
    ("gameplay_3", include_str!("spoiler_eval/gameplay_3.txt")),
    ("gameplay_4", include_str!("spoiler_eval/gameplay_4.txt")),
    ("story_1", include_str!("spoiler_eval/story_1.txt")),
    ("story_2", include_str!("spoiler_eval/story_2.txt")),
    ("story_3", include_str!("spoiler_eval/story_3.txt")),
    ("story_4", include_str!("spoiler_eval/story_4.txt")),
    (
        "insufficient_1",
        include_str!("spoiler_eval/insufficient_1.txt"),
    ),
    (
        "insufficient_2",
        include_str!("spoiler_eval/insufficient_2.txt"),
    ),
    (
        "insufficient_3",
        include_str!("spoiler_eval/insufficient_3.txt"),
    ),
    (
        "insufficient_4",
        include_str!("spoiler_eval/insufficient_4.txt"),
    ),
];

fn category_index(key: &str) -> Option<usize> {
    Category::ALL.iter().position(|c| c.key() == key)
}

#[tokio::test]
#[ignore = "live: needs OPENROUTER_API_KEY and network"]
async fn spoiler_eval_live() {
    let Ok(key) = std::env::var("OPENROUTER_API_KEY") else {
        println!("OPENROUTER_API_KEY not set, skipping spoiler live eval");
        return;
    };
    let model = std::env::var("SPOILER_EVAL_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());
    twitch_1337::util::install_crypto_provider();
    let http = reqwest::Client::new();
    let endpoint = decisions_endpoint("https://openrouter.ai/api/v1").expect("endpoint");

    // matrix[expected][predicted]; last column counts refusals/errors.
    let mut matrix = [[0usize; 6]; 5];
    let mut misses: Vec<String> = Vec::new();

    for (name, raw) in FIXTURES {
        let (first, text) = raw.split_once('\n').expect("fixture has a game line");
        let game = first.strip_prefix("game: ").expect("game: prefix").trim();
        let expected_key = name.rsplit_once('_').expect("name").0;
        let expected = category_index(expected_key).expect("known category");

        let req = DecisionRequest {
            model: model.clone(),
            state: vec![StatePart::Text(format!("{UNTRUSTED_PREFIX}{text}"))],
            questions: BTreeMap::from([("spoiler".to_owned(), build_question(game))]),
        };
        let resp = decide(&http, &endpoint, Some(&key), &req, Duration::from_secs(60))
            .await
            .unwrap_or_else(|e| panic!("{name}: decision call failed: {e:?}"));
        let answer = resp.answers.get("spoiler");
        let (predicted, note) = match answer {
            Some(Answer::Choice {
                choice,
                probabilities,
                ..
            }) => {
                let p = probabilities.get(choice.as_str()).copied().unwrap_or(0.0);
                (category_index(choice), format!("{choice} p={p:.2}"))
            }
            other => (None, format!("no choice: {other:?}")),
        };
        matrix[expected][predicted.unwrap_or(5)] += 1;
        if predicted != Some(expected) {
            misses.push(format!("{name}: expected {expected_key}, got {note}"));
        }
    }

    println!("\nrows = expected, cols = predicted");
    print!("{:<14}", "");
    for c in Category::ALL {
        print!("{:>13}", c.key());
    }
    println!("{:>13}", "refusal/err");
    for (i, c) in Category::ALL.iter().enumerate() {
        print!("{:<14}", c.key());
        for n in matrix[i] {
            print!("{n:>13}");
        }
        println!();
    }
    let correct: usize = (0..5).map(|i| matrix[i][i]).sum();
    println!(
        "\naccuracy: {correct}/{} ({:.0}%)",
        FIXTURES.len(),
        correct as f64 * 100.0 / FIXTURES.len() as f64
    );
    for m in &misses {
        println!("MISS {m}");
    }
}
