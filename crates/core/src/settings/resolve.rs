//! Override → effective `Settings` resolution. Each field falls through to the
//! compile-time default when the sparse override leaves it unset.

use super::ai::{AiBackendKind, AiProfile};
use super::overrides;
use super::provider::AiProvider;
use super::{
    AiEmotes, AiPrefill, AiSettings, AiWeb, AlertsSettings, AviationstackSettings, CmeAlerts,
    Cooldowns, EmoteTrackerSettings, PingsSettings, PotatAttributionSettings, QuakeAlerts,
    SCHEMA_VERSION, Settings, SolarAlerts, SuspendSettings, TwitchRuntime, WebRuntime,
};

impl Settings {
    pub fn resolve(defaults: &Settings, overrides: &overrides::SettingsOverrides) -> Settings {
        let mut resolved = Self::resolve_unhealed(defaults, overrides);
        heal_recap_history(&mut resolved.ai.history, &mut resolved.ai.recap);
        resolved
    }

    pub(crate) fn resolve_unhealed(
        defaults: &Settings,
        overrides: &overrides::SettingsOverrides,
    ) -> Settings {
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
            alerts: AlertsSettings {
                quake: QuakeAlerts {
                    enabled: overrides
                        .alerts
                        .quake
                        .enabled
                        .unwrap_or(defaults.alerts.quake.enabled),
                    poll_secs: overrides
                        .alerts
                        .quake
                        .poll_secs
                        .unwrap_or(defaults.alerts.quake.poll_secs),
                    min_mag_tenths: overrides
                        .alerts
                        .quake
                        .min_mag_tenths
                        .unwrap_or(defaults.alerts.quake.min_mag_tenths),
                },
                solar: SolarAlerts {
                    enabled: overrides
                        .alerts
                        .solar
                        .enabled
                        .unwrap_or(defaults.alerts.solar.enabled),
                    poll_secs: overrides
                        .alerts
                        .solar
                        .poll_secs
                        .unwrap_or(defaults.alerts.solar.poll_secs),
                    min_class: overrides
                        .alerts
                        .solar
                        .min_class
                        .clone()
                        .unwrap_or_else(|| defaults.alerts.solar.min_class.clone()),
                },
                cme: CmeAlerts {
                    enabled: overrides
                        .alerts
                        .cme
                        .enabled
                        .unwrap_or(defaults.alerts.cme.enabled),
                    poll_secs: overrides
                        .alerts
                        .cme
                        .poll_secs
                        .unwrap_or(defaults.alerts.cme.poll_secs),
                },
                max_age_secs: overrides
                    .alerts
                    .max_age_secs
                    .unwrap_or(defaults.alerts.max_age_secs),
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

fn heal_recap_history(
    history: &mut crate::settings::ai::AiHistory,
    recap: &mut crate::settings::ai::AiRecap,
) {
    heal_history_window(
        "ai.history.length",
        &mut history.length,
        "ai.history.n",
        &mut history.n,
        &mut recap.batch_size,
    );
    heal_history_window(
        "ai.history.ai_channel_length",
        &mut history.ai_channel_length,
        "ai.history.ai_channel_n",
        &mut history.ai_channel_n,
        &mut recap.batch_size,
    );
}

fn heal_history_window(
    length_name: &str,
    length: &mut u64,
    n_name: &str,
    n: &mut u64,
    batch_size: &mut u64,
) {
    if *length == 0 || *length >= n.saturating_add(*batch_size) {
        return;
    }

    let original_n = *n;
    let original_batch_size = *batch_size;
    if *length < 2 {
        *length = 0;
        tracing::warn!(
            %length_name,
            original_length = 1,
            "disabled legacy recap history with insufficient capacity"
        );
        return;
    }
    *batch_size = (*length).saturating_sub(1).min(*batch_size).max(1);
    *n = (*length).saturating_sub(*batch_size).max(1);
    tracing::warn!(
        %length_name,
        length = *length,
        %n_name,
        old_n = original_n,
        n = *n,
        old_batch_size = original_batch_size,
        batch_size = *batch_size,
        "clamped legacy recap history settings to fit history capacity"
    );
}

fn resolve_ai(defaults: &AiSettings, o: &overrides::AiOverrides) -> AiSettings {
    use super::ai::{AiAurora, AiDreamer, AiGrok, AiHistory, AiMedia, AiMemory, AiRecap};

    let profiles = resolve_profiles(defaults, o);
    let legacy_active = o
        .active_profile
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| legacy_profile(o).map(|p| p.name))
        .unwrap_or_else(|| "default".to_string());

    AiSettings {
        profiles,
        aurora: AiAurora {
            profile: fold_required_profile_slot(&o.aurora.profile, &legacy_active),
            max_turn_rounds: o
                .aurora
                .max_turn_rounds
                .or(o.behavior.max_turn_rounds)
                .unwrap_or(defaults.aurora.max_turn_rounds),
            max_writes_per_turn: o
                .aurora
                .max_writes_per_turn
                .or(o.behavior.max_writes_per_turn)
                .unwrap_or(defaults.aurora.max_writes_per_turn),
            persona_name: o
                .aurora
                .persona_name
                .clone()
                .or_else(|| o.behavior.persona_name.clone())
                .unwrap_or_else(|| defaults.aurora.persona_name.clone()),
            min_response_delay_ms: o
                .aurora
                .min_response_delay_ms
                .or(o.behavior.min_response_delay_ms)
                .unwrap_or(defaults.aurora.min_response_delay_ms),
            reply_target_chars: o
                .aurora
                .reply_target_chars
                .or(o.behavior.reply_target_chars)
                .unwrap_or(defaults.aurora.reply_target_chars),
            reply_soft_chars: o
                .aurora
                .reply_soft_chars
                .or(o.behavior.reply_soft_chars)
                .unwrap_or(defaults.aurora.reply_soft_chars),
            reply_hard_chars: o
                .aurora
                .reply_hard_chars
                .or(o.behavior.reply_hard_chars)
                .unwrap_or(defaults.aurora.reply_hard_chars),
        },
        grok: AiGrok {
            profile: fold_required_profile_slot(&o.grok.profile, &legacy_active),
            persona_name: o
                .grok
                .persona_name
                .clone()
                .unwrap_or_else(|| defaults.grok.persona_name.clone()),
        },
        recap: AiRecap {
            profile: fold_opt_string(o.recap.profile.as_ref(), o.history.recap_profile.as_ref()),
            batch_size: o
                .recap
                .batch_size
                .or(o.history.batch_size)
                .unwrap_or(defaults.recap.batch_size),
            max_notes: o
                .recap
                .max_notes
                .or(o.history.max_notes)
                .unwrap_or(defaults.recap.max_notes),
            merge_width: o
                .recap
                .merge_width
                .or(o.history.merge_width)
                .unwrap_or(defaults.recap.merge_width),
        },
        dreamer: AiDreamer {
            enabled: o.dreamer.enabled.unwrap_or(defaults.dreamer.enabled),
            profile: fold_required_profile_slot(&o.dreamer.profile, &legacy_active),
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
        media: AiMedia {
            profile: fold_required_profile_slot(&o.media.profile, &legacy_active),
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
        history: AiHistory {
            length: o.history.length.unwrap_or(defaults.history.length),
            ai_channel_length: o
                .history
                .ai_channel_length
                .unwrap_or(defaults.history.ai_channel_length),
            n: o.history.n.unwrap_or(defaults.history.n),
            ai_channel_n: o
                .history
                .ai_channel_n
                .unwrap_or(defaults.history.ai_channel_n),
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
        prefill: resolve_prefill(defaults.prefill.as_ref(), &o.prefill),
        web: resolve_web(defaults.web.as_ref(), &o.web),
        emotes: resolve_emotes(defaults.emotes.as_ref(), &o.emotes),
    }
}

fn fold_required_profile_slot(slot: &Option<Option<String>>, legacy_active: &str) -> String {
    match slot {
        Some(Some(name)) if !name.trim().is_empty() => name.clone(),
        _ => legacy_active.to_string(),
    }
}

fn fold_opt_string(new: Option<&Option<String>>, old: Option<&Option<String>>) -> Option<String> {
    match new {
        Some(v) => v.clone(),
        None => old.and_then(Clone::clone),
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
    let caps = provider.caps();
    Some(AiProfile {
        name: "default".to_string(),
        key: None,
        provider,
        base_url: base_url.filter(|u| !u.is_empty()),
        model: c.model.clone().unwrap_or_default(),
        timeout: c.timeout.unwrap_or(30),
        // The old dashboard hid these rows for backends that couldn't use
        // them rather than clearing the stored value (a hidden row is
        // absent from the POST, which the tri-state merge reads as
        // "unchanged"). Drop what the mapped provider can't accept so a
        // stale value from a since-abandoned backend can't fail the new,
        // stricter caps validation and quarantine the whole file — the
        // client already dropped these on the wire under the old caps-blind
        // validation (`OllamaClient` never serialises either field, and the
        // old `OpenAiClient` stripped the tier off non-OpenRouter dialects),
        // so this changes nothing an operator could observe.
        reasoning_effort: c
            .reasoning_effort
            .clone()
            .flatten()
            .filter(|v| caps.allows_effort(v.trim())),
        service_tier: c
            .service_tier
            .clone()
            .flatten()
            .filter(|_| caps.service_tier),
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

    fn profile_named(name: &str) -> AiProfile {
        AiProfile {
            name: name.into(),
            key: None,
            provider: AiProvider::Ollama,
            base_url: Some("http://127.0.0.1:1".into()),
            model: name.into(),
            timeout: 30,
            reasoning_effort: None,
            service_tier: None,
        }
    }

    #[test]
    fn empty_overrides_equal_defaults() {
        let defaults = Settings::compiled_defaults();
        let overrides = SettingsOverrides::default();
        assert_eq!(Settings::resolve(&defaults, &overrides), defaults);
    }

    #[test]
    fn legacy_history_length_is_healed_to_fit_default_recap_window() {
        let defaults = Settings::compiled_defaults();
        let mut overrides = SettingsOverrides::default();
        overrides.ai.history.length = Some(50);

        let resolved = Settings::resolve(&defaults, &overrides);
        assert_eq!(resolved.ai.history.length, 50);
        assert!(resolved.ai.history.n + resolved.ai.recap.batch_size <= 50);
        resolved
            .validate(&crate::settings::ValidationContext {
                channel: "test".into(),
            })
            .expect("healed settings must validate");
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
        assert_eq!(
            resolved
                .ai
                .by_name(&resolved.ai.aurora.profile)
                .unwrap()
                .model,
            "gpt-5"
        );
        assert_eq!(
            resolved
                .ai
                .by_name(&resolved.ai.aurora.profile)
                .unwrap()
                .timeout,
            defaults
                .ai
                .by_name(&defaults.ai.aurora.profile)
                .unwrap()
                .timeout
        );
    }

    #[test]
    fn service_tier_override_resolves_for_connection() {
        let mut overrides = overrides::SettingsOverrides::default();
        overrides.ai.connection.service_tier = Some(Some("flex".to_string()));
        let s = Settings::resolve(&Settings::compiled_defaults(), &overrides);
        assert_eq!(
            s.ai.by_name(&s.ai.aurora.profile)
                .unwrap()
                .service_tier
                .as_deref(),
            Some("flex")
        );
    }

    #[test]
    fn service_tier_explicit_clear_resolves_to_none() {
        // Set a non-None default to prove `Some(None)` clears it.
        let mut defaults = Settings::compiled_defaults();
        defaults.ai.profiles[0].service_tier = Some("flex".to_string());
        let mut overrides = overrides::SettingsOverrides::default();
        overrides.ai.connection.service_tier = Some(None);
        let s = Settings::resolve(&defaults, &overrides);
        assert!(
            s.ai.by_name(&s.ai.aurora.profile)
                .unwrap()
                .service_tier
                .is_none()
        );
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
    fn grok_profile_override_resolves_and_inherits_when_cleared() {
        let defaults = Settings::compiled_defaults();
        let mut ovr = overrides::SettingsOverrides::default();
        let s = Settings::resolve(&defaults, &ovr);
        assert_eq!(s.ai.grok.profile, "default");

        ovr.ai.grok.profile = Some(Some("cheap".into()));
        let s = Settings::resolve(&defaults, &ovr);
        assert_eq!(s.ai.grok.profile, "cheap");

        ovr.ai.grok.profile = Some(None);
        let s = Settings::resolve(&defaults, &ovr);
        assert_eq!(s.ai.grok.profile, "default");
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

        assert_eq!(s.ai.aurora.profile, "default");
        let p = s.ai.by_name(&s.ai.aurora.profile).unwrap();
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
        assert_eq!(
            s.ai.by_name(&s.ai.aurora.profile).unwrap().provider,
            AiProvider::Ollama
        );
    }

    #[test]
    fn legacy_openai_with_a_third_party_url_folds_to_compatible() {
        let mut o = SettingsOverrides::default();
        o.ai.connection.backend = Some(AiBackendKind::OpenAi);
        o.ai.connection.base_url = Some(Some("https://api.deepseek.com/v1".into()));
        let s = Settings::resolve(&Settings::compiled_defaults(), &o);
        assert_eq!(
            s.ai.by_name(&s.ai.aurora.profile).unwrap().provider,
            AiProvider::OpenAiCompatible
        );
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
        assert_eq!(
            s.ai.by_name(&s.ai.aurora.profile).unwrap().model,
            "qwen3:8b"
        );
    }

    #[test]
    fn alerts_resolve_and_fallthrough() {
        let defaults = Settings::compiled_defaults();
        let overrides = crate::settings::overrides::SettingsOverrides {
            alerts: crate::settings::overrides::AlertsOverrides {
                quake: crate::settings::overrides::QuakeAlertsOverrides {
                    enabled: Some(true),
                    min_mag_tenths: Some(40),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let r = Settings::resolve(&defaults, &overrides);
        assert!(r.alerts.quake.enabled);
        assert_eq!(r.alerts.quake.min_mag_tenths, 40);
        // untouched fields fall through to defaults
        assert_eq!(r.alerts.quake.poll_secs, 300);
        assert!(!r.alerts.cme.enabled);
        assert_eq!(r.alerts.cme.poll_secs, 900);
        assert_eq!(r.alerts.solar.min_class, "M");
        assert_eq!(r.alerts.max_age_secs, 7_200);
    }

    #[test]
    fn v6_stale_active_profile_remains_a_required_agent_pointer() {
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
        assert_eq!(s.ai.aurora.profile, "gone");
    }

    #[test]
    fn v6_active_profile_stamps_required_agents_and_leaves_recap_off() {
        let defaults = Settings::compiled_defaults();
        let mut o = SettingsOverrides::default();
        o.ai.profiles = Some(vec![crate::settings::ai::AiProfile {
            name: "cheap".into(),
            key: None,
            provider: AiProvider::Ollama,
            base_url: Some("http://127.0.0.1:1".into()),
            model: "cheap-model".into(),
            timeout: 30,
            reasoning_effort: None,
            service_tier: None,
        }]);
        o.ai.active_profile = Some("cheap".into());
        let s = Settings::resolve(&defaults, &o);
        assert_eq!(s.ai.aurora.profile, "cheap");
        assert_eq!(s.ai.grok.profile, "cheap");
        assert_eq!(s.ai.dreamer.profile, "cheap");
        assert_eq!(s.ai.media.profile, "cheap");
        assert!(s.ai.recap.profile.is_none());
    }

    #[test]
    fn v6_grok_pin_wins_over_active_profile() {
        let defaults = Settings::compiled_defaults();
        let mut o = SettingsOverrides::default();
        o.ai.profiles = Some(vec![profile_named("cheap"), profile_named("local")]);
        o.ai.active_profile = Some("cheap".into());
        o.ai.grok.profile = Some(Some("local".into()));
        let s = Settings::resolve(&defaults, &o);
        assert_eq!(s.ai.aurora.profile, "cheap");
        assert_eq!(s.ai.grok.profile, "local");
        assert_eq!(s.ai.dreamer.profile, "cheap");
    }

    #[test]
    fn v6_history_recap_profile_lands_on_ai_recap() {
        let defaults = Settings::compiled_defaults();
        let mut o = SettingsOverrides::default();
        o.ai.profiles = Some(vec![profile_named("tiny")]);
        o.ai.history.recap_profile = Some(Some("tiny".into()));
        o.ai.history.batch_size = Some(7);
        o.ai.history.max_notes = Some(4);
        o.ai.history.merge_width = Some(2);
        let s = Settings::resolve(&defaults, &o);
        assert_eq!(s.ai.recap.profile.as_deref(), Some("tiny"));
        assert_eq!(s.ai.recap.batch_size, 7);
        assert_eq!(s.ai.recap.max_notes, 4);
        assert_eq!(s.ai.recap.merge_width, 2);
        // history no longer carries recap fields
    }

    #[test]
    fn v6_behavior_persona_lands_on_aurora() {
        let defaults = Settings::compiled_defaults();
        let mut o = SettingsOverrides::default();
        o.ai.behavior.persona_name = Some("Nyx".into());
        o.ai.behavior.max_turn_rounds = Some(6);
        let s = Settings::resolve(&defaults, &o);
        assert_eq!(s.ai.aurora.persona_name, "Nyx");
        assert_eq!(s.ai.aurora.max_turn_rounds, 6);
    }

    #[test]
    fn aurora_override_wins_over_legacy_behavior() {
        let defaults = Settings::compiled_defaults();
        let mut o = SettingsOverrides::default();
        o.ai.behavior.persona_name = Some("Nyx".into());
        o.ai.aurora.persona_name = Some("Vesper".into());
        let s = Settings::resolve(&defaults, &o);
        assert_eq!(s.ai.aurora.persona_name, "Vesper");
    }

    #[test]
    fn missing_active_profile_stamps_default() {
        let defaults = Settings::compiled_defaults();
        let o = SettingsOverrides::default();
        let s = Settings::resolve(&defaults, &o);
        assert_eq!(s.ai.aurora.profile, "default");
        assert_eq!(s.ai.grok.profile, "default");
        assert!(s.ai.recap.profile.is_none());
    }
}
