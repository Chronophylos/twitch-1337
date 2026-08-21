//! Override → effective `Settings` resolution. Each field falls through to the
//! compile-time default when the sparse override leaves it unset.

use super::ai::{AiBackendKind, AiProfile};
use super::overrides;
use super::provider::AiProvider;
use super::{
    AiEmotes, AiPrefill, AiSettings, AiWeb, AviationstackSettings, Cooldowns, EmoteTrackerSettings,
    PingsSettings, PotatAttributionSettings, SCHEMA_VERSION, Settings, SuspendSettings,
    TwitchRuntime, WebRuntime,
};

impl Settings {
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
            twitch: TwitchRuntime {
                expected_latency: overrides
                    .twitch
                    .expected_latency
                    .unwrap_or(defaults.twitch.expected_latency),
                hidden_admins: overrides
                    .twitch
                    .hidden_admins
                    .clone()
                    .unwrap_or_else(|| defaults.twitch.hidden_admins.clone()),
                viewer_allowlist: overrides
                    .twitch
                    .viewer_allowlist
                    .clone()
                    .unwrap_or_else(|| defaults.twitch.viewer_allowlist.clone()),
                emote_usage_deny_list: overrides
                    .twitch
                    .emote_usage_deny_list
                    .clone()
                    .unwrap_or_else(|| defaults.twitch.emote_usage_deny_list.clone()),
                admin_channel: match &overrides.twitch.admin_channel {
                    Some(v) => v.clone(),
                    None => defaults.twitch.admin_channel.clone(),
                },
                ai_channel: match &overrides.twitch.ai_channel {
                    Some(v) => v.clone(),
                    None => defaults.twitch.ai_channel.clone(),
                },
            },
            aviationstack: AviationstackSettings {
                enabled: overrides
                    .aviationstack
                    .enabled
                    .unwrap_or(defaults.aviationstack.enabled),
                base_url: overrides
                    .aviationstack
                    .base_url
                    .clone()
                    .unwrap_or_else(|| defaults.aviationstack.base_url.clone()),
                timeout_secs: overrides
                    .aviationstack
                    .timeout_secs
                    .unwrap_or(defaults.aviationstack.timeout_secs),
            },
            suspend: SuspendSettings {
                default_duration_secs: overrides
                    .suspend
                    .default_duration_secs
                    .unwrap_or(defaults.suspend.default_duration_secs),
            },
            emote_tracker: EmoteTrackerSettings {
                potat_attribution: PotatAttributionSettings {
                    enabled: overrides
                        .emote_tracker
                        .potat_attribution
                        .enabled
                        .unwrap_or(defaults.emote_tracker.potat_attribution.enabled),
                    window_secs: overrides
                        .emote_tracker
                        .potat_attribution
                        .window_secs
                        .unwrap_or(defaults.emote_tracker.potat_attribution.window_secs),
                    potat_7tv_user_id: overrides
                        .emote_tracker
                        .potat_attribution
                        .potat_7tv_user_id
                        .clone()
                        .unwrap_or_else(|| {
                            defaults
                                .emote_tracker
                                .potat_attribution
                                .potat_7tv_user_id
                                .clone()
                        }),
                },
            },
            web: WebRuntime {
                session_ttl_secs: overrides
                    .web
                    .session_ttl_secs
                    .unwrap_or(defaults.web.session_ttl_secs),
                mod_check_refresh_secs: overrides
                    .web
                    .mod_check_refresh_secs
                    .unwrap_or(defaults.web.mod_check_refresh_secs),
            },
            schedules: overrides
                .schedules
                .clone()
                .unwrap_or_else(|| defaults.schedules.clone()),
        }
    }
}

