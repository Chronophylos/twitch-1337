//! AI subsection of dashboard settings.
//!
//! Mirrors the runtime shape of the old `core::config::AiConfig`
//! minus the `api_key` secret. Defaults intentionally match the
//! pre-hoist behavior so existing deployments see no drift after
//! the schema bump.

use serde::{Deserialize, Serialize};

use super::provider::AiProvider;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiSettings {
    /// Named provider configurations. Agent profile changes are dashboard
    /// actions; no restart, no config.toml edit.
    pub profiles: Vec<AiProfile>,
    pub aurora: AiAurora,
    pub grok: AiGrok,
    pub recap: AiRecap,
    pub dreamer: AiDreamer,
    pub media: AiMedia,
    pub history: AiHistory,
    pub memory: AiMemory,
    pub prefill: Option<AiPrefill>,
    pub web: Option<AiWeb>,
    pub emotes: Option<AiEmotes>,
}

/// What a profile is for. `Chat` profiles drive agents through an
/// `LlmClient`; `Decision` profiles are single-shot structured-choice
/// endpoints (OpenRouter decisions) and never back a chat client. See
/// ADR-0010.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    #[default]
    Chat,
    Decision,
}

impl ProfileKind {
    pub const ALL: [Self; 2] = [Self::Chat, Self::Decision];

    /// Stable string form, matches the serde output; used by the web form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Decision => "decision",
        }
    }
}

/// One named provider configuration.
///
/// `key` names an entry in config.toml's `[ai.keys]`; the secret itself
/// never touches settings.ron. Required for every provider except Ollama.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiProfile {
    pub name: String,
    #[serde(default)]
    pub key: Option<String>,
    pub provider: AiProvider,
    pub base_url: Option<String>,
    pub model: String,
    pub timeout: u64,
    pub reasoning_effort: Option<String>,
    /// Service tier hint. Only honored when `provider.caps().service_tier`.
    pub service_tier: Option<String>,
    #[serde(default)]
    pub kind: ProfileKind,
}

impl Default for AiProfile {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            key: None,
            provider: AiProvider::OpenRouter,
            base_url: None,
            model: String::new(),
            timeout: 30,
            reasoning_effort: None,
            service_tier: None,
            kind: ProfileKind::Chat,
        }
    }
}

impl AiSettings {
    pub fn by_name(&self, name: &str) -> Option<&AiProfile> {
        self.profiles.iter().find(|p| p.name == name)
    }
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            profiles: vec![AiProfile::default()],
            aurora: AiAurora::default(),
            grok: AiGrok::default(),
            recap: AiRecap::default(),
            dreamer: AiDreamer::default(),
            media: AiMedia::default(),
            history: AiHistory::default(),
            memory: AiMemory::default(),
            prefill: None,
            web: None,
            emotes: None,
        }
    }
}

/// MIGRATION(ai-profiles): only reachable through the legacy
/// `ai.connection` override block in pre-schema-6 settings.ron files.
/// New code uses [`AiProvider`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AiBackendKind {
    OpenAi,
    Ollama,
}

impl AiBackendKind {
    /// Stable lowercase string form — matches the `serde(rename_all)` output
    /// and is consumed by the settings page (`<input value=…>`) plus the POST
    /// parser. Keeping this in code lets templates avoid re-doing the match.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Ollama => "ollama",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiAurora {
    pub profile: String,
    pub max_turn_rounds: usize,
    pub max_writes_per_turn: usize,
    /// Persona name the bot uses in chat-history identity push and chat-line
    /// rendering. Defaults to `"Aurora"`. Missing in pre-existing settings.ron
    /// files is handled by `#[serde(default = ...)]` so v2 reads stay forward-
    /// compatible without a new migration sentinel.
    #[serde(default = "default_persona_name")]
    pub persona_name: String,
    /// Artificial floor on how long a turn takes before the reply is spoken.
    /// The model usually answers faster than a human would type, which reads
    /// as a bot; padding the tail makes it look like it thought about it.
    /// `0` disables the padding.
    #[serde(default)]
    pub min_response_delay_ms: u64,
    /// What Aurora is told to aim for in `system.md` (`{reply_target}`).
    #[serde(default = "default_reply_target_chars")]
    pub reply_target_chars: usize,
    /// Rewrite once if the cleaned reply is longer than this.
    #[serde(default = "default_reply_soft_chars")]
    pub reply_soft_chars: usize,
    /// Ellipsis-clamp if the line is still longer after the rewrite.
    #[serde(default = "default_reply_hard_chars")]
    pub reply_hard_chars: usize,
}

