//! `!spoiler`: classify a linked page as a game spoiler via a Decision
//! profile. See `docs/superpowers/specs/2026-11-12-spoiler-check-design.md`.

pub mod input;
pub mod url;
pub mod verdict;

use std::collections::BTreeMap;
use std::time::Duration;

use llm::decision::{DecisionRequest, StatePart, decide, decisions_endpoint};
use reqwest::Url;
use secrecy::ExposeSecret;
use tracing::{debug, error, info, warn};

use crate::llm_factory::LlmRegistry;
use crate::settings::{Settings, ai::AiProfile, ai::ProfileKind};

use self::input::{GatherError, IMAGE_TIMEOUT, Input, PAGE_TIMEOUT, gather};
use self::verdict::{Category, UNTRUSTED_PREFIX, Verdict, build_question, verdict};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
const QUESTION_KEY: &str = "spoiler";

/// Result of one spoiler check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Verdict(Verdict),
    /// Page could not be fetched or read. No model call was made.
    Unreadable,
    /// The decision call failed or its answer could not be mapped.
    Broken,
    /// `spoiler.profile` is missing or not a Decision profile.
    NotConfigured,
}

impl Outcome {
    /// Whether the decision model was called, i.e. the cooldown applies.
    pub fn called_model(&self) -> bool {
        matches!(self, Self::Verdict(_) | Self::Broken)
    }
}

/// The Decision profile `spoiler.profile` points at, if usable.
pub fn decision_profile(settings: &Settings) -> Option<&AiProfile> {
    settings
        .ai
        .by_name(settings.spoiler.profile.trim())
        .filter(|p| p.kind == ProfileKind::Decision)
}

/// Upper bound for a whole check: page + image budgets plus the profile's
/// decision timeout.
pub fn overall_timeout(profile: Option<&AiProfile>) -> Duration {
    PAGE_TIMEOUT + IMAGE_TIMEOUT + Duration::from_secs(profile.map_or(0, |p| p.timeout))
}

/// Fetch `url` with `http` (SSRF-guarded), then classify it for `game` with
/// the configured Decision profile over `api` (the owner-configured endpoint,
/// so no SSRF guard). `game` must already be validated.
pub async fn check(
    http: &reqwest::Client,
    api: &reqwest::Client,
    registry: &LlmRegistry,
    settings: &Settings,
    url: &Url,
    game: &str,
) -> Outcome {
    let Some(profile) = decision_profile(settings) else {
        warn!(profile = %settings.spoiler.profile, "spoiler.profile is not a Decision profile");
        return Outcome::NotConfigured;
    };
    let endpoint = match decisions_endpoint(profile.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL))
    {
        Ok(endpoint) => endpoint,
        Err(error) => {
            warn!(?error, profile = %profile.name, "spoiler profile has an unusable base URL");
            return Outcome::NotConfigured;
        }
    };

    let input = match gather(http, url).await {
        Ok(input) => input,
        Err(GatherError::Unreadable) => {
            debug!(%url, "spoiler link not readable");
            return Outcome::Unreadable;
        }
    };
    if input.text.trim().is_empty() && input.image_data_url.is_none() {
        return Outcome::Verdict(Verdict::Sure {
            category: Category::Insufficient,
            pct: 0,
        });
    }

    let req = request(profile, input, game);
    let key = registry.secret_for(profile);
    let resp = match decide(
        api,
        &endpoint,
        key.as_ref().map(ExposeSecret::expose_secret),
        &req,
        Duration::from_secs(profile.timeout),
    )
    .await
    {
        Ok(resp) => resp,
        Err(error) => {
            error!(?error, model = %profile.model, "spoiler decision call failed");
            return Outcome::Broken;
        }
    };
    let Some(answer) = resp.answers.get(QUESTION_KEY) else {
        error!(model = %resp.model, "spoiler decision response has no answer");
        return Outcome::Broken;
    };
    match verdict(answer, settings.spoiler.confidence_floor) {
        Ok(v) => {
            log_verdict(&v, &resp.model);
            Outcome::Verdict(v)
        }
        Err(error) => {
            error!(?error, model = %resp.model, "spoiler decision answer unusable");
            Outcome::Broken
        }
    }
}

fn request(profile: &AiProfile, input: Input, game: &str) -> DecisionRequest {
    let mut state = vec![StatePart::Text(format!("{UNTRUSTED_PREFIX}{}", input.text))];
    if let Some(image) = input.image_data_url {
        state.push(StatePart::image(image));
    }
    DecisionRequest {
        model: profile.model.clone(),
        state,
        questions: BTreeMap::from([(QUESTION_KEY.to_string(), build_question(game))]),
    }
}

fn log_verdict(v: &Verdict, model: &str) {
    match *v {
        Verdict::Sure { category, pct } => {
            info!(choice = category.key(), pct, model, "spoiler verdict");
        }
        Verdict::Unsure {
            top: [(a, pa), (b, pb)],
        } => info!(
            choice = a.key(),
            pct = pa,
            runner_up = b.key(),
            runner_up_pct = pb,
            model,
            "spoiler verdict below confidence floor"
        ),
    }
}
