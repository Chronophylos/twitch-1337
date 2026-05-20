//! Dashboard-managed runtime settings.
//!
//! `Settings` is the fully-resolved snapshot read by command handlers via
//! a `SettingsHandle = Arc<ArcSwap<Settings>>`. Sparse `SettingsOverrides`
//! (see `overrides.rs`) live on disk at `$DATA_DIR/settings.ron`; missing
//! fields fall through to `compiled_defaults()`.
//!
//! Writes go through `SettingsStore::apply` (see `store.rs`) which
//! validates, atomically persists, swaps the handle, and appends an audit
//! entry.

pub mod ai;
pub mod audit;
pub mod migrate;
pub mod overrides;
pub mod store;

pub use ai::{
    AiBackendKind, AiBehavior, AiConnection, AiDreamer, AiEmotes, AiHistory, AiMedia, AiMemory,
    AiPrefill, AiSettings, AiWeb,
};
#[cfg(any(test, feature = "testing"))]
pub use audit::MemoryAuditLog;
pub use audit::{AuditChange, AuditEntry, AuditError, AuditLog, FileAuditLog};
pub use overrides::{AiOverrides, CooldownsOverrides, PingsOverrides, SettingsOverrides};
pub use store::{Actor, SettingsStore};

use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type SettingsHandle = Arc<ArcSwap<Settings>>;

pub const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    pub schema_version: u32,
    pub cooldowns: Cooldowns,
    pub pings: PingsSettings,
    pub ai: AiSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cooldowns {
    pub ai: u64,
    pub news: u64,
    pub up: u64,
    pub feedback: u64,
    pub doener: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PingsSettings {
    pub cooldown: u64,
    pub public: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Cooldowns,
    Pings,
    AiConnection,
    AiBehavior,
    AiHistory,
    AiMemory,
    AiDreamer,
    AiPrefill,
    AiWeb,
    AiEmotes,
    AiMedia,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("validation failed")]
    Validation(Vec<FieldError>),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("ron: {0}")]
    Ron(#[from] ron::error::SpannedError),
    #[error("persist: {0}")]
    Persist(#[from] crate::util::persist::AtomicPersistError),
}

impl From<Vec<FieldError>> for SettingsError {
    fn from(errs: Vec<FieldError>) -> Self {
        Self::Validation(errs)
    }
}

impl Settings {
    pub fn compiled_defaults() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            cooldowns: Cooldowns {
                ai: 30,
                news: 60,
                up: 30,
                feedback: 300,
                doener: 30,
            },
            pings: PingsSettings {
                cooldown: 300,
                public: false,
            },
            ai: AiSettings::default(),
        }
    }

    pub fn validate(&self) -> Result<(), Vec<FieldError>> {
        let mut errs = Vec::new();
        fn bound(name: &str, v: u64, lo: u64, hi: u64, errs: &mut Vec<FieldError>) {
            if v < lo || v > hi {
                errs.push(FieldError {
                    field: name.to_owned(),
                    message: format!("must be {lo}..={hi} seconds (got {v})"),
                });
            }
        }
        bound("cooldowns.ai", self.cooldowns.ai, 1, 3600, &mut errs);
        bound("cooldowns.news", self.cooldowns.news, 1, 3600, &mut errs);
        bound("cooldowns.up", self.cooldowns.up, 1, 3600, &mut errs);
        bound(
            "cooldowns.feedback",
            self.cooldowns.feedback,
            1,
            3600,
            &mut errs,
        );
        bound(
            "cooldowns.doener",
            self.cooldowns.doener,
            1,
            3600,
            &mut errs,
        );
        bound("pings.cooldown", self.pings.cooldown, 1, 86_400, &mut errs);
        validate_ai(&self.ai, &mut errs);
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }

    pub fn resolve(defaults: &Settings, overrides: &overrides::SettingsOverrides) -> Settings {
        Settings {
            schema_version: SCHEMA_VERSION,
            cooldowns: Cooldowns {
                ai: overrides.cooldowns.ai.unwrap_or(defaults.cooldowns.ai),
                news: overrides.cooldowns.news.unwrap_or(defaults.cooldowns.news),
                up: overrides.cooldowns.up.unwrap_or(defaults.cooldowns.up),
                feedback: overrides
                    .cooldowns
                    .feedback
                    .unwrap_or(defaults.cooldowns.feedback),
                doener: overrides
                    .cooldowns
                    .doener
                    .unwrap_or(defaults.cooldowns.doener),
            },
            pings: PingsSettings {
                cooldown: overrides.pings.cooldown.unwrap_or(defaults.pings.cooldown),
                public: overrides.pings.public.unwrap_or(defaults.pings.public),
            },
            ai: resolve_ai(&defaults.ai, &overrides.ai),
        }
    }
}