fn default_persona_name() -> String {
    "Aurora".to_string()
}

fn default_reply_target_chars() -> usize {
    480
}

fn default_reply_soft_chars() -> usize {
    490
}

fn default_reply_hard_chars() -> usize {
    490
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiHistory {
    pub length: u64,
    pub ai_channel_length: u64,
    #[serde(default)]
    pub n: u64,
    #[serde(default)]
    pub ai_channel_n: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiRecap {
    /// Profile Recap paraphrases on. `None` / empty = Recap off.
    #[serde(default)]
    pub profile: Option<String>,
    pub batch_size: u64,
    pub max_notes: u64,
    pub merge_width: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiMemory {
    pub soul_bytes: usize,
    pub lore_bytes: usize,
    pub user_bytes: usize,
    /// ADR-0003 renamed `state` to `scratch`. The alias reads settings.ron
    /// files written before the rename; the next dashboard save rewrites them
    /// under the new key, so no migration pass is needed.
    #[serde(alias = "state_bytes")]
    pub scratch_bytes: usize,
    pub inject_byte_budget: usize,
    #[serde(alias = "max_state_files")]
    pub max_scratch_files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiDreamer {
    pub enabled: bool,
    /// Required profile name to run this agent on.
    pub profile: String,
    pub run_at: String,
    pub timeout_secs: u64,
    pub max_rounds: usize,
    /// Per-turn `write_file` quota for the dreamer ritual. Separate from the
    /// chat-turn cap (`AiAurora::max_writes_per_turn`): the dreamer rewrites
    /// SOUL + LORE + many user files in one pass, so it defaults higher (32 vs
    /// the chat cap's 8). Both are bounded to the same `1..=64` range.
    pub max_writes_per_turn: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiGrok {
    /// Required profile name to run this agent on.
    pub profile: String,
    /// Persona name for the `@grok` alias, used in chat-history identity push
    /// and chat-line rendering (mirrors `AiAurora::persona_name` for
    /// Aurora). Defaults to `"Grok"`. `#[serde(default = ...)]` keeps v1
    /// settings.ron reads forward-compatible without a migration sentinel.
    #[serde(default = "default_grok_persona_name")]
    pub persona_name: String,
}

impl Default for AiGrok {
    fn default() -> Self {
        Self {
            profile: "default".into(),
            persona_name: default_grok_persona_name(),
        }
    }
}

fn default_grok_persona_name() -> String {
    "Grok".to_string()
}

/// Prefill config — `threshold` is `f64` compared as bits to allow `Eq`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiPrefill {
    pub base_url: String,
    pub threshold: f64,
}

impl PartialEq for AiPrefill {
    fn eq(&self, other: &Self) -> bool {
        self.base_url == other.base_url && self.threshold.to_bits() == other.threshold.to_bits()
    }
}

impl Eq for AiPrefill {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiWeb {
    pub base_url: String,
    pub timeout: u64,
    pub max_results: usize,
    pub max_rounds: usize,
    pub cache_ttl_secs: u64,
    pub cache_capacity: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiEmotes {
    pub include_global: bool,
    pub refresh_interval_secs: u64,
    pub max_prompt_emotes: usize,
    pub min_baseline_emotes: usize,
    /// Emote codes always injected into the per-turn block (the persona's
    /// reflex emotes from the live persona prompt). Names absent from the
    /// baked glossary are dropped with a warning; the list must not exceed
    /// `max_prompt_emotes`. Owner-managed; not auto-synced from SOUL.md.
    #[serde(default = "default_pinned_emotes")]
    pub pinned_emotes: Vec<String>,
    pub base_url: Option<String>,
}

/// Seed pins to the documented soul-reflex codes that exist in the baked
/// glossary, so the reported "persona told to use emotes it isn't given" bug
/// is fixed out of the box. Owner extends via the dashboard.
fn default_pinned_emotes() -> Vec<String> {
    vec!["PepeLa".to_string(), "okjj".to_string()]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiMedia {
    /// Required profile name to run this agent on.
    pub profile: String,
    pub timeout: u64,
    pub max_image_size: bytesize::ByteSize,
    pub max_pdf_size: bytesize::ByteSize,
    pub max_audio_size: bytesize::ByteSize,
    pub max_video_size: bytesize::ByteSize,
    pub max_text_size: bytesize::ByteSize,
}

impl Default for AiAurora {
    fn default() -> Self {
        Self {
            profile: "default".into(),
            max_turn_rounds: 4,
            max_writes_per_turn: 8,
            persona_name: default_persona_name(),
            min_response_delay_ms: 0,
            reply_target_chars: default_reply_target_chars(),
            reply_soft_chars: default_reply_soft_chars(),
            reply_hard_chars: default_reply_hard_chars(),
        }
    }
}

impl Default for AiHistory {
    fn default() -> Self {
        Self {
            length: crate::ai::chat_history::DEFAULT_HISTORY_LENGTH,
            ai_channel_length: 100,
            n: 20,
            ai_channel_n: 10,
        }
    }
}

impl Default for AiRecap {
    fn default() -> Self {
        Self {
            profile: None,
            batch_size: 40,
            max_notes: 8,
            merge_width: 3,
        }
    }
}

impl Default for AiMemory {
    fn default() -> Self {
        Self {
            soul_bytes: 4096,
            lore_bytes: 12_288,
            user_bytes: 4096,
            scratch_bytes: 2048,
            inject_byte_budget: 24_576,
            max_scratch_files: 16,
        }
    }
}

impl Default for AiDreamer {
    fn default() -> Self {
        Self {
            enabled: true,
            profile: "default".into(),
            run_at: "04:00".into(),
            timeout_secs: 120,
            max_rounds: 20,
            max_writes_per_turn: 32,
        }
    }
}

impl Default for AiPrefill {
    fn default() -> Self {
        Self {
            base_url: "https://logs.zonian.dev".into(),
            threshold: 0.5,
        }
    }
}

impl Default for AiWeb {
    fn default() -> Self {
        Self {
            // SearXNG's own default port. 8080 is the bot's dashboard, so that
            // default could only ever 404 against ourselves.
            base_url: "http://localhost:8888/search".into(),
            timeout: 15,
            max_results: 5,
            max_rounds: 3,
            cache_ttl_secs: 300,
            cache_capacity: 100,
        }
    }
}

impl Default for AiEmotes {
    fn default() -> Self {
        Self {
            include_global: true,
            refresh_interval_secs: 3600,
            max_prompt_emotes: 20,
            min_baseline_emotes: 4,
            pinned_emotes: default_pinned_emotes(),
            base_url: None,
        }
    }
}

impl Default for AiMedia {
    fn default() -> Self {
        Self {
            profile: "default".into(),
            timeout: 60,
            max_image_size: bytesize::ByteSize::mib(10),
            max_pdf_size: bytesize::ByteSize::mib(25),
            max_audio_size: bytesize::ByteSize::mib(25),
            max_video_size: bytesize::ByteSize::mib(50),
            max_text_size: bytesize::ByteSize::mib(1),
        }
    }
}

impl AiMedia {
    pub fn cap_for(&self, bucket: crate::ai::content::detect::Bucket) -> bytesize::ByteSize {
        use crate::ai::content::detect::Bucket;
        match bucket {
            Bucket::Image => self.max_image_size,
            Bucket::Pdf => self.max_pdf_size,
            Bucket::Audio => self.max_audio_size,
            Bucket::Video => self.max_video_size,
            Bucket::Text => self.max_text_size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_without_kind_deserializes_as_chat() {
        let p: AiProfile = ron::from_str(
            r#"(name: "x", provider: open_router, base_url: None, model: "m", timeout: 30, reasoning_effort: None, service_tier: None)"#,
        )
        .expect("legacy profile must parse");
        assert_eq!(p.kind, ProfileKind::Chat);
    }

    #[test]
    fn profile_kind_as_str_matches_serde() {
        for k in ProfileKind::ALL {
            assert_eq!(ron::to_string(&k).unwrap(), k.as_str());
        }
    }

    #[test]
    fn ai_aurora_default_has_persona_aurora() {
        let b = AiAurora::default();
        assert_eq!(b.persona_name, "Aurora");
    }

    #[test]
    fn ai_aurora_reply_length_defaults_preserve_today() {
        let b = AiAurora::default();
        assert_eq!(b.reply_target_chars, 480);
        assert_eq!(b.reply_soft_chars, 490);
        assert_eq!(b.reply_hard_chars, 490);
    }

    #[test]
    fn legacy_ai_behavior_overrides_without_reply_keys_keep_defaults() {
        let o: crate::settings::overrides::AiBehaviorOverrides =
            ron::from_str("(max_turn_rounds: Some(5))").expect("partial override must parse");
        assert_eq!(o.max_turn_rounds, Some(5));
        assert!(o.reply_target_chars.is_none());
        assert!(o.reply_soft_chars.is_none());
        assert!(o.reply_hard_chars.is_none());
        let resolved = crate::settings::Settings::resolve(
            &crate::settings::Settings::compiled_defaults(),
            &crate::settings::SettingsOverrides {
                ai: crate::settings::overrides::AiOverrides {
                    behavior: o,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert_eq!(resolved.ai.aurora.reply_target_chars, 480);
        assert_eq!(resolved.ai.aurora.reply_soft_chars, 490);
        assert_eq!(resolved.ai.aurora.reply_hard_chars, 490);
        assert_eq!(resolved.ai.aurora.max_turn_rounds, 5);
    }

    #[test]
    fn ai_grok_persona_name_defaults_to_grok() {
        let g = crate::settings::ai::AiGrok::default();
        assert_eq!(g.persona_name, "Grok");
    }

    /// ADR-0003 renamed state -> scratch. Every settings.ron in the wild was
    /// written with the old keys; without the serde aliases they'd fail to
    /// parse (or silently fall back to defaults), quietly discarding whatever
    /// caps the operator had configured.
    #[test]
    fn ai_memory_reads_pre_adr0003_state_keys() {
        let legacy = r#"(
            soul_bytes: 1,
            lore_bytes: 2,
            user_bytes: 3,
            state_bytes: 4,
            inject_byte_budget: 5,
            max_state_files: 6,
        )"#;
        let m: AiMemory = ron::from_str(legacy).expect("legacy state_* keys must still parse");
        assert_eq!(m.scratch_bytes, 4);
        assert_eq!(m.max_scratch_files, 6);
    }

    #[test]
    fn ai_memory_reads_new_scratch_keys() {
        let current = r#"(
            soul_bytes: 1,
            lore_bytes: 2,
            user_bytes: 3,
            scratch_bytes: 4,
            inject_byte_budget: 5,
            max_scratch_files: 6,
        )"#;
        let m: AiMemory = ron::from_str(current).expect("new keys must parse");
        assert_eq!(m.scratch_bytes, 4);
        assert_eq!(m.max_scratch_files, 6);
    }

    #[test]
    fn ai_emotes_defaults_pin_soul_reflexes_and_widen_window() {
        let e = AiEmotes::default();
        assert_eq!(e.max_prompt_emotes, 20, "default window widened 12 -> 20");
        assert_eq!(
            e.pinned_emotes,
            vec!["PepeLa".to_string(), "okjj".to_string()]
        );
    }

    #[test]
    fn defaults_match_legacy_ai_config_defaults() {
        let s = AiSettings::default();
        let aurora_profile = s.by_name(&s.aurora.profile).expect("aurora profile");
        assert_eq!(aurora_profile.timeout, 30);
        assert!(aurora_profile.base_url.is_none());
        assert_eq!(s.aurora.max_turn_rounds, 4);
        assert_eq!(s.aurora.max_writes_per_turn, 8);
        assert_eq!(s.aurora.reply_target_chars, 480);
        assert_eq!(s.aurora.reply_soft_chars, 490);
        assert_eq!(s.aurora.reply_hard_chars, 490);
        assert_eq!(
            s.history.length,
            crate::ai::chat_history::DEFAULT_HISTORY_LENGTH
        );
        assert_eq!(s.history.ai_channel_length, 100);
        assert_eq!(s.history.n, 20);
        assert_eq!(s.history.ai_channel_n, 10);
        assert!(s.recap.profile.is_none());
        assert_eq!(s.recap.batch_size, 40);
        assert_eq!(s.memory.soul_bytes, 4096);
        assert_eq!(s.memory.lore_bytes, 12_288);
        assert_eq!(s.memory.user_bytes, 4096);
        assert_eq!(s.memory.scratch_bytes, 2048);
        assert_eq!(s.memory.inject_byte_budget, 24_576);
        assert_eq!(s.memory.max_scratch_files, 16);
        assert!(s.dreamer.enabled);
        assert_eq!(s.dreamer.run_at, "04:00");
        assert_eq!(s.dreamer.timeout_secs, 120);
        assert_eq!(s.dreamer.max_rounds, 20);
        assert_eq!(s.dreamer.max_writes_per_turn, 32);
        assert!(s.prefill.is_none());
        assert!(s.web.is_none());
        assert!(s.emotes.is_none());
        assert_eq!(s.media.timeout, 60);
        assert_eq!(s.media.max_image_size.as_u64(), 10 * 1024 * 1024);
        assert!(aurora_profile.service_tier.is_none());
        assert_eq!(s.grok.profile, "default");
    }
}