fn resolve_ai(defaults: &AiSettings, o: &overrides::AiOverrides) -> AiSettings {
    use super::ai::{AiBehavior, AiDreamer, AiGrok, AiHistory, AiMedia, AiMemory};
    AiSettings {
        profiles: resolve_profiles(defaults, o),
        active_profile: o
            .active_profile
            .clone()
            .or_else(|| {
                // MIGRATION(ai-profiles): a legacy connection block always
                // lands under the name "default".
                legacy_profile(o).map(|p| p.name)
            })
            .unwrap_or_else(|| defaults.active_profile.clone()),
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
            min_response_delay_ms: o
                .behavior
                .min_response_delay_ms
                .unwrap_or(defaults.behavior.min_response_delay_ms),
            reply_target_chars: o
                .behavior
                .reply_target_chars
                .unwrap_or(defaults.behavior.reply_target_chars),
            reply_soft_chars: o
                .behavior
                .reply_soft_chars
                .unwrap_or(defaults.behavior.reply_soft_chars),
            reply_hard_chars: o
                .behavior
                .reply_hard_chars
                .unwrap_or(defaults.behavior.reply_hard_chars),
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
            scratch_bytes: o
                .memory
                .scratch_bytes
                .unwrap_or(defaults.memory.scratch_bytes),
            inject_byte_budget: o
                .memory
                .inject_byte_budget
                .unwrap_or(defaults.memory.inject_byte_budget),
            max_scratch_files: o
                .memory
                .max_scratch_files
                .unwrap_or(defaults.memory.max_scratch_files),
        },
        dreamer: AiDreamer {
            enabled: o.dreamer.enabled.unwrap_or(defaults.dreamer.enabled),
            profile: match &o.dreamer.profile {
                Some(v) => v.clone(),
                None => defaults.dreamer.profile.clone(),
            },
            model: match &o.dreamer.model {
                Some(v) => v.clone(),
                None => defaults.dreamer.model.clone(),
            },
            reasoning_effort: match &o.dreamer.reasoning_effort {
                Some(v) => v.clone(),
                None => defaults.dreamer.reasoning_effort.clone(),
            },
            service_tier: match &o.dreamer.service_tier {
                Some(v) => v.clone(),
                None => defaults.dreamer.service_tier.clone(),
            },
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
            max_writes_per_turn: o
                .dreamer
                .max_writes_per_turn
                .unwrap_or(defaults.dreamer.max_writes_per_turn),
        },
        grok: AiGrok {
            profile: match &o.grok.profile {
                Some(v) => v.clone(),
                None => defaults.grok.profile.clone(),
            },
            model: match &o.grok.model {
                Some(v) => v.clone(),
                None => defaults.grok.model.clone(),
            },
            reasoning_effort: match &o.grok.reasoning_effort {
                Some(v) => v.clone(),
                None => defaults.grok.reasoning_effort.clone(),
            },
            service_tier: match &o.grok.service_tier {
                Some(v) => v.clone(),
                None => defaults.grok.service_tier.clone(),
            },
            // No override plumbing in v1 — baked default only (brief: dashboard
            // field is optional / not yet wired).
            persona_name: defaults.grok.persona_name.clone(),
        },
        prefill: resolve_prefill(defaults.prefill.as_ref(), &o.prefill),
        web: resolve_web(defaults.web.as_ref(), &o.web),
        emotes: resolve_emotes(defaults.emotes.as_ref(), &o.emotes),
        media: AiMedia {
            profile: match &o.media.profile {
                Some(v) => v.clone(),
                None => defaults.media.profile.clone(),
            },
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

/// MIGRATION(ai-profiles): fold a pre-schema-6 `ai.connection` override into
/// a single profile named "default". Returns `None` when the legacy block is
/// entirely empty (a fresh install, or a file already on the new shape).
fn legacy_profile(o: &overrides::AiOverrides) -> Option<AiProfile> {
    let c = &o.connection;
    if c.backend.is_none()
        && c.base_url.is_none()
        && c.model.is_none()
        && c.timeout.is_none()
        && c.reasoning_effort.is_none()
        && c.service_tier.is_none()
    {
        return None;
    }
    let base_url = c.base_url.clone().flatten();
    // Reproduces the pre-profiles sniff exactly: an empty base_url meant
    // OpenRouter's default, and only "openrouter.ai" in the URL unlocked
    // the reasoning-object + service-tier dialect.
    let provider = match c.backend.unwrap_or(AiBackendKind::OpenAi) {
        AiBackendKind::Ollama => AiProvider::Ollama,
        AiBackendKind::OpenAi => match base_url.as_deref() {
            None | Some("") => AiProvider::OpenRouter,
            Some(u) if u.contains("openrouter.ai") => AiProvider::OpenRouter,
            Some(_) => AiProvider::OpenAiCompatible,
        },
    };
    Some(AiProfile {
        name: "default".to_string(),
        key: None,
        provider,
        base_url: base_url.filter(|u| !u.is_empty()),
        model: c.model.clone().unwrap_or_default(),
        timeout: c.timeout.unwrap_or(30),
        reasoning_effort: c.reasoning_effort.clone().flatten(),
        service_tier: c.service_tier.clone().flatten(),
    })
}

fn resolve_profiles(defaults: &AiSettings, o: &overrides::AiOverrides) -> Vec<AiProfile> {
    if let Some(list) = &o.profiles {
        return list.clone();
    }
    if let Some(legacy) = legacy_profile(o) {
        return vec![legacy];
    }
    defaults.profiles.clone()
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
        pinned_emotes: o.pinned_emotes.clone().unwrap_or(base.pinned_emotes),
        base_url: match &o.base_url {
            Some(v) => v.clone(),
            None => base.base_url,
        },
    })
}

