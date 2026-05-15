//! AI subsection of dashboard settings.
//!
//! Mirrors the runtime shape of the old `core::config::AiConfig`
//! minus the `api_key` secret. Defaults intentionally match the
//! pre-hoist behavior so existing deployments see no drift after
//! the schema bump.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiSettings {
    pub connection: AiConnection,
    pub behavior: AiBehavior,
    pub history: AiHistory,
    pub memory: AiMemory,
    pub dreamer: AiDreamer,
    pub prefill: Option<AiPrefill>,
    pub web: Option<AiWeb>,
    pub emotes: Option<AiEmotes>,
    pub media: AiMedia,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiConnection {
    pub backend: AiBackendKind,
    pub base_url: Option<String>,
    pub model: String,
    pub timeout: u64,
    pub reasoning_effort: Option<String>,
}

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
pub struct AiBehavior {
    pub max_turn_rounds: usize,
    pub max_writes_per_turn: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiHistory {
    pub length: u64,
    pub ai_channel_length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiMemory {
    pub soul_bytes: usize,
    pub lore_bytes: usize,
    pub user_bytes: usize,
    pub state_bytes: usize,
    pub inject_byte_budget: usize,
    pub max_state_files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiDreamer {
    pub enabled: bool,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub run_at: String,
    pub timeout_secs: u64,
    pub max_rounds: usize,
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
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiMedia {
    pub model: String,
    pub timeout: u64,
    pub max_image_size: bytesize::ByteSize,
    pub max_pdf_size: bytesize::ByteSize,
    pub max_audio_size: bytesize::ByteSize,
    pub max_video_size: bytesize::ByteSize,
    pub max_text_size: bytesize::ByteSize,
}

impl Default for AiConnection {
    fn default() -> Self {
        Self {
            backend: AiBackendKind::OpenAi,
            base_url: None,
            model: String::new(),
            timeout: 30,
            reasoning_effort: None,
        }
    }
}

impl Default for AiBehavior {
    fn default() -> Self {
        Self {
            max_turn_rounds: 4,
            max_writes_per_turn: 8,
        }
    }
}

impl Default for AiHistory {
    fn default() -> Self {
        Self {
            length: crate::ai::chat_history::DEFAULT_HISTORY_LENGTH,
            ai_channel_length: 50,
        }
    }
}

impl Default for AiMemory {
    fn default() -> Self {
        Self {
            soul_bytes: 4096,
            lore_bytes: 12_288,
            user_bytes: 4096,
            state_bytes: 2048,
            inject_byte_budget: 24_576,
            max_state_files: 16,
        }
    }
}

impl Default for AiDreamer {
    fn default() -> Self {
        Self {
            enabled: true,
            model: None,
            reasoning_effort: None,
            run_at: "04:00".into(),
            timeout_secs: 120,
            max_rounds: 20,
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
            base_url: "http://localhost:8080/search".into(),
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
            max_prompt_emotes: 12,
            min_baseline_emotes: 4,
            base_url: None,
        }
    }
}

impl Default for AiMedia {
    fn default() -> Self {
        Self {
            model: "~google/gemini-flash-latest".into(),
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
    fn defaults_match_legacy_ai_config_defaults() {
        let s = AiSettings::default();
        assert_eq!(s.connection.timeout, 30);
        assert!(s.connection.base_url.is_none());
        assert_eq!(s.behavior.max_turn_rounds, 4);
        assert_eq!(s.behavior.max_writes_per_turn, 8);
        assert_eq!(
            s.history.length,
            crate::ai::chat_history::DEFAULT_HISTORY_LENGTH
        );
        assert_eq!(s.history.ai_channel_length, 50);
        assert_eq!(s.memory.soul_bytes, 4096);
        assert_eq!(s.memory.lore_bytes, 12_288);
        assert_eq!(s.memory.user_bytes, 4096);
        assert_eq!(s.memory.state_bytes, 2048);
        assert_eq!(s.memory.inject_byte_budget, 24_576);
        assert_eq!(s.memory.max_state_files, 16);
        assert!(s.dreamer.enabled);
        assert_eq!(s.dreamer.run_at, "04:00");
        assert_eq!(s.dreamer.timeout_secs, 120);
        assert_eq!(s.dreamer.max_rounds, 20);
        assert!(s.prefill.is_none());
        assert!(s.web.is_none());
        assert!(s.emotes.is_none());
        assert_eq!(s.media.model, "~google/gemini-flash-latest");
        assert_eq!(s.media.timeout, 60);
        assert_eq!(s.media.max_image_size.as_u64(), 10 * 1024 * 1024);
    }
}