fn resolve_ai(defaults: &AiSettings, o: &overrides::AiOverrides) -> AiSettings {
    use ai::{AiBehavior, AiConnection, AiDreamer, AiHistory, AiMedia, AiMemory};
    AiSettings {
        connection: AiConnection {
            backend: o.connection.backend.unwrap_or(defaults.connection.backend),
            base_url: match &o.connection.base_url {
                Some(v) => v.clone(),
                None => defaults.connection.base_url.clone(),
            },
            model: o
                .connection
                .model
                .clone()
                .unwrap_or_else(|| defaults.connection.model.clone()),
            timeout: o.connection.timeout.unwrap_or(defaults.connection.timeout),
            reasoning_effort: match &o.connection.reasoning_effort {
                Some(v) => v.clone(),
                None => defaults.connection.reasoning_effort.clone(),
            },
            // TODO(Task 3): wire `o.connection.service_tier` once the override field exists.
            service_tier: defaults.connection.service_tier.clone(),
        },
        behavior: AiBehavior {
            max_turn_rounds: o
                .behavior
                .max_turn_rounds
                .unwrap_or(defaults.behavior.max_turn_rounds),
            max_writes_per_turn: o
                .behavior
                .max_writes_per_turn
                .unwrap_or(defaults.behavior.max_writes_per_turn),
            persona_name: o
                .behavior
                .persona_name
                .clone()
                .unwrap_or_else(|| defaults.behavior.persona_name.clone()),
        },
        history: AiHistory {
            length: o.history.length.unwrap_or(defaults.history.length),
            ai_channel_length: o
                .history
                .ai_channel_length
                .unwrap_or(defaults.history.ai_channel_length),
        },
        memory: AiMemory {
            soul_bytes: o.memory.soul_bytes.unwrap_or(defaults.memory.soul_bytes),
            lore_bytes: o.memory.lore_bytes.unwrap_or(defaults.memory.lore_bytes),
            user_bytes: o.memory.user_bytes.unwrap_or(defaults.memory.user_bytes),
            state_bytes: o.memory.state_bytes.unwrap_or(defaults.memory.state_bytes),
            inject_byte_budget: o
                .memory
                .inject_byte_budget
                .unwrap_or(defaults.memory.inject_byte_budget),
            max_state_files: o
                .memory
                .max_state_files
                .unwrap_or(defaults.memory.max_state_files),
        },
        dreamer: AiDreamer {
            enabled: o.dreamer.enabled.unwrap_or(defaults.dreamer.enabled),
            model: match &o.dreamer.model {
                Some(v) => v.clone(),
                None => defaults.dreamer.model.clone(),
            },
            reasoning_effort: match &o.dreamer.reasoning_effort {
                Some(v) => v.clone(),
                None => defaults.dreamer.reasoning_effort.clone(),
            },
            // TODO(Task 3): wire `o.dreamer.service_tier` once the override field exists.
            service_tier: defaults.dreamer.service_tier.clone(),
            run_at: o
                .dreamer
                .run_at
                .clone()
                .unwrap_or_else(|| defaults.dreamer.run_at.clone()),
            timeout_secs: o
                .dreamer
                .timeout_secs
                .unwrap_or(defaults.dreamer.timeout_secs),
            max_rounds: o.dreamer.max_rounds.unwrap_or(defaults.dreamer.max_rounds),
        },
        prefill: resolve_prefill(defaults.prefill.as_ref(), &o.prefill),
        web: resolve_web(defaults.web.as_ref(), &o.web),
        emotes: resolve_emotes(defaults.emotes.as_ref(), &o.emotes),
        media: AiMedia {
            model: o
                .media
                .model
                .clone()
                .unwrap_or_else(|| defaults.media.model.clone()),
            timeout: o.media.timeout.unwrap_or(defaults.media.timeout),
            max_image_size: o
                .media
                .max_image_size
                .unwrap_or(defaults.media.max_image_size),
            max_pdf_size: o.media.max_pdf_size.unwrap_or(defaults.media.max_pdf_size),
            max_audio_size: o
                .media
                .max_audio_size
                .unwrap_or(defaults.media.max_audio_size),
            max_video_size: o
                .media
                .max_video_size
                .unwrap_or(defaults.media.max_video_size),
            max_text_size: o
                .media
                .max_text_size
                .unwrap_or(defaults.media.max_text_size),
        },
    }
}

