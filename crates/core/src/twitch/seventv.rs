//! 7TV emote catalog + SQLite meanings for AI prompt grounding.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use eyre::{Result, WrapErr as _, bail, eyre};
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::{
    APP_USER_AGENT,
    emote_tracker::{EmoteMeaningRow, EmoteTrackerHandle},
    settings::SettingsHandle,
    settings::ai::AiEmotes,
};

const DEFAULT_BASE_URL: &str = "https://7tv.io/v3";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Max emotes returned by the on-demand `search_emotes` tool. Independent of
/// the per-turn `max_prompt_emotes` window — the tool exists to reach the full
/// catalog without bloating every prompt.
pub const SEARCH_EMOTES_LIMIT: usize = 20;

/// Lazily refreshes the available 7TV catalog and builds an LLM prompt block
/// from the tracker active catalog and global REST set joined to SQLite meanings.
pub struct SevenTvEmoteProvider {
    settings: SettingsHandle,
    http: reqwest::Client,
    base_url: String,
    emote_tracker: Arc<EmoteTrackerHandle>,
    cache: Mutex<PromptCache>,
}

/// Live-readable knobs snapshotted from [`SettingsHandle`] on each use.
struct EmotesLiveCaps {
    include_global: bool,
    refresh_interval: Duration,
    max_prompt_emotes: usize,
    min_baseline_emotes: usize,
    pinned_emotes: Vec<String>,
}