#[cfg(test)]
mod tests {
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
            cooldowns: CooldownsOverrides {
                ai: Some(15),
                ..Default::default()
            },
            ..SettingsOverrides::default()
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
        assert_eq!(resolved.ai.active().model, "gpt-5");
        assert_eq!(resolved.ai.active().timeout, defaults.ai.active().timeout);
    }

    #[test]
    fn service_tier_override_resolves_for_connection_and_dreamer() {
        let mut overrides = overrides::SettingsOverrides::default();
        overrides.ai.connection.service_tier = Some(Some("flex".to_string()));
        overrides.ai.dreamer.service_tier = Some(Some("priority".to_string()));
        let s = Settings::resolve(&Settings::compiled_defaults(), &overrides);
        assert_eq!(s.ai.active().service_tier.as_deref(), Some("flex"));
        assert_eq!(s.ai.dreamer.service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn service_tier_explicit_clear_resolves_to_none() {
        // Set a non-None default to prove `Some(None)` clears it.
        let mut defaults = Settings::compiled_defaults();
        defaults.ai.profiles[0].service_tier = Some("flex".to_string());
        let mut overrides = overrides::SettingsOverrides::default();
        overrides.ai.connection.service_tier = Some(None);
        let s = Settings::resolve(&defaults, &overrides);
        assert!(s.ai.active().service_tier.is_none());
    }

    #[test]
    fn twitch_override_resolves() {
        use crate::settings::overrides::TwitchOverrides;
        let defaults = Settings::compiled_defaults();
        let overrides = SettingsOverrides {
            twitch: TwitchOverrides {
                expected_latency: Some(250),
                hidden_admins: Some(vec!["111".into(), "222".into()]),
                admin_channel: Some(Some("admins".into())),
                ai_channel: Some(None), // explicit clear
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        let r = Settings::resolve(&defaults, &overrides);
        assert_eq!(r.twitch.expected_latency, 250);
        assert_eq!(r.twitch.hidden_admins, vec!["111", "222"]);
        assert_eq!(r.twitch.admin_channel.as_deref(), Some("admins"));
        assert!(r.twitch.ai_channel.is_none());
    }

    #[test]
    fn aviationstack_suspend_web_resolve() {
        use crate::settings::overrides::{
            AviationstackOverrides, SuspendOverrides, WebRuntimeOverrides,
        };
        let defaults = Settings::compiled_defaults();
        let overrides = SettingsOverrides {
            aviationstack: AviationstackOverrides {
                enabled: Some(true),
                timeout_secs: Some(7),
                ..Default::default()
            },
            suspend: SuspendOverrides {
                default_duration_secs: Some(900),
            },
            web: WebRuntimeOverrides {
                session_ttl_secs: Some(3600 * 12),
                mod_check_refresh_secs: Some(120),
            },
            ..SettingsOverrides::default()
        };
        let r = Settings::resolve(&defaults, &overrides);
        assert!(r.aviationstack.enabled);
        assert_eq!(r.aviationstack.timeout_secs, 7);
        assert_eq!(r.aviationstack.base_url, defaults.aviationstack.base_url);
        assert_eq!(r.suspend.default_duration_secs, 900);
        assert_eq!(r.web.session_ttl_secs, 3600 * 12);
        assert_eq!(r.web.mod_check_refresh_secs, 120);
    }

    #[test]
    fn emotes_pinned_override_wholesale_replaces() {
        use crate::settings::overrides::{AiEmotesOverrides, AiOverrides};
        let defaults = Settings::compiled_defaults();
        let overrides = SettingsOverrides {
            ai: AiOverrides {
                emotes: AiEmotesOverrides {
                    enabled: Some(true),
                    pinned_emotes: Some(vec!["PepeLa".into(), "Clueless".into()]),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        let r = Settings::resolve(&defaults, &overrides);
        let e = r.ai.emotes.expect("emotes enabled");
        assert_eq!(e.pinned_emotes, vec!["PepeLa", "Clueless"]);
    }

    #[test]
    fn emotes_pinned_none_override_resolves_to_default_seed() {
        use crate::settings::overrides::{AiEmotesOverrides, AiOverrides};
        let defaults = Settings::compiled_defaults();
        let overrides = SettingsOverrides {
            ai: AiOverrides {
                emotes: AiEmotesOverrides {
                    enabled: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..SettingsOverrides::default()
        };
        let r = Settings::resolve(&defaults, &overrides);
        let e = r.ai.emotes.expect("emotes enabled");
        assert_eq!(e.pinned_emotes, vec!["PepeLa", "okjj"]);
    }

    #[test]
    fn schedules_override_wholesale_replaces() {
        use crate::schedule::{Schedule, Trigger, WeekdaySet};
        let defaults = Settings::compiled_defaults();
        let overrides = overrides::SettingsOverrides {
            schedules: Some(vec![Schedule {
                name: "noon".into(),
                message: "hi".into(),
                trigger: Trigger::Interval {
                    every: std::time::Duration::from_secs(3600),
                    days: WeekdaySet::default(),
                    active_from: None,
                    active_to: None,
                },
                start_date: None,
                end_date: None,
                enabled: true,
            }]),
            ..overrides::SettingsOverrides::default()
        };
        let r = Settings::resolve(&defaults, &overrides);
        assert_eq!(r.schedules.len(), 1);
        assert_eq!(r.schedules[0].name, "noon");
    }

    #[test]
    fn schedules_none_override_resolves_to_defaults() {
        let defaults = Settings::compiled_defaults();
        let overrides = overrides::SettingsOverrides::default();
        let r = Settings::resolve(&defaults, &overrides);
        assert!(r.schedules.is_empty());
    }

    #[test]
    fn grok_overrides_resolve_and_default_to_none() {
        let defaults = Settings::compiled_defaults();
        let mut ovr = overrides::SettingsOverrides::default();
        // No grok patch → still None
        let s = Settings::resolve(&defaults, &ovr);
        assert!(s.ai.grok.model.is_none());
        assert!(s.ai.grok.reasoning_effort.is_none());
        assert!(s.ai.grok.service_tier.is_none());

        ovr.ai.grok.model = Some(Some("x-ai/grok-4".into()));
        ovr.ai.grok.reasoning_effort = Some(Some("high".into()));
        ovr.ai.grok.service_tier = Some(Some("flex".into()));
        let s = Settings::resolve(&defaults, &ovr);
        assert_eq!(s.ai.grok.model.as_deref(), Some("x-ai/grok-4"));
        assert_eq!(s.ai.grok.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(s.ai.grok.service_tier.as_deref(), Some("flex"));

        // Explicit clear
        ovr.ai.grok.model = Some(None);
        ovr.ai.grok.reasoning_effort = Some(None);
        ovr.ai.grok.service_tier = Some(None);
        let s = Settings::resolve(&defaults, &ovr);
        assert!(s.ai.grok.model.is_none());
        assert!(s.ai.grok.reasoning_effort.is_none());
        assert!(s.ai.grok.service_tier.is_none());
    }

    #[test]
    fn dreamer_write_cap_resolves_default_and_override() {
        // Empty overrides → compiled default (32).
        let s = Settings::resolve(
            &Settings::compiled_defaults(),
            &overrides::SettingsOverrides::default(),
        );
        assert_eq!(s.ai.dreamer.max_writes_per_turn, 32);

        // Explicit override wins.
        let mut ovr = overrides::SettingsOverrides::default();
        ovr.ai.dreamer.max_writes_per_turn = Some(50);
        let s = Settings::resolve(&Settings::compiled_defaults(), &ovr);
        assert_eq!(s.ai.dreamer.max_writes_per_turn, 50);
    }

    #[test]
    fn legacy_connection_override_folds_into_a_default_profile() {
        let mut o = SettingsOverrides::default();
        o.ai.connection.backend = Some(AiBackendKind::OpenAi);
        o.ai.connection.base_url = Some(Some("https://openrouter.ai/api/v1".into()));
        o.ai.connection.model = Some("z-ai/glm-4.6".into());
        o.ai.connection.reasoning_effort = Some(Some("high".into()));

        let s = Settings::resolve(&Settings::compiled_defaults(), &o);

        assert_eq!(s.ai.active_profile, "default");
        let p = s.ai.active();
        assert_eq!(p.name, "default");
        assert_eq!(p.provider, AiProvider::OpenRouter);
        assert_eq!(p.model, "z-ai/glm-4.6");
        assert_eq!(p.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn legacy_ollama_backend_folds_to_the_ollama_provider() {
        let mut o = SettingsOverrides::default();
        o.ai.connection.backend = Some(AiBackendKind::Ollama);
        let s = Settings::resolve(&Settings::compiled_defaults(), &o);
        assert_eq!(s.ai.active().provider, AiProvider::Ollama);
    }

    #[test]
    fn legacy_openai_with_a_third_party_url_folds_to_compatible() {
        let mut o = SettingsOverrides::default();
        o.ai.connection.backend = Some(AiBackendKind::OpenAi);
        o.ai.connection.base_url = Some(Some("https://api.deepseek.com/v1".into()));
        let s = Settings::resolve(&Settings::compiled_defaults(), &o);
        assert_eq!(s.ai.active().provider, AiProvider::OpenAiCompatible);
    }

    #[test]
    fn explicit_profiles_win_over_the_legacy_connection_block() {
        let mut o = SettingsOverrides::default();
        o.ai.connection.model = Some("legacy-model".into());
        o.ai.profiles = Some(vec![AiProfile {
            name: "local".into(),
            key: None,
            provider: AiProvider::Ollama,
            base_url: None,
            model: "qwen3:8b".into(),
            timeout: 30,
            reasoning_effort: None,
            service_tier: None,
        }]);
        o.ai.active_profile = Some("local".into());

        let s = Settings::resolve(&Settings::compiled_defaults(), &o);
        assert_eq!(s.ai.profiles.len(), 1);
        assert_eq!(s.ai.active().model, "qwen3:8b");
    }

    #[test]
    fn active_falls_back_to_the_first_profile_when_the_name_is_stale() {
        let mut o = SettingsOverrides::default();
        o.ai.profiles = Some(vec![AiProfile {
            name: "a".into(),
            key: None,
            provider: AiProvider::Ollama,
            base_url: None,
            model: "m".into(),
            timeout: 30,
            reasoning_effort: None,
            service_tier: None,
        }]);
        o.ai.active_profile = Some("gone".into());
        let s = Settings::resolve(&Settings::compiled_defaults(), &o);
        assert_eq!(s.ai.active().name, "a");
    }
}