fn resolve_prefill(
    defaults: Option<&AiPrefill>,
    o: &overrides::AiPrefillOverrides,
) -> Option<AiPrefill> {
    let enabled = o.enabled.unwrap_or_else(|| defaults.is_some());
    if !enabled {
        return None;
    }
    let base = defaults.cloned().unwrap_or_default();
    Some(AiPrefill {
        base_url: o.base_url.clone().unwrap_or(base.base_url),
        threshold: o.threshold.unwrap_or(base.threshold),
    })
}

fn resolve_web(defaults: Option<&AiWeb>, o: &overrides::AiWebOverrides) -> Option<AiWeb> {
    let enabled = o.enabled.unwrap_or_else(|| defaults.is_some());
    if !enabled {
        return None;
    }
    let base = defaults.cloned().unwrap_or_default();
    Some(AiWeb {
        base_url: o.base_url.clone().unwrap_or(base.base_url),
        timeout: o.timeout.unwrap_or(base.timeout),
        max_results: o.max_results.unwrap_or(base.max_results),
        max_rounds: o.max_rounds.unwrap_or(base.max_rounds),
        cache_ttl_secs: o.cache_ttl_secs.unwrap_or(base.cache_ttl_secs),
        cache_capacity: o.cache_capacity.unwrap_or(base.cache_capacity),
    })
}

fn resolve_emotes(
    defaults: Option<&AiEmotes>,
    o: &overrides::AiEmotesOverrides,
) -> Option<AiEmotes> {
    let enabled = o.enabled.unwrap_or_else(|| defaults.is_some());
    if !enabled {
        return None;
    }
    let base = defaults.cloned().unwrap_or_default();
    Some(AiEmotes {
        include_global: o.include_global.unwrap_or(base.include_global),
        refresh_interval_secs: o
            .refresh_interval_secs
            .unwrap_or(base.refresh_interval_secs),
        max_prompt_emotes: o.max_prompt_emotes.unwrap_or(base.max_prompt_emotes),
        min_baseline_emotes: o.min_baseline_emotes.unwrap_or(base.min_baseline_emotes),
        base_url: match &o.base_url {
            Some(v) => v.clone(),
            None => base.base_url,
        },
    })
}