#[derive(Debug, Default)]
struct PromptCache {
    last_refresh: Option<Instant>,
    emotes: Option<Vec<PromptEmote>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptEmote {
    name: String,
    appearance: String,
    meaning: Option<String>,
    usage: Option<String>,
    avoid: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SevenTvEmoteSet {
    #[serde(default)]
    emotes: Vec<SevenTvEmote>,
}

#[derive(Debug, Clone, Deserialize)]
struct SevenTvEmote {
    id: String,
    name: String,
}

/// Read the live-tunable emote knobs from a [`SettingsHandle`].
///
/// Extracted as a free function so tests can verify live-rebinding without
/// constructing a full [`SevenTvEmoteProvider`] (which requires a live
/// `reqwest::Client`).
fn live_caps_from_handle(settings: &SettingsHandle) -> EmotesLiveCaps {
    let snap = settings.load();
    if let Some(cfg) = snap.ai.emotes.as_ref() {
        EmotesLiveCaps {
            include_global: cfg.include_global,
            refresh_interval: Duration::from_secs(cfg.refresh_interval_secs),
            max_prompt_emotes: cfg.max_prompt_emotes,
            min_baseline_emotes: cfg.min_baseline_emotes.min(cfg.max_prompt_emotes),
            pinned_emotes: cfg.pinned_emotes.clone(),
        }
    } else {
        let fallback = AiEmotes::default();
        EmotesLiveCaps {
            include_global: fallback.include_global,
            refresh_interval: Duration::from_secs(fallback.refresh_interval_secs),
            max_prompt_emotes: fallback.max_prompt_emotes,
            min_baseline_emotes: fallback.min_baseline_emotes.min(fallback.max_prompt_emotes),
            pinned_emotes: fallback.pinned_emotes,
        }
    }
}

impl SevenTvEmoteProvider {
    /// Build a provider from a [`SettingsHandle`] and the emote-tracker handle.
    ///
    /// Channel emote names come from the tracker's active catalog joined to
    /// `emote_meanings`; globals are REST global ids ∩ meanings by id.
    ///
    /// `base_url` is resolved once at startup from the current snapshot;
    /// changing it requires a restart. All other emote knobs are read live
    /// per-call via [`Self::live_caps`].
    pub fn new(settings: SettingsHandle, emote_tracker: Arc<EmoteTrackerHandle>) -> Result<Self> {
        let snap = settings.load();
        let cfg = snap
            .ai
            .emotes
            .as_ref()
            .ok_or_else(|| eyre!("emotes not configured"))?;

        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .wrap_err("Failed to build 7TV HTTP client")?;

        let base_url = cfg
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_string();

        Ok(Self {
            settings,
            http,
            base_url,
            emote_tracker,
            cache: Mutex::new(PromptCache::default()),
        })
    }

    /// Snapshot the live-tunable knobs from the current settings.
    ///
    /// If the emotes section has somehow been removed from settings while the
    /// provider is running, falls back to compiled defaults so the provider
    /// degrades gracefully rather than panicking.
    fn live_caps(&self) -> EmotesLiveCaps {
        live_caps_from_handle(&self.settings)
    }

    /// Return a turn-specific prompt block for the Twitch channel id.
    ///
    /// The backing catalog + glossary are refreshed at most once per
    /// configured interval, but ranking happens per turn so current chat emotes
    /// and the user instruction can influence which entries the model sees.
    pub async fn prompt_block_for_turn(
        &self,
        twitch_channel_id: &str,
        instruction: &str,
        recent_chat: &str,
    ) -> Option<String> {
        let caps = self.live_caps();
        let emotes = self.prompt_emotes(twitch_channel_id, &caps).await?;
        build_prompt_block(
            &emotes,
            caps.max_prompt_emotes,
            caps.min_baseline_emotes,
            &caps.pinned_emotes,
            instruction,
            recent_chat,
        )
    }

    /// Rank the full available emote set against a free-text query for the
    /// on-demand `search_emotes` tool. Returns a chat-tool result string: up to
    /// [`SEARCH_EMOTES_LIMIT`] matches in the prompt-line format, or a graceful
    /// note when nothing matches / the catalog is unavailable. Unlike the
    /// per-turn block this ignores `max_prompt_emotes` and does not dedupe
    /// against the current turn's window.
    pub async fn search_emotes(&self, twitch_channel_id: &str, query: &str) -> String {
        let caps = self.live_caps();
        match self.prompt_emotes(twitch_channel_id, &caps).await {
            Some(emotes) => build_search_block(&emotes, query, SEARCH_EMOTES_LIMIT),
            None => "Emote catalog is unavailable right now; use only the emote codes already provided in this turn's context.".to_string(),
        }
    }

    async fn prompt_emotes(
        &self,
        twitch_channel_id: &str,
        caps: &EmotesLiveCaps,
    ) -> Option<Vec<PromptEmote>> {
        let mut cache = self.cache.lock().await;
        let now = Instant::now();

        if cache
            .last_refresh
            .is_some_and(|last| now.duration_since(last) < caps.refresh_interval)
        {
            return cache.emotes.clone();
        }

        match self.refresh_prompt_emotes(twitch_channel_id, caps).await {
            Ok(emotes) => {
                cache.last_refresh = Some(now);
                cache.emotes = emotes;
            }
            Err(e) => {
                cache.last_refresh = Some(now);
                warn!(
                    error = ?e,
                    "Failed to refresh 7TV emote glossary; using cached entries if available"
                );
            }
        }

        cache.emotes.clone()
    }

    async fn refresh_prompt_emotes(
        &self,
        _twitch_channel_id: &str,
        caps: &EmotesLiveCaps,
    ) -> Result<Option<Vec<PromptEmote>>> {
        let mut had_error = false;
        let mut emotes = match self.fetch_channel_prompt_emotes().await {
            Ok(channel) => channel,
            Err(e) => {
                warn!(error = ?e, "Failed to read channel emote catalog from tracker");
                had_error = true;
                Vec::new()
            }
        };

        if caps.include_global {
            match self.fetch_global_prompt_emotes().await {
                Ok(global) => merge_prompt_emotes(&mut emotes, global),
                Err(e) => {
                    warn!(error = ?e, "Failed to fetch global 7TV emotes");
                    had_error = true;
                }
            }
        }

        if emotes.is_empty() {
            if had_error {
                bail!("7TV emote catalog refresh failed and no entries are available");
            }
            debug!("7TV emote pool is empty after joining catalog with meanings");
            return Ok(None);
        }

        Ok(Some(emotes))
    }

    async fn fetch_channel_prompt_emotes(&self) -> Result<Vec<PromptEmote>> {
        let active = self.emote_tracker.list_active_emotes().await?;
        if active.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<String> = active.iter().map(|row| row.emote_id.clone()).collect();
        let meanings = self.emote_tracker.list_emote_meanings_for_ids(&ids).await?;
        let meaning_by_id: HashMap<&str, &EmoteMeaningRow> = meanings
            .iter()
            .map(|row| (row.emote_id.as_str(), row))
            .collect();

        let mut emotes = Vec::new();
        for row in active {
            let name = row.name.trim();
            if name.is_empty() {
                continue;
            }
            let Some(meaning) = meaning_by_id.get(row.emote_id.as_str()) else {
                continue;
            };
            if let Some(prompt) = prompt_emote_from_meaning(name, meaning) {
                emotes.push(prompt);
            }
        }

        Ok(emotes)
    }

    async fn fetch_global_prompt_emotes(&self) -> Result<Vec<PromptEmote>> {
        let global = self.fetch_global_emotes().await?;
        if global.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<String> = global.iter().map(|emote| emote.id.clone()).collect();
        let meanings = self.emote_tracker.list_emote_meanings_for_ids(&ids).await?;
        let meaning_by_id: HashMap<&str, &EmoteMeaningRow> = meanings
            .iter()
            .map(|row| (row.emote_id.as_str(), row))
            .collect();

        let mut emotes = Vec::new();
        for emote in global {
            let name = emote.name.trim();
            if name.is_empty() {
                continue;
            }
            let Some(meaning) = meaning_by_id.get(emote.id.as_str()) else {
                continue;
            };
            if let Some(prompt) = prompt_emote_from_meaning(name, meaning) {
                emotes.push(prompt);
            }
        }

        Ok(emotes)
    }

    async fn fetch_global_emotes(&self) -> Result<Vec<SevenTvEmote>> {
        let url = format!("{}/emote-sets/global", self.base_url);
        let set: SevenTvEmoteSet = self.get_json(&url).await?;
        Ok(set.emotes)
    }

    async fn get_json<T>(&self, url: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .wrap_err_with(|| format!("Failed to send 7TV request to {url}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("7TV request failed with status {status}: {body}");
        }

        response
            .json()
            .await
            .wrap_err_with(|| format!("Failed to parse 7TV response from {url}"))
    }
}

fn is_usable_meaning(row: &EmoteMeaningRow) -> bool {
    !normalize_prompt_field(&row.appearance).is_empty()
        || row
            .meaning
            .as_deref()
            .is_some_and(|value| !normalize_prompt_field(value).is_empty())
        || row
            .usage
            .as_deref()
            .is_some_and(|value| !normalize_prompt_field(value).is_empty())
}

fn prompt_emote_from_meaning(name: &str, row: &EmoteMeaningRow) -> Option<PromptEmote> {
    if !is_usable_meaning(row) {
        return None;
    }

    let appearance = normalize_prompt_field(&row.appearance);
    let meaning = row
        .meaning
        .as_deref()
        .map(normalize_prompt_field)
        .filter(|value| !value.is_empty());
    let usage = row
        .usage
        .as_deref()
        .map(normalize_prompt_field)
        .filter(|value| !value.is_empty());
    let avoid = row
        .avoid
        .as_deref()
        .map(normalize_prompt_field)
        .filter(|value| !value.is_empty());

    Some(PromptEmote {
        name: name.to_string(),
        appearance,
        meaning,
        usage,
        avoid,
    })
}

fn merge_prompt_emotes(channel: &mut Vec<PromptEmote>, global: Vec<PromptEmote>) {
    let mut seen: HashSet<String> = channel.iter().map(|emote| emote.name.clone()).collect();
    for emote in global {
        if seen.insert(emote.name.clone()) {
            channel.push(emote);
        }
    }
}

fn build_prompt_block(
    emotes: &[PromptEmote],
    max_prompt_emotes: usize,
    min_baseline_emotes: usize,
    pinned_emotes: &[String],
    instruction: &str,
    recent_chat: &str,
) -> Option<String> {
    let lines = select_prompt_emotes(
        emotes,
        max_prompt_emotes,
        min_baseline_emotes,
        pinned_emotes,
        instruction,
        recent_chat,
    )
    .into_iter()
    .map(format_prompt_emote_line)
    .collect::<Vec<_>>();

    if lines.is_empty() {
        return None;
    }

    // The "do not invent" rule is reconciled with the search_emotes tool: the
    // tool is registered whenever this block is present, so the model is told
    // it may reach for codes beyond the window via the tool — never fabricate.
    Some(format!(
        "\n\n7TV emotes available in this channel:\nIn normal casual Twitch-chat replies, include exactly one fitting emote by default. Use zero emotes only for extremely serious, administrative, fact-sensitive, or clearly unsuitable topics. Use two emotes only when the chat moment is obviously hype, chaotic, or spammy. Prefer emotes recently used by chat when they fit. Use only the exact emote codes listed below or returned by the search_emotes tool; if you want an emote for a feeling that is not shown, call search_emotes to find one. Never invent or alter emote codes.\n{}",
        lines.join("\n")
    ))
}

/// Pick which emotes to inject this turn, in three deduped layers capped by
/// `max_prompt_emotes`:
/// 1. **Pinned core** — `pinned_emotes`, in list order, for the persona's
///    reflex emotes; only those present in the available set are injected
///    (absent pins are dropped). This guarantees soul-reflex emotes are always
///    in-window when they exist in the catalog.
/// 2. **Scored** — anything seen in recent chat, or whose name/appearance/meaning/usage
///    shares 4+ char terms with the current instruction.
/// 3. **Baseline fill** — pool-order fallbacks, until at least
///    `min_baseline_emotes` total are present, so the model always has a
///    baseline vocabulary.
///
/// No emote appears in more than one layer (deduped by pool index).
fn select_prompt_emotes<'a>(
    emotes: &'a [PromptEmote],
    max_prompt_emotes: usize,
    min_baseline_emotes: usize,
    pinned_emotes: &[String],
    instruction: &str,
    recent_chat: &str,
) -> Vec<&'a PromptEmote> {
    if max_prompt_emotes == 0 {
        return Vec::new();
    }
    let mut picked: Vec<&PromptEmote> = Vec::with_capacity(max_prompt_emotes);
    let mut picked_indexes: HashSet<usize> = HashSet::new();

    // Layer 1: pinned core.
    for name in pinned_emotes {
        if picked.len() >= max_prompt_emotes {
            break;
        }
        if let Some((index, emote)) = emotes.iter().enumerate().find(|(_, e)| e.name == *name)
            && picked_indexes.insert(index)
        {
            picked.push(emote);
        }
    }

    // Layer 2: scored.
    let context_terms = searchable_terms(instruction);
    let scored: Vec<(usize, usize, usize, &PromptEmote)> = emotes
        .iter()
        .enumerate()
        .map(|(index, emote)| {
            let recent_count = recent_emote_count(recent_chat, &emote.name);
            let context_score = context_match_score(emote, &context_terms);
            (index, recent_count, context_score, emote)
        })
        .collect();

    let mut scoring: Vec<&(usize, usize, usize, &PromptEmote)> = scored
        .iter()
        .filter(|(_, r, c, _)| *r > 0 || *c > 0)
        .collect();
    scoring.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.0.cmp(&b.0))
    });

    for entry in &scoring {
        if picked.len() >= max_prompt_emotes {
            break;
        }
        if picked_indexes.insert(entry.0) {
            picked.push(entry.3);
        }
    }

    // Layer 3: baseline fill.
    let baseline_floor = min_baseline_emotes.min(max_prompt_emotes);
    if picked.len() < baseline_floor {
        for (index, _, _, emote) in &scored {
            if picked.len() >= baseline_floor {
                break;
            }
            if picked_indexes.insert(*index) {
                picked.push(emote);
            }
        }
    }
    picked
}

