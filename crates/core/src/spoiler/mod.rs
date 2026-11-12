//! `!spoiler`: classify a linked page as a game spoiler via a Decision
//! profile. See `docs/superpowers/specs/2026-11-12-spoiler-check-design.md`.

pub mod input;
pub mod url;
pub mod verdict;

use std::collections::BTreeMap;
use std::time::Duration;

use llm::decision::{DecisionRequest, StatePart, decide, decisions_endpoint};
use reqwest::Url;
use secrecy::{ExposeSecret, SecretString};
use tracing::{debug, error, info, warn};

use crate::llm_factory::LlmRegistry;
use crate::settings::{Settings, ai::AiProfile, ai::ProfileKind};

use self::input::{GatherError, IMAGE_TIMEOUT, Input, PAGE_TIMEOUT, gather};
use self::verdict::{UNTRUSTED_PREFIX, Verdict, build_question, verdict};

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
}

/// A Decision profile that is ready to call: kind, model, key and endpoint
/// all check out. Built before the cooldown is charged so a misconfigured
/// bot answers "nicht konfiguriert" for free.
pub struct Prepared<'a> {
    profile: &'a AiProfile,
    endpoint: String,
    key: SecretString,
}

impl Prepared<'_> {
    /// Upper bound for a whole check: page + image budgets plus the
    /// profile's decision timeout.
    pub fn overall_timeout(&self) -> Duration {
        PAGE_TIMEOUT + IMAGE_TIMEOUT + Duration::from_secs(self.profile.timeout)
    }
}

/// Resolve `spoiler.profile` into something [`check`] can call, or `None`
/// (logged at warn) when the profile is missing, not a Decision profile, has
/// no model, no usable key, or an unusable base URL.
pub fn prepare<'a>(registry: &LlmRegistry, settings: &'a Settings) -> Option<Prepared<'a>> {
    let Some(profile) = settings
        .ai
        .by_name(settings.spoiler.profile.trim())
        .filter(|p| p.kind == ProfileKind::Decision)
    else {
        warn!(profile = %settings.spoiler.profile, "spoiler.profile is not a Decision profile");
        return None;
    };
    if profile.model.trim().is_empty() {
        warn!(profile = %profile.name, "spoiler profile has no model");
        return None;
    }
    let Some(key) = registry.secret_for(profile) else {
        warn!(profile = %profile.name, "spoiler profile has no key in [ai.keys]");
        return None;
    };
    let endpoint = match decisions_endpoint(profile.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL))
    {
        Ok(endpoint) => endpoint,
        Err(error) => {
            warn!(?error, profile = %profile.name, "spoiler profile has an unusable base URL");
            return None;
        }
    };
    Some(Prepared {
        profile,
        endpoint,
        key,
    })
}

/// Fetch `url` with `http` (SSRF-guarded), then classify it for `game` with
/// the prepared Decision profile over `api` (the owner-configured endpoint,
/// so no SSRF guard). `game` must already be validated.
pub async fn check(
    http: &reqwest::Client,
    api: &reqwest::Client,
    prepared: &Prepared<'_>,
    floor_pct: u8,
    url: &Url,
    game: &str,
) -> Outcome {
    let profile = prepared.profile;
    let input = match gather(http, url).await {
        Ok(input) => input,
        Err(GatherError::Unreadable) => {
            debug!(%url, "spoiler link not readable");
            return Outcome::Unreadable;
        }
    };
    let req = request(profile, input, game);
    let resp = match decide(
        api,
        &prepared.endpoint,
        Some(prepared.key.expose_secret()),
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
    match verdict(answer, floor_pct) {
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