fn validate_ai(ai: &AiSettings, errs: &mut Vec<FieldError>) {
    fn err(errs: &mut Vec<FieldError>, field: &str, msg: String) {
        errs.push(FieldError {
            field: field.into(),
            message: msg,
        });
    }

    if !(1..=20).contains(&ai.behavior.max_turn_rounds) {
        err(
            errs,
            "ai.behavior.max_turn_rounds",
            format!("must be 1..=20 (got {})", ai.behavior.max_turn_rounds),
        );
    }
    if !(1..=64).contains(&ai.behavior.max_writes_per_turn) {
        err(
            errs,
            "ai.behavior.max_writes_per_turn",
            format!("must be 1..=64 (got {})", ai.behavior.max_writes_per_turn),
        );
    }
    if ai.behavior.persona_name.trim().is_empty() {
        err(errs, "ai.behavior.persona_name", "must not be empty".into());
    }
    if ai.history.length > crate::ai::chat_history::MAX_HISTORY_LENGTH {
        err(
            errs,
            "ai.history.length",
            format!("must be <= {}", crate::ai::chat_history::MAX_HISTORY_LENGTH),
        );
    }
    if ai.history.ai_channel_length > crate::ai::chat_history::MAX_HISTORY_LENGTH {
        err(
            errs,
            "ai.history.ai_channel_length",
            format!("must be <= {}", crate::ai::chat_history::MAX_HISTORY_LENGTH),
        );
    }
    if ai.memory.inject_byte_budget < ai.memory.soul_bytes + ai.memory.lore_bytes {
        err(
            errs,
            "ai.memory.inject_byte_budget",
            "must be >= soul_bytes + lore_bytes".into(),
        );
    }
    if !(1..=200).contains(&ai.dreamer.max_rounds) {
        err(
            errs,
            "ai.dreamer.max_rounds",
            format!("must be 1..=200 (got {})", ai.dreamer.max_rounds),
        );
    }
    if ai.dreamer.timeout_secs == 0 {
        err(errs, "ai.dreamer.timeout_secs", "must be > 0".into());
    }
    if chrono::NaiveTime::parse_from_str(&ai.dreamer.run_at, "%H:%M").is_err() {
        err(
            errs,
            "ai.dreamer.run_at",
            format!("must be HH:MM (got {:?})", ai.dreamer.run_at),
        );
    }
    for (field, val) in [
        (
            "ai.connection.reasoning_effort",
            ai.connection.reasoning_effort.as_deref(),
        ),
        (
            "ai.dreamer.reasoning_effort",
            ai.dreamer.reasoning_effort.as_deref(),
        ),
    ] {
        if let Some(v) = val
            && v.trim().is_empty()
        {
            err(errs, field, "must be non-empty when set".into());
        }
    }
    if let Some(url) = ai.connection.base_url.as_deref()
        && reqwest::Url::parse(url).is_err()
    {
        err(
            errs,
            "ai.connection.base_url",
            format!("must be a valid URL (got {url:?})"),
        );
    }
    if ai.connection.timeout == 0 {
        err(errs, "ai.connection.timeout", "must be > 0".into());
    }
    if let Some(prefill) = &ai.prefill {
        if reqwest::Url::parse(&prefill.base_url).is_err() {
            err(
                errs,
                "ai.prefill.base_url",
                format!("must be a valid URL (got {:?})", prefill.base_url),
            );
        }
        if !(0.0..=1.0).contains(&prefill.threshold) {
            err(
                errs,
                "ai.prefill.threshold",
                format!("must be 0.0..=1.0 (got {})", prefill.threshold),
            );
        }
        if ai.history.length == 0 {
            err(errs, "ai.prefill", "requires ai.history.length > 0".into());
        }
    }
    if let Some(web) = &ai.web {
        if reqwest::Url::parse(&web.base_url).is_err() {
            err(
                errs,
                "ai.web.base_url",
                format!("must be a valid URL (got {:?})", web.base_url),
            );
        }
        if !(1..=10).contains(&web.max_results) {
            err(
                errs,
                "ai.web.max_results",
                format!("must be 1..=10 (got {})", web.max_results),
            );
        }
        if !(1..=6).contains(&web.max_rounds) {
            err(
                errs,
                "ai.web.max_rounds",
                format!("must be 1..=6 (got {})", web.max_rounds),
            );
        }
        if web.cache_capacity == 0 {
            err(errs, "ai.web.cache_capacity", "must be > 0".into());
        }
    }
    if let Some(em) = &ai.emotes {
        if em.refresh_interval_secs == 0 {
            err(
                errs,
                "ai.emotes.refresh_interval_secs",
                "must be > 0".into(),
            );
        }
        if !(1..=200).contains(&em.max_prompt_emotes) {
            err(
                errs,
                "ai.emotes.max_prompt_emotes",
                format!("must be 1..=200 (got {})", em.max_prompt_emotes),
            );
        }
        if em.min_baseline_emotes > em.max_prompt_emotes {
            err(
                errs,
                "ai.emotes.min_baseline_emotes",
                "must be <= max_prompt_emotes".into(),
            );
        }
        if let Some(url) = em.base_url.as_deref()
            && url.trim().is_empty()
        {
            err(
                errs,
                "ai.emotes.base_url",
                "must be non-empty when set".into(),
            );
        }
    }
}

#[cfg(any(test, feature = "testing"))]
pub fn test_handle() -> SettingsHandle {
    Arc::new(ArcSwap::from_pointee(Settings::compiled_defaults()))
}

#[cfg(test)]
mod resolve_tests {
    use super::overrides::{CooldownsOverrides, PingsOverrides, SettingsOverrides};
    use super::*;

    #[test]
    fn empty_overrides_equal_defaults() {
        let defaults = Settings::compiled_defaults();
        let overrides = SettingsOverrides::default();
        assert_eq!(Settings::resolve(&defaults, &overrides), defaults);
    }