/// Render the `search_emotes` tool result: up to `limit` ranked matches in the
/// prompt-line format, or a graceful note when nothing matches.
fn build_search_block(emotes: &[PromptEmote], query: &str, limit: usize) -> String {
    let matches = select_search_emotes(emotes, query, limit);
    if matches.is_empty() {
        return format!(
            "No emotes matched {query:?}. Use only the emote codes already provided in this turn's context; do not invent codes."
        );
    }
    let lines = matches
        .into_iter()
        .map(format_prompt_emote_line)
        .collect::<Vec<_>>();
    format!(
        "7TV emotes matching {query:?} ({} shown). Use the exact codes:\n{}",
        lines.len(),
        lines.join("\n")
    )
}

/// Rank the full available set against a free-text query, returning the top
/// `limit` matches. Reuses the prompt-block term scorer (`searchable_terms` +
/// `terms_match`) over name + appearance + meaning + usage, plus a strong bonus
/// for a case-insensitive partial name match so the model can look an emote up
/// by code. Zero-score entries are dropped (graceful empty when nothing matches).
fn select_search_emotes<'a>(
    emotes: &'a [PromptEmote],
    query: &str,
    limit: usize,
) -> Vec<&'a PromptEmote> {
    let terms = searchable_terms(query);
    let query_lc = query.trim().to_lowercase();
    let mut scored: Vec<(usize, usize, &PromptEmote)> = emotes
        .iter()
        .enumerate()
        .map(|(index, emote)| (search_match_score(emote, &terms, &query_lc), index, emote))
        .filter(|(score, _, _)| *score > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, emote)| emote)
        .collect()
}