    #[test]
    fn cooldown_override_wins_per_field() {
        let defaults = Settings::compiled_defaults();
        let overrides = SettingsOverrides {
            schema_version: SCHEMA_VERSION,
            cooldowns: CooldownsOverrides {
                ai: Some(15),
                ..Default::default()
            },
            pings: PingsOverrides::default(),
            ai: Default::default(),
        };
        let resolved = Settings::resolve(&defaults, &overrides);
        assert_eq!(resolved.cooldowns.ai, 15);
        assert_eq!(resolved.cooldowns.news, defaults.cooldowns.news);
        assert_eq!(resolved.pings, defaults.pings);
    }

    #[test]
    fn pings_public_override_flips_bool() {
        let defaults = Settings::compiled_defaults();
        let overrides = SettingsOverrides {
            pings: PingsOverrides {
                public: Some(true),
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        let resolved = Settings::resolve(&defaults, &overrides);
        assert!(resolved.pings.public);
        assert_eq!(resolved.pings.cooldown, defaults.pings.cooldown);
    }

    #[test]
    fn pings_cooldown_override_leaves_public_at_default() {
        let defaults = Settings::compiled_defaults();
        let overrides = SettingsOverrides {
            pings: PingsOverrides {
                cooldown: Some(600),
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        let resolved = Settings::resolve(&defaults, &overrides);
        assert_eq!(resolved.pings.cooldown, 600);
        assert_eq!(resolved.pings.public, defaults.pings.public);
    }

    #[test]
    fn validate_collects_multiple_errors() {
        let mut s = Settings::compiled_defaults();
        s.cooldowns.ai = 0;
        s.pings.cooldown = 0;
        let errs = s.validate().expect_err("both bounds violated");
        let fields: Vec<&str> = errs.iter().map(|e| e.field.as_str()).collect();
        assert!(fields.contains(&"cooldowns.ai"));
        assert!(fields.contains(&"pings.cooldown"));
    }

    #[test]
    fn validate_accepts_compiled_defaults() {
        Settings::compiled_defaults()
            .validate()
            .expect("compiled defaults pass validate()");
    }

    #[test]
    fn compiled_defaults_include_ai_block_v2() {
        let s = Settings::compiled_defaults();
        assert_eq!(s.schema_version, 2);
        assert_eq!(s.ai, AiSettings::default());
    }

    #[test]
    fn ai_connection_model_override_wins() {
        use crate::settings::overrides::{AiConnectionOverrides, AiOverrides};

        let defaults = Settings::compiled_defaults();
        let overrides = SettingsOverrides {
            ai: AiOverrides {
                connection: AiConnectionOverrides {
                    model: Some("gpt-5".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        let resolved = Settings::resolve(&defaults, &overrides);
        assert_eq!(resolved.ai.connection.model, "gpt-5");
        assert_eq!(
            resolved.ai.connection.timeout,
            defaults.ai.connection.timeout
        );
    }

    #[test]
    fn validate_rejects_empty_persona_name() {
        let mut s = Settings::compiled_defaults();
        s.ai.behavior.persona_name = "   ".into();
        let errs = s.validate().expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "ai.behavior.persona_name"));
    }

    #[test]
    fn validate_rejects_max_turn_rounds_out_of_range() {
        let mut s = Settings::compiled_defaults();
        s.ai.behavior.max_turn_rounds = 0;
        let errs = s.validate().expect_err("must fail");
        assert!(
            errs.iter()
                .any(|e| e.field == "ai.behavior.max_turn_rounds")
        );
    }

    #[test]
    fn validate_rejects_inject_budget_below_soul_plus_lore() {
        let mut s = Settings::compiled_defaults();
        s.ai.memory.soul_bytes = 4096;
        s.ai.memory.lore_bytes = 12288;
        s.ai.memory.inject_byte_budget = 1024;
        let errs = s.validate().expect_err("must fail");
        assert!(
            errs.iter()
                .any(|e| e.field == "ai.memory.inject_byte_budget")
        );
    }

    #[test]
    fn validate_rejects_malformed_dreamer_run_at() {
        let mut s = Settings::compiled_defaults();
        s.ai.dreamer.run_at = "not-a-time".into();
        let errs = s.validate().expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "ai.dreamer.run_at"));
    }

    #[test]
    fn validate_rejects_invalid_connection_base_url() {
        let mut s = Settings::compiled_defaults();
        s.ai.connection.base_url = Some("not a url".into());
        let errs = s.validate().expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "ai.connection.base_url"));
    }
}