fn search_match_score(emote: &PromptEmote, query_terms: &[String], query_lc: &str) -> usize {
    let mut score = 0;
    if query_lc.len() >= 2 && emote.name.to_lowercase().contains(query_lc) {
        score += 10;
    }
    if !query_terms.is_empty() {
        let emote_terms = searchable_terms(&searchable_fields(emote));
        score += query_terms
            .iter()
            .filter(|query| emote_terms.iter().any(|term| terms_match(query, term)))
            .count();
    }
    score
}

fn format_prompt_emote_line(emote: &PromptEmote) -> String {
    let mut line = format!("- {}", emote.name);
    if !emote.appearance.is_empty() {
        line.push_str("; appearance=");
        line.push_str(&emote.appearance);
    }
    if let Some(meaning) = emote.meaning.as_deref() {
        line.push_str("; meaning=");
        line.push_str(meaning);
    }
    if let Some(usage) = emote.usage.as_deref() {
        line.push_str("; use=");
        line.push_str(usage);
    }
    if let Some(avoid) = emote.avoid.as_deref() {
        line.push_str("; avoid=");
        line.push_str(avoid);
    }
    line
}

fn recent_emote_count(recent_chat: &str, emote_name: &str) -> usize {
    recent_chat
        .split_whitespace()
        .filter(|token| token_matches_emote(token, emote_name))
        .count()
}

fn token_matches_emote(token: &str, emote_name: &str) -> bool {
    token == emote_name || trim_wrapping_chat_punctuation(token) == emote_name
}

fn trim_wrapping_chat_punctuation(token: &str) -> &str {
    token.trim_matches(|c| {
        matches!(
            c,
            ',' | '.' | '!' | ';' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}'
        )
    })
}

fn context_match_score(emote: &PromptEmote, context_terms: &[String]) -> usize {
    if context_terms.is_empty() {
        return 0;
    }

    let emote_terms = searchable_terms(&searchable_fields(emote));

    context_terms
        .iter()
        .filter(|query| emote_terms.iter().any(|term| terms_match(query, term)))
        .count()
}

fn searchable_fields(emote: &PromptEmote) -> String {
    let mut fields = emote.name.clone();
    if !emote.appearance.is_empty() {
        fields.push(' ');
        fields.push_str(&emote.appearance);
    }
    if let Some(meaning) = emote.meaning.as_deref() {
        fields.push(' ');
        fields.push_str(meaning);
    }
    if let Some(usage) = emote.usage.as_deref() {
        fields.push(' ');
        fields.push_str(usage);
    }
    fields
}

fn searchable_terms(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|term| term.len() >= 4 && !is_context_stopword(term))
        .filter(|term| seen.insert(term.clone()))
        .collect()
}

fn is_context_stopword(term: &str) -> bool {
    matches!(
        term,
        "eine"
            | "einer"
            | "einem"
            | "einen"
            | "etwas"
            | "wenn"
            | "oder"
            | "nicht"
            | "bitte"
            | "reply"
            | "message"
            | "author"
            | "parent"
            | "user"
            | "chat"
            | "channel"
    )
}

fn terms_match(query: &str, term: &str) -> bool {
    query == term
        || (query.len() >= 5
            && term.len() >= 5
            && (query.starts_with(term) || term.starts_with(query)))
}

fn normalize_prompt_field(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_global_emote_set_response() {
        let json = serde_json::json!({
            "id": "global",
            "emotes": [
                {"id": "1", "name": "KEKW", "data": {"name": "ignored"}},
                {"id": "2", "name": "peepoHappy"}
            ]
        });

        let parsed: SevenTvEmoteSet = serde_json::from_value(json).unwrap();

        assert_eq!(parsed.emotes.len(), 2);
        assert_eq!(parsed.emotes[0].id, "1");
        assert_eq!(parsed.emotes[0].name, "KEKW");
        assert_eq!(parsed.emotes[1].name, "peepoHappy");
    }

    fn prompt_emote(name: &str, appearance: &str) -> PromptEmote {
        PromptEmote {
            name: name.into(),
            appearance: appearance.into(),
            meaning: None,
            usage: None,
            avoid: None,
        }
    }

    fn prompt_emote_full(
        name: &str,
        appearance: &str,
        meaning: Option<&str>,
        usage: Option<&str>,
        avoid: Option<&str>,
    ) -> PromptEmote {
        PromptEmote {
            name: name.into(),
            appearance: appearance.into(),
            meaning: meaning.map(str::to_string),
            usage: usage.map(str::to_string),
            avoid: avoid.map(str::to_string),
        }
    }

    #[test]
    fn merge_prompt_emotes_deduplicates_by_name() {
        let mut channel = vec![prompt_emote("KEKW", "laughing")];
        merge_prompt_emotes(
            &mut channel,
            vec![
                prompt_emote("KEKW", "duplicate"),
                prompt_emote("Local", "insider"),
            ],
        );
        assert_eq!(channel.len(), 2);
        assert_eq!(channel[0].name, "KEKW");
        assert_eq!(channel[1].name, "Local");
    }

    #[test]
    fn usable_meaning_requires_non_empty_semantic_field() {
        let row = EmoteMeaningRow {
            emote_id: "e1".into(),
            appearance: "   ".into(),
            meaning: None,
            usage: None,
            avoid: Some("never".into()),
            updated_at: 1,
        };
        assert!(!is_usable_meaning(&row));
        assert!(prompt_emote_from_meaning("KEKW", &row).is_none());
    }

    #[test]
    fn prompt_line_emits_only_non_empty_fields() {
        let line = format_prompt_emote_line(&prompt_emote_full(
            "KEKW",
            "laughing face",
            Some("lachen"),
            Some("bei witzen"),
            Some("ernste themen"),
        ));
        assert!(line.contains("- KEKW; appearance=laughing face"));
        assert!(line.contains("meaning=lachen"));
        assert!(line.contains("use=bei witzen"));
        assert!(line.contains("avoid=ernste themen"));
    }

    #[test]
    fn prompt_contains_only_catalog_backed_entries() {
        let emotes = vec![prompt_emote_full(
            "KEKW",
            "lachen",
            None,
            Some("wenn etwas lustig ist"),
            Some("ernste Themen"),
        )];
        let prompt = build_prompt_block(&emotes, 40, 40, &[], "", "").unwrap();

        assert!(prompt.contains("KEKW"));
        assert!(prompt.contains("appearance=lachen"));
        assert!(!prompt.contains("Missing"));
    }

    #[test]
    fn prompt_respects_max_prompt_emotes() {
        let emotes = vec![prompt_emote("A", "first"), prompt_emote("B", "second")];
        let prompt = build_prompt_block(&emotes, 1, 1, &[], "", "").unwrap();

        assert!(prompt.contains("A"));
        assert!(!prompt.contains("B"));
    }

    #[test]
    fn prompt_prioritizes_emotes_seen_in_recent_chat() {
        let emotes = vec![
            prompt_emote_full("KEKW", "lachen", None, Some("wenn etwas lustig ist"), None),
            prompt_emote_full(
                "LocalEmote",
                "lokaler Channel-Insider",
                None,
                Some("wenn der Chat den Insider anspricht"),
                None,
            ),
        ];

        let prompt = build_prompt_block(
            &emotes,
            2,
            2,
            &[],
            "sag etwas lustiges",
            "## Recent chat (#main)\n[13:37] bob: LocalEmote",
        )
        .unwrap();

        let local_pos = prompt.find("- LocalEmote;").unwrap();
        let kekw_pos = prompt.find("- KEKW;").unwrap();
        assert!(
            local_pos < kekw_pos,
            "recent chat emote should rank first:\n{prompt}"
        );
    }

    #[test]
    fn prompt_prioritizes_context_matches_when_chat_is_neutral() {
        let emotes = vec![
            prompt_emote_full(
                "LocalEmote",
                "lokaler Channel-Insider",
                None,
                Some("wenn der Chat den Insider anspricht"),
                None,
            ),
            prompt_emote_full(
                "KEKW",
                "lachen",
                Some("etwas ist lustig"),
                Some("bei Witzen oder Fail-Momenten"),
                None,
            ),
        ];

        let prompt = build_prompt_block(&emotes, 2, 2, &[], "sag etwas lustiges", "").unwrap();

        let local_pos = prompt.find("- LocalEmote;").unwrap();
        let kekw_pos = prompt.find("- KEKW;").unwrap();
        assert!(
            kekw_pos < local_pos,
            "context-matching emote should outrank pool order:\n{prompt}"
        );
    }

    #[test]
    fn prompt_drops_zero_score_emotes_when_baseline_is_zero() {
        let emotes = vec![
            prompt_emote("Hit", "lustig"),
            prompt_emote("Idle", "nichts dergleichen"),
        ];

        let prompt = build_prompt_block(&emotes, 8, 0, &[], "etwas lustiges", "").unwrap();

        assert!(prompt.contains("- Hit;"));
        assert!(
            !prompt.contains("- Idle;"),
            "zero-score emote leaked through:\n{prompt}"
        );
    }

    #[test]
    fn prompt_baseline_floor_fills_in_pool_order_when_no_scoring_hits() {
        let emotes = vec![
            prompt_emote("First", "platzhalter"),
            prompt_emote("Second", "platzhalter"),
            prompt_emote("Third", "platzhalter"),
        ];

        let prompt = build_prompt_block(&emotes, 8, 2, &[], "hi", "").unwrap();

        assert!(prompt.contains("- First;"));
        assert!(prompt.contains("- Second;"));
        assert!(
            !prompt.contains("- Third;"),
            "baseline floor exceeded:\n{prompt}"
        );
    }

    #[test]
    fn prompt_block_returns_none_when_max_prompt_emotes_is_zero() {
        let emotes = vec![prompt_emote("A", "x")];
        assert!(build_prompt_block(&emotes, 0, 0, &[], "hi", "").is_none());
    }

    #[test]
    fn pinned_emote_always_present_and_leads_even_without_scoring() {
        let emotes = vec![
            prompt_emote("First", "platzhalter"),
            prompt_emote("Second", "platzhalter"),
            prompt_emote("PinnedReflex", "lacht"),
        ];
        let pinned = vec!["PinnedReflex".to_string()];
        let prompt = build_prompt_block(&emotes, 8, 2, &pinned, "hi", "").unwrap();

        let pinned_pos = prompt
            .find("- PinnedReflex;")
            .expect("pinned emote must be present");
        let first_pos = prompt.find("- First;").expect("baseline fill present");
        assert!(
            pinned_pos < first_pos,
            "pinned core must lead the block:\n{prompt}"
        );
    }

    #[test]
    fn pinned_emote_absent_from_catalog_is_dropped() {
        let emotes = vec![prompt_emote("First", "platzhalter")];
        let pinned = vec!["Ghost".to_string()];
        let prompt = build_prompt_block(&emotes, 8, 1, &pinned, "hi", "").unwrap();
        assert!(
            !prompt.contains("Ghost"),
            "absent pin must be dropped:\n{prompt}"
        );
        assert!(
            prompt.contains("- First;"),
            "baseline still fills:\n{prompt}"
        );
    }

    #[test]
    fn no_emote_appears_twice_across_pinned_scored_baseline() {
        let emotes = vec![
            prompt_emote("KEKW", "lustig lachen"),
            prompt_emote("Filler", "platzhalter"),
        ];
        let pinned = vec!["KEKW".to_string()];
        let prompt =
            build_prompt_block(&emotes, 8, 4, &pinned, "etwas lustig", "KEKW KEKW").unwrap();
        let occurrences = prompt.matches("- KEKW;").count();
        assert_eq!(occurrences, 1, "KEKW must not be duplicated:\n{prompt}");
    }

    #[test]
    fn pinned_core_capped_by_max_prompt_emotes() {
        let emotes = vec![
            prompt_emote("A", "x"),
            prompt_emote("B", "y"),
            prompt_emote("C", "z"),
        ];
        let pinned = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let prompt = build_prompt_block(&emotes, 2, 0, &pinned, "hi", "").unwrap();
        assert!(prompt.contains("- A;"));
        assert!(prompt.contains("- B;"));
        assert!(
            !prompt.contains("- C;"),
            "window must stay capped at 2:\n{prompt}"
        );
    }

    #[test]
    fn search_ranks_name_match_above_term_match() {
        let emotes = vec![
            prompt_emote("SomethingFunny", "ein lustiger Moment"),
            prompt_emote("Pepe", "irgendwas"),
        ];
        let ranked = select_search_emotes(&emotes, "pepe", 10);
        assert_eq!(ranked.first().map(|e| e.name.as_str()), Some("Pepe"));
    }

    #[test]
    fn search_matches_appearance_and_meaning_terms() {
        let emotes = vec![
            prompt_emote_full("KEKW", "lachen wenn etwas lustig ist", None, None, None),
            prompt_emote("Sadge", "traurig sein"),
        ];
        let ranked = select_search_emotes(&emotes, "lustig", 10);
        let names: Vec<&str> = ranked.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"KEKW"),
            "appearance match expected: {names:?}"
        );
        assert!(
            !names.contains(&"Sadge"),
            "non-match must be excluded: {names:?}"
        );
    }

    #[test]
    fn search_respects_limit() {
        let emotes: Vec<PromptEmote> = (0..25)
            .map(|i| prompt_emote(&format!("Laugh{i}"), "lachen lustig moment"))
            .collect();
        let ranked = select_search_emotes(&emotes, "lachen", SEARCH_EMOTES_LIMIT);
        assert_eq!(ranked.len(), SEARCH_EMOTES_LIMIT, "must cap at the limit");
    }

    #[test]
    fn search_block_is_graceful_when_nothing_matches() {
        let emotes = vec![prompt_emote("KEKW", "lachen")];
        let block = build_search_block(&emotes, "zzzzqqqq", SEARCH_EMOTES_LIMIT);
        assert!(block.contains("No emotes matched"), "got: {block}");
        assert!(block.contains("this turn's context"), "got: {block}");
    }

    #[test]
    fn search_block_renders_matches_in_prompt_line_format() {
        let emotes = vec![prompt_emote_full(
            "KEKW",
            "lachen",
            None,
            Some("bei witzen"),
            Some("ernste themen"),
        )];
        let block = build_search_block(&emotes, "lachen", SEARCH_EMOTES_LIMIT);
        assert!(block.contains("- KEKW; appearance=lachen"), "got: {block}");
        assert!(block.contains("use=bei witzen"), "got: {block}");
        assert!(block.contains("avoid=ernste themen"), "got: {block}");
    }

    fn make_emotes_handle(max_prompt_emotes: usize) -> crate::settings::SettingsHandle {
        use std::sync::Arc;

        use arc_swap::ArcSwap;

        use crate::settings::Settings;

        let mut s = Settings::compiled_defaults();
        s.ai.emotes = Some(AiEmotes {
            max_prompt_emotes,
            min_baseline_emotes: 0,
            ..AiEmotes::default()
        });
        Arc::new(ArcSwap::from_pointee(s))
    }

    #[test]
    fn live_caps_max_prompt_emotes_respected_and_live_rebindable() {
        use std::sync::Arc;

        let emotes: Vec<PromptEmote> = ["A", "B", "C", "D", "E"]
            .into_iter()
            .map(|name| prompt_emote(name, "entry"))
            .collect();

        let handle = make_emotes_handle(5);
        let caps = live_caps_from_handle(&handle);
        assert_eq!(caps.max_prompt_emotes, 5);
        let prompt5 =
            build_prompt_block(&emotes, caps.max_prompt_emotes, 5, &[], "hi", "").unwrap();
        assert!(prompt5.contains("- A;"));
        assert!(prompt5.contains("- E;"));

        let mut s = (*handle.load_full()).clone();
        s.ai.emotes.as_mut().unwrap().max_prompt_emotes = 2;
        s.ai.emotes.as_mut().unwrap().min_baseline_emotes = 2;
        handle.store(Arc::new(s));

        let caps2 = live_caps_from_handle(&handle);
        assert_eq!(caps2.max_prompt_emotes, 2);
        let prompt2 = build_prompt_block(
            &emotes,
            caps2.max_prompt_emotes,
            caps2.min_baseline_emotes,
            &[],
            "hi",
            "",
        )
        .unwrap();
        assert!(prompt2.contains("- A;"));
        assert!(prompt2.contains("- B;"));
        assert!(
            !prompt2.contains("- C;"),
            "prompt should be capped at 2:\n{prompt2}"
        );
    }

    fn tracker_test_settings(
        mock_uri: &str,
        include_global: bool,
    ) -> crate::settings::SettingsHandle {
        use std::sync::Arc;

        use arc_swap::ArcSwap;

        use crate::settings::Settings;

        let mut s = Settings::compiled_defaults();
        s.ai.emotes = Some(AiEmotes {
            base_url: Some(mock_uri.to_string()),
            include_global,
            refresh_interval_secs: 0,
            ..AiEmotes::default()
        });
        Arc::new(ArcSwap::from_pointee(s))
    }

    async fn seed_tracker_emote(
        handle: &EmoteTrackerHandle,
        emote_id: &str,
        name: &str,
        appearance: &str,
        meaning: Option<&str>,
        usage: Option<&str>,
        avoid: Option<&str>,
    ) {
        handle.upsert_emote(emote_id, name, true, 1).await.unwrap();
        handle
            .upsert_emote_meaning(emote_id, appearance, meaning, usage, avoid, 1)
            .await
            .unwrap();
    }

    async fn tracker_test_provider(
        mock_uri: &str,
        tracker: Arc<EmoteTrackerHandle>,
        include_global: bool,
    ) -> SevenTvEmoteProvider {
        let settings = tracker_test_settings(mock_uri, include_global);
        SevenTvEmoteProvider::new(settings, tracker).unwrap()
    }

    #[tokio::test]
    async fn tracker_channel_names_join_meanings_without_rest_fallback() {
        crate::install_crypto_provider();

        let mock = wiremock::MockServer::start().await;
        let channel_id = "12345";

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/users/twitch/{channel_id}"
            )))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "user",
                    "emote_set": {
                        "emotes": [{"id": "rest", "name": "RestOnly"}]
                    }
                })),
            )
            .expect(0)
            .mount(&mock)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let handle = Arc::new(
            EmoteTrackerHandle::open(&dir.path().join(crate::emote_tracker::DB_FILENAME)).unwrap(),
        );
        seed_tracker_emote(
            &handle,
            "tracker-1",
            "TrackerOnly",
            "from the SQLite catalog",
            None,
            None,
            None,
        )
        .await;

        let provider = tracker_test_provider(&mock.uri(), handle, false).await;
        let prompt = provider
            .prompt_block_for_turn(channel_id, "hi", "")
            .await
            .expect("prompt block should be available");

        assert!(
            prompt.contains("- TrackerOnly;"),
            "tracker catalog emote missing:\n{prompt}"
        );
        assert!(
            !prompt.contains("RestOnly"),
            "REST channel emote must not appear:\n{prompt}"
        );

        mock.verify().await;
    }

    #[tokio::test]
    async fn empty_tracker_catalog_yields_no_prompt_block() {
        crate::install_crypto_provider();

        let mock = wiremock::MockServer::start().await;
        let channel_id = "12345";

        let dir = tempfile::tempdir().unwrap();
        let handle = Arc::new(
            EmoteTrackerHandle::open(&dir.path().join(crate::emote_tracker::DB_FILENAME)).unwrap(),
        );

        let provider = tracker_test_provider(&mock.uri(), handle, false).await;
        assert!(
            provider
                .prompt_block_for_turn(channel_id, "hi", "")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn globals_rest_intersect_meanings_by_id() {
        crate::install_crypto_provider();

        let mock = wiremock::MockServer::start().await;
        let channel_id = "12345";

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/emote-sets/global"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "global",
                    "emotes": [
                        {"id": "global-kekw", "name": "KEKW"},
                        {"id": "global-peepo", "name": "peepoHappy"}
                    ]
                })),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let handle = Arc::new(
            EmoteTrackerHandle::open(&dir.path().join(crate::emote_tracker::DB_FILENAME)).unwrap(),
        );
        seed_tracker_emote(
            &handle,
            "tracker-1",
            "TrackerOnly",
            "from the SQLite catalog",
            None,
            None,
            None,
        )
        .await;
        seed_tracker_emote(
            &handle,
            "global-kekw",
            "KEKW",
            "global emote",
            None,
            None,
            None,
        )
        .await;

        let provider = tracker_test_provider(&mock.uri(), handle, true).await;
        let prompt = provider
            .prompt_block_for_turn(channel_id, "hi", "")
            .await
            .expect("prompt block should be available");

        assert!(
            prompt.contains("- TrackerOnly;"),
            "tracker channel emote missing:\n{prompt}"
        );
        assert!(
            prompt.contains("- KEKW;"),
            "global REST emote missing:\n{prompt}"
        );
        assert!(
            !prompt.contains("peepoHappy"),
            "global without meaning row must be omitted:\n{prompt}"
        );

        mock.verify().await;
    }

    #[tokio::test]
    async fn active_emote_without_usable_meaning_is_excluded() {
        crate::install_crypto_provider();

        let mock = wiremock::MockServer::start().await;
        let channel_id = "12345";
        let dir = tempfile::tempdir().unwrap();
        let handle = Arc::new(
            EmoteTrackerHandle::open(&dir.path().join(crate::emote_tracker::DB_FILENAME)).unwrap(),
        );
        handle
            .upsert_emote("tracker-1", "SilentEmote", true, 1)
            .await
            .unwrap();
        handle
            .upsert_emote_meaning("tracker-1", "   ", None, None, Some("never"), 1)
            .await
            .unwrap();

        let provider = tracker_test_provider(&mock.uri(), handle, false).await;
        assert!(
            provider
                .prompt_block_for_turn(channel_id, "hi", "")
                .await
                .is_none()
        );
    }
}
