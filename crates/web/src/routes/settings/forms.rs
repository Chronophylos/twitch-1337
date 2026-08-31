//! Per-card form deserialization + field conversion. Each `*Form` maps the
//! flat HTML form fields for one settings card into the matching sparse
//! `*Overrides`. `overrides_from_save_form` bundles the whole submitted
//! `SaveForm` into a single `SettingsOverrides` patch.

use serde::Deserialize;
use twitch_1337_core::settings::overrides::{
    AiAuroraOverrides, AiConnectionOverrides, AiDreamerOverrides, AiEmotesOverrides,
    AiGrokOverrides, AiHistoryOverrides, AiMediaOverrides, AiMemoryOverrides, AiPrefillOverrides,
    AiRecapOverrides, AiWebOverrides, AlertsOverrides, AviationstackOverrides, CmeAlertsOverrides,
    EmoteTrackerOverrides, PotatAttributionOverrides, QuakeAlertsOverrides, SolarAlertsOverrides,
    SuspendOverrides, TwitchOverrides, WebRuntimeOverrides,
};
use twitch_1337_core::settings::{
    AiOverrides, CooldownsOverrides, FieldError, PingsOverrides, SettingsOverrides,
};

use super::SaveForm;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Maps a submitted optional string to a tri-state override value:
/// - `None`      -> field absent from form (no change)
/// - `Some("")`  -> present but empty -> explicit clear (`Some(None)`)
/// - `Some(v)`   -> set to v (`Some(Some(v))`)
fn tri_state(val: Option<String>) -> Option<Option<String>> {
    match val {
        None => None,
        Some(s) if s.is_empty() => Some(None),
        Some(s) => Some(Some(s)),
    }
}

/// Resolves the `enabled` field for toggle cards that use a `*_card_visible`
/// hidden input to differentiate "card rendered but unchecked" from "card absent".
fn enabled_from_card(visible: &Option<String>, checked: &Option<String>) -> Option<bool> {
    visible.is_some().then(|| checked.is_some())
}

fn parse_id_list(s: &str) -> Vec<String> {
    s.lines()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Trims an optional profile `<select>` value, where empty explicitly disables
/// the feature.
fn opt_str(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Required profile fields must always remain explicit pointers. The save
/// handler rejects blanks before conversion, so this must never encode an
/// inherit/clear operation as `Some(None)`.
fn required_profile(s: &str) -> Option<Option<String>> {
    Some(Some(s.trim().to_owned()))
}

pub(super) fn required_agent_profile_errors(form: &SaveForm) -> Vec<FieldError> {
    [
        ("ai.aurora.profile", form.ai_aurora_profile.as_str()),
        ("ai.grok.profile", form.ai_grok_profile.as_str()),
        ("ai.dreamer.profile", form.ai_dreamer_profile.as_str()),
        ("ai.media.profile", form.ai_media_profile.as_str()),
    ]
    .into_iter()
    .filter(|(_, value)| value.trim().is_empty())
    .map(|(field, _)| FieldError {
        field: field.to_owned(),
        message: "must not be empty".to_owned(),
    })
    .collect()
}

// ---------------------------------------------------------------------------
// Per-card form structs
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct CooldownsForm {
    #[serde(default)]
    cooldown_ai: u64,
    #[serde(default)]
    cooldown_news: u64,
    #[serde(default)]
    cooldown_up: u64,
    #[serde(default)]
    cooldown_feedback: u64,
    #[serde(default)]
    cooldown_doener: u64,
}

impl CooldownsForm {
    fn into_overrides(self) -> CooldownsOverrides {
        CooldownsOverrides {
            ai: Some(self.cooldown_ai),
            news: Some(self.cooldown_news),
            up: Some(self.cooldown_up),
            feedback: Some(self.cooldown_feedback),
            doener: Some(self.cooldown_doener),
        }
    }
}

#[derive(Default, Deserialize)]
struct PingsForm {
    #[serde(default)]
    ping_cooldown: u64,
    /// Unchecked HTML checkboxes don't submit a value at all, so a missing
    /// `ping_public` key means "false". Form fields with `value="1"` send
    /// `Some("1")` when checked.
    #[serde(default)]
    ping_public: Option<String>,
}

impl PingsForm {
    fn into_overrides(self) -> PingsOverrides {
        PingsOverrides {
            cooldown: Some(self.ping_cooldown),
            public: Some(self.ping_public.is_some()),
        }
    }
}

#[derive(Default, Deserialize)]
struct AiAuroraForm {
    #[serde(default)]
    ai_aurora_profile: String,
    #[serde(default)]
    ai_aurora_max_turn_rounds: Option<usize>,
    #[serde(default)]
    ai_aurora_max_writes_per_turn: Option<usize>,
    #[serde(default)]
    ai_aurora_persona_name: Option<String>,
    #[serde(default)]
    ai_aurora_min_response_delay_ms: Option<u64>,
    #[serde(default)]
    ai_aurora_reply_target_chars: Option<usize>,
    #[serde(default)]
    ai_aurora_reply_soft_chars: Option<usize>,
    #[serde(default)]
    ai_aurora_reply_hard_chars: Option<usize>,
}

impl AiAuroraForm {
    fn into_overrides(self) -> AiAuroraOverrides {
        AiAuroraOverrides {
            profile: required_profile(&self.ai_aurora_profile),
            max_turn_rounds: self.ai_aurora_max_turn_rounds,
            max_writes_per_turn: self.ai_aurora_max_writes_per_turn,
            persona_name: self.ai_aurora_persona_name.map(|s| s.trim().to_string()),
            min_response_delay_ms: self.ai_aurora_min_response_delay_ms,
            reply_target_chars: self.ai_aurora_reply_target_chars,
            reply_soft_chars: self.ai_aurora_reply_soft_chars,
            reply_hard_chars: self.ai_aurora_reply_hard_chars,
        }
    }
}

#[derive(Default, Deserialize)]
struct AiHistoryForm {
    #[serde(default)]
    ai_history_length: Option<u64>,
    #[serde(default)]
    ai_history_ai_channel_length: Option<u64>,
    #[serde(default)]
    ai_history_n: Option<u64>,
    #[serde(default)]
    ai_history_ai_channel_n: Option<u64>,
}

impl AiHistoryForm {
    fn into_overrides(self) -> AiHistoryOverrides {
        AiHistoryOverrides {
            length: self.ai_history_length,
            ai_channel_length: self.ai_history_ai_channel_length,
            n: self.ai_history_n,
            ai_channel_n: self.ai_history_ai_channel_n,
            ..Default::default()
        }
    }
}

#[derive(Default, Deserialize)]
struct AiRecapForm {
    #[serde(default)]
    ai_recap_profile: String,
    #[serde(default)]
    ai_recap_batch_size: Option<u64>,
    #[serde(default)]
    ai_recap_max_notes: Option<u64>,
    #[serde(default)]
    ai_recap_merge_width: Option<u64>,
}

impl AiRecapForm {
    fn into_overrides(self) -> AiRecapOverrides {
        AiRecapOverrides {
            profile: Some(opt_str(&self.ai_recap_profile)),
            batch_size: self.ai_recap_batch_size,
            max_notes: self.ai_recap_max_notes,
            merge_width: self.ai_recap_merge_width,
        }
    }
}

#[derive(Default, Deserialize)]
struct AiMemoryForm {
    #[serde(default)]
    ai_memory_soul_bytes: Option<usize>,
    #[serde(default)]
    ai_memory_lore_bytes: Option<usize>,
    #[serde(default)]
    ai_memory_user_bytes: Option<usize>,
    #[serde(default)]
    ai_memory_scratch_bytes: Option<usize>,
    #[serde(default)]
    ai_memory_inject_byte_budget: Option<usize>,
    #[serde(default)]
    ai_memory_max_scratch_files: Option<usize>,
}

impl AiMemoryForm {
    fn into_overrides(self) -> AiMemoryOverrides {
        AiMemoryOverrides {
            soul_bytes: self.ai_memory_soul_bytes,
            lore_bytes: self.ai_memory_lore_bytes,
            user_bytes: self.ai_memory_user_bytes,
            scratch_bytes: self.ai_memory_scratch_bytes,
            inject_byte_budget: self.ai_memory_inject_byte_budget,
            max_scratch_files: self.ai_memory_max_scratch_files,
        }
    }
}

#[derive(Default, Deserialize)]
struct AiDreamerForm {
    #[serde(default)]
    ai_dreamer_profile: String,
    #[serde(default)]
    ai_dreamer_enabled: Option<String>,
    #[serde(default)]
    ai_dreamer_run_at: Option<String>,
    #[serde(default)]
    ai_dreamer_timeout_secs: Option<u64>,
    #[serde(default)]
    ai_dreamer_max_rounds: Option<usize>,
    #[serde(default)]
    ai_dreamer_max_writes_per_turn: Option<usize>,
}

impl AiDreamerForm {
    fn into_overrides(self) -> AiDreamerOverrides {
        AiDreamerOverrides {
            profile: required_profile(&self.ai_dreamer_profile),
            // Dreamer card always renders, so checkbox semantics mirror
            // `ping_public`: missing key = false, present = true.
            enabled: Some(self.ai_dreamer_enabled.is_some()),
            run_at: self.ai_dreamer_run_at,
            timeout_secs: self.ai_dreamer_timeout_secs,
            max_rounds: self.ai_dreamer_max_rounds,
            max_writes_per_turn: self.ai_dreamer_max_writes_per_turn,
        }
    }
}

#[derive(Default, Deserialize)]
struct AiGrokForm {
    #[serde(default)]
    ai_grok_profile: String,
    #[serde(default)]
    ai_grok_persona_name: Option<String>,
}

impl AiGrokForm {
    fn into_overrides(self) -> AiGrokOverrides {
        AiGrokOverrides {
            profile: required_profile(&self.ai_grok_profile),
            persona_name: self.ai_grok_persona_name.map(|s| s.trim().to_string()),
        }
    }
}

#[derive(Default, Deserialize)]
struct AiPrefillForm {
    /// Hidden marker emitted by the rendered card so the handler can tell
    /// "card visible but unchecked" from "card not in form".
    #[serde(default)]
    ai_prefill_card_visible: Option<String>,
    #[serde(default)]
    ai_prefill_enabled: Option<String>,
    #[serde(default)]
    ai_prefill_base_url: Option<String>,
    #[serde(default)]
    ai_prefill_threshold: Option<f64>,
}

impl AiPrefillForm {
    fn into_overrides(self) -> AiPrefillOverrides {
        AiPrefillOverrides {
            enabled: enabled_from_card(&self.ai_prefill_card_visible, &self.ai_prefill_enabled),
            base_url: self.ai_prefill_base_url,
            threshold: self.ai_prefill_threshold,
        }
    }
}

#[derive(Default, Deserialize)]
struct AiWebForm {
    #[serde(default)]
    ai_web_card_visible: Option<String>,
    #[serde(default)]
    ai_web_enabled: Option<String>,
    #[serde(default)]
    ai_web_base_url: Option<String>,
    #[serde(default)]
    ai_web_timeout: Option<u64>,
    #[serde(default)]
    ai_web_max_results: Option<usize>,
    #[serde(default)]
    ai_web_max_rounds: Option<usize>,
    #[serde(default)]
    ai_web_cache_ttl_secs: Option<u64>,
    #[serde(default)]
    ai_web_cache_capacity: Option<usize>,
}

impl AiWebForm {
    fn into_overrides(self) -> AiWebOverrides {
        AiWebOverrides {
            enabled: enabled_from_card(&self.ai_web_card_visible, &self.ai_web_enabled),
            base_url: self.ai_web_base_url,
            timeout: self.ai_web_timeout,
            max_results: self.ai_web_max_results,
            max_rounds: self.ai_web_max_rounds,
            cache_ttl_secs: self.ai_web_cache_ttl_secs,
            cache_capacity: self.ai_web_cache_capacity,
        }
    }
}

#[derive(Default, Deserialize)]
struct AiEmotesForm {
    #[serde(default)]
    ai_emotes_card_visible: Option<String>,
    #[serde(default)]
    ai_emotes_enabled: Option<String>,
    #[serde(default)]
    ai_emotes_include_global: Option<String>,
    #[serde(default)]
    ai_emotes_refresh_interval_secs: Option<u64>,
    #[serde(default)]
    ai_emotes_max_prompt_emotes: Option<usize>,
    #[serde(default)]
    ai_emotes_min_baseline_emotes: Option<usize>,
    #[serde(default)]
    ai_emotes_pinned_emotes: Option<String>,
    #[serde(default)]
    ai_emotes_base_url: Option<String>,
}

impl AiEmotesForm {
    fn into_overrides(self) -> AiEmotesOverrides {
        AiEmotesOverrides {
            enabled: enabled_from_card(&self.ai_emotes_card_visible, &self.ai_emotes_enabled),
            // include_global only meaningful when the card is visible; mirror
            // the prefill/web pattern so the inner checkbox can be unchecked
            // explicitly without disabling the whole card.
            include_global: self
                .ai_emotes_card_visible
                .as_ref()
                .map(|_| self.ai_emotes_include_global.is_some()),
            refresh_interval_secs: self.ai_emotes_refresh_interval_secs,
            max_prompt_emotes: self.ai_emotes_max_prompt_emotes,
            min_baseline_emotes: self.ai_emotes_min_baseline_emotes,
            // Wholesale-replace list, gated on card visibility like the
            // checkboxes: an empty textarea clears the pins rather than
            // falling back to the seed.
            pinned_emotes: self
                .ai_emotes_card_visible
                .as_ref()
                .map(|_| parse_id_list(self.ai_emotes_pinned_emotes.as_deref().unwrap_or(""))),
            base_url: self
                .ai_emotes_base_url
                .map(|v| if v.is_empty() { None } else { Some(v) }),
        }
    }
}

#[derive(Default, Deserialize)]
struct AiMediaForm {
    #[serde(default)]
    ai_media_profile: String,
    #[serde(default)]
    ai_media_timeout: Option<u64>,
    #[serde(default)]
    ai_media_max_image_size: Option<String>,
    #[serde(default)]
    ai_media_max_pdf_size: Option<String>,
    #[serde(default)]
    ai_media_max_audio_size: Option<String>,
    #[serde(default)]
    ai_media_max_video_size: Option<String>,
    #[serde(default)]
    ai_media_max_text_size: Option<String>,
}

impl AiMediaForm {
    fn into_overrides(self) -> AiMediaOverrides {
        AiMediaOverrides {
            profile: required_profile(&self.ai_media_profile),
            timeout: self.ai_media_timeout,
            max_image_size: self
                .ai_media_max_image_size
                .as_deref()
                .and_then(|s| s.parse().ok()),
            max_pdf_size: self
                .ai_media_max_pdf_size
                .as_deref()
                .and_then(|s| s.parse().ok()),
            max_audio_size: self
                .ai_media_max_audio_size
                .as_deref()
                .and_then(|s| s.parse().ok()),
            max_video_size: self
                .ai_media_max_video_size
                .as_deref()
                .and_then(|s| s.parse().ok()),
            max_text_size: self
                .ai_media_max_text_size
                .as_deref()
                .and_then(|s| s.parse().ok()),
        }
    }
}

#[derive(Default, Deserialize)]
struct TwitchPermissionsForm {
    #[serde(default)]
    twitch_hidden_admins: Option<String>,
    #[serde(default)]
    twitch_viewer_allowlist: Option<String>,
    #[serde(default)]
    twitch_emote_usage_deny_list: Option<String>,
}

impl TwitchPermissionsForm {
    fn into_overrides(self) -> TwitchOverrides {
        TwitchOverrides {
            hidden_admins: self.twitch_hidden_admins.as_deref().map(parse_id_list),
            viewer_allowlist: self.twitch_viewer_allowlist.as_deref().map(parse_id_list),
            emote_usage_deny_list: self
                .twitch_emote_usage_deny_list
                .as_deref()
                .map(parse_id_list),
            ..Default::default()
        }
    }
}

#[derive(Default, Deserialize)]
struct TwitchChannelsForm {
    #[serde(default)]
    twitch_expected_latency: Option<u32>,
    #[serde(default)]
    twitch_admin_channel: Option<String>,
    #[serde(default)]
    twitch_ai_channel: Option<String>,
}

impl TwitchChannelsForm {
    fn into_overrides(self) -> TwitchOverrides {
        TwitchOverrides {
            expected_latency: self.twitch_expected_latency,
            admin_channel: tri_state(self.twitch_admin_channel),
            ai_channel: tri_state(self.twitch_ai_channel),
            ..Default::default()
        }
    }
}

#[derive(Default, Deserialize)]
struct AviationstackForm {
    /// Hidden sentinel emitted by the rendered card so the handler can tell
    /// "card visible but checkbox unchecked" from "card absent (don't touch)".
    #[serde(default)]
    aviationstack_card_visible: Option<String>,
    /// Checkbox: `"1"` when checked, absent when unchecked.
    #[serde(default)]
    aviationstack_enabled: Option<String>,
    #[serde(default)]
    aviationstack_base_url: Option<String>,
    #[serde(default)]
    aviationstack_timeout_secs: Option<u64>,
}

impl AviationstackForm {
    fn into_overrides(self) -> AviationstackOverrides {
        AviationstackOverrides {
            enabled: enabled_from_card(
                &self.aviationstack_card_visible,
                &self.aviationstack_enabled,
            ),
            base_url: self.aviationstack_base_url,
            timeout_secs: self.aviationstack_timeout_secs,
        }
    }
}

#[derive(Default, Deserialize)]
struct SuspendForm {
    #[serde(default)]
    suspend_default_duration_secs: Option<u64>,
}

impl SuspendForm {
    fn into_overrides(self) -> SuspendOverrides {
        SuspendOverrides {
            default_duration_secs: self.suspend_default_duration_secs,
        }
    }
}

#[derive(Default, Deserialize)]
struct EmoteTrackerPotatForm {
    #[serde(default)]
    emote_tracker_potat_card_visible: Option<String>,
    #[serde(default)]
    emote_tracker_potat_enabled: Option<String>,
    #[serde(default)]
    emote_tracker_potat_window_secs: Option<u64>,
    #[serde(default)]
    emote_tracker_potat_7tv_user_id: Option<String>,
}

impl EmoteTrackerPotatForm {
    fn into_overrides(self) -> EmoteTrackerOverrides {
        EmoteTrackerOverrides {
            potat_attribution: PotatAttributionOverrides {
                enabled: enabled_from_card(
                    &self.emote_tracker_potat_card_visible,
                    &self.emote_tracker_potat_enabled,
                ),
                window_secs: self.emote_tracker_potat_window_secs,
                potat_7tv_user_id: self.emote_tracker_potat_7tv_user_id,
            },
        }
    }
}

#[derive(Default, Deserialize)]
struct WebRuntimeForm {
    #[serde(default)]
    web_session_ttl_secs: Option<u64>,
    #[serde(default)]
    web_mod_check_refresh_secs: Option<u64>,
}

impl WebRuntimeForm {
    fn into_overrides(self) -> WebRuntimeOverrides {
        WebRuntimeOverrides {
            session_ttl_secs: self.web_session_ttl_secs,
            mod_check_refresh_secs: self.web_mod_check_refresh_secs,
        }
    }
}

#[derive(Default, Deserialize)]
struct AlertsForm {
    #[serde(default)]
    alerts_quake_card_visible: Option<String>,
    #[serde(default)]
    alerts_quake_enabled: Option<String>,
    #[serde(default)]
    alerts_quake_poll_secs: Option<u64>,
    #[serde(default)]
    alerts_quake_min_mag_tenths: Option<u32>,
    #[serde(default)]
    alerts_solar_card_visible: Option<String>,
    #[serde(default)]
    alerts_solar_enabled: Option<String>,
    #[serde(default)]
    alerts_solar_poll_secs: Option<u64>,
    #[serde(default)]
    alerts_solar_min_class: Option<String>,
    #[serde(default)]
    alerts_cme_card_visible: Option<String>,
    #[serde(default)]
    alerts_cme_enabled: Option<String>,
    #[serde(default)]
    alerts_cme_poll_secs: Option<u64>,
    #[serde(default)]
    alerts_max_age_secs: Option<u64>,
}

impl AlertsForm {
    fn into_overrides(self) -> AlertsOverrides {
        AlertsOverrides {
            quake: QuakeAlertsOverrides {
                enabled: enabled_from_card(
                    &self.alerts_quake_card_visible,
                    &self.alerts_quake_enabled,
                ),
                poll_secs: self.alerts_quake_poll_secs,
                min_mag_tenths: self.alerts_quake_min_mag_tenths,
            },
            solar: SolarAlertsOverrides {
                enabled: enabled_from_card(
                    &self.alerts_solar_card_visible,
                    &self.alerts_solar_enabled,
                ),
                poll_secs: self.alerts_solar_poll_secs,
                min_class: self.alerts_solar_min_class,
            },
            cme: CmeAlertsOverrides {
                enabled: enabled_from_card(&self.alerts_cme_card_visible, &self.alerts_cme_enabled),
                poll_secs: self.alerts_cme_poll_secs,
            },
            max_age_secs: self.alerts_max_age_secs,
        }
    }
}

/// Bundle the flat submitted [`SaveForm`] into a single sparse
/// `SettingsOverrides` patch. Schedules are managed via the dedicated
/// `/schedules` CRUD endpoint, so the settings form must not overwrite them.
pub(super) fn overrides_from_save_form(form: SaveForm) -> SettingsOverrides {
    SettingsOverrides {
        schema_version: twitch_1337_core::settings::SCHEMA_VERSION,
        cooldowns: CooldownsForm {
            cooldown_ai: form.cooldown_ai,
            cooldown_news: form.cooldown_news,
            cooldown_up: form.cooldown_up,
            cooldown_feedback: form.cooldown_feedback,
            cooldown_doener: form.cooldown_doener,
        }
        .into_overrides(),
        pings: PingsForm {
            ping_cooldown: form.ping_cooldown,
            ping_public: form.ping_public,
        }
        .into_overrides(),
        ai: AiOverrides {
            // Profiles are managed wholesale on /ai-profiles, not this form.
            profiles: None,
            active_profile: None,
            // MIGRATION(ai-profiles): `connection` is a kept legacy field —
            // the settings form no longer writes to it, so every save is a
            // no-op on that block. Do NOT delete it from `settings::` core;
            // removing it would drop a live deployment's existing override
            // on the next save.
            connection: AiConnectionOverrides::default(),
            // MIGRATION(ai-agents): legacy fields are read-only and stripped
            // from the next persisted schema-7 save.
            behavior: Default::default(),
            aurora: AiAuroraForm {
                ai_aurora_profile: form.ai_aurora_profile,
                ai_aurora_max_turn_rounds: form.ai_aurora_max_turn_rounds,
                ai_aurora_max_writes_per_turn: form.ai_aurora_max_writes_per_turn,
                ai_aurora_persona_name: form.ai_aurora_persona_name,
                ai_aurora_min_response_delay_ms: form.ai_aurora_min_response_delay_ms,
                ai_aurora_reply_target_chars: form.ai_aurora_reply_target_chars,
                ai_aurora_reply_soft_chars: form.ai_aurora_reply_soft_chars,
                ai_aurora_reply_hard_chars: form.ai_aurora_reply_hard_chars,
            }
            .into_overrides(),
            history: AiHistoryForm {
                ai_history_length: form.ai_history_length,
                ai_history_ai_channel_length: form.ai_history_ai_channel_length,
                ai_history_n: form.ai_history_n,
                ai_history_ai_channel_n: form.ai_history_ai_channel_n,
            }
            .into_overrides(),
            recap: AiRecapForm {
                ai_recap_profile: form.ai_recap_profile,
                ai_recap_batch_size: form.ai_recap_batch_size,
                ai_recap_max_notes: form.ai_recap_max_notes,
                ai_recap_merge_width: form.ai_recap_merge_width,
            }
            .into_overrides(),
            memory: AiMemoryForm {
                ai_memory_soul_bytes: form.ai_memory_soul_bytes,
                ai_memory_lore_bytes: form.ai_memory_lore_bytes,
                ai_memory_user_bytes: form.ai_memory_user_bytes,
                ai_memory_scratch_bytes: form.ai_memory_scratch_bytes,
                ai_memory_inject_byte_budget: form.ai_memory_inject_byte_budget,
                ai_memory_max_scratch_files: form.ai_memory_max_scratch_files,
            }
            .into_overrides(),
            dreamer: AiDreamerForm {
                ai_dreamer_profile: form.ai_dreamer_profile,
                ai_dreamer_enabled: form.ai_dreamer_enabled,
                ai_dreamer_run_at: form.ai_dreamer_run_at,
                ai_dreamer_timeout_secs: form.ai_dreamer_timeout_secs,
                ai_dreamer_max_rounds: form.ai_dreamer_max_rounds,
                ai_dreamer_max_writes_per_turn: form.ai_dreamer_max_writes_per_turn,
            }
            .into_overrides(),
            grok: AiGrokForm {
                ai_grok_profile: form.ai_grok_profile,
                ai_grok_persona_name: form.ai_grok_persona_name,
            }
            .into_overrides(),
            prefill: AiPrefillForm {
                ai_prefill_card_visible: form.ai_prefill_card_visible,
                ai_prefill_enabled: form.ai_prefill_enabled,
                ai_prefill_base_url: form.ai_prefill_base_url,
                ai_prefill_threshold: form.ai_prefill_threshold,
            }
            .into_overrides(),
            web: AiWebForm {
                ai_web_card_visible: form.ai_web_card_visible,
                ai_web_enabled: form.ai_web_enabled,
                ai_web_base_url: form.ai_web_base_url,
                ai_web_timeout: form.ai_web_timeout,
                ai_web_max_results: form.ai_web_max_results,
                ai_web_max_rounds: form.ai_web_max_rounds,
                ai_web_cache_ttl_secs: form.ai_web_cache_ttl_secs,
                ai_web_cache_capacity: form.ai_web_cache_capacity,
            }
            .into_overrides(),
            emotes: AiEmotesForm {
                ai_emotes_card_visible: form.ai_emotes_card_visible,
                ai_emotes_enabled: form.ai_emotes_enabled,
                ai_emotes_include_global: form.ai_emotes_include_global,
                ai_emotes_refresh_interval_secs: form.ai_emotes_refresh_interval_secs,
                ai_emotes_max_prompt_emotes: form.ai_emotes_max_prompt_emotes,
                ai_emotes_min_baseline_emotes: form.ai_emotes_min_baseline_emotes,
                ai_emotes_pinned_emotes: form.ai_emotes_pinned_emotes,
                ai_emotes_base_url: form.ai_emotes_base_url,
            }
            .into_overrides(),
            media: AiMediaForm {
                ai_media_profile: form.ai_media_profile,
                ai_media_timeout: form.ai_media_timeout,
                ai_media_max_image_size: form.ai_media_max_image_size,
                ai_media_max_pdf_size: form.ai_media_max_pdf_size,
                ai_media_max_audio_size: form.ai_media_max_audio_size,
                ai_media_max_video_size: form.ai_media_max_video_size,
                ai_media_max_text_size: form.ai_media_max_text_size,
            }
            .into_overrides(),
        },
        twitch: {
            let mut t = TwitchPermissionsForm {
                twitch_hidden_admins: form.twitch_hidden_admins,
                twitch_viewer_allowlist: form.twitch_viewer_allowlist,
                twitch_emote_usage_deny_list: form.twitch_emote_usage_deny_list,
            }
            .into_overrides();
            let c = TwitchChannelsForm {
                twitch_expected_latency: form.twitch_expected_latency,
                twitch_admin_channel: form.twitch_admin_channel,
                twitch_ai_channel: form.twitch_ai_channel,
            }
            .into_overrides();
            t.expected_latency = c.expected_latency;
            t.admin_channel = c.admin_channel;
            t.ai_channel = c.ai_channel;
            t
        },
        aviationstack: AviationstackForm {
            aviationstack_card_visible: form.aviationstack_card_visible,
            aviationstack_enabled: form.aviationstack_enabled,
            aviationstack_base_url: form.aviationstack_base_url,
            aviationstack_timeout_secs: form.aviationstack_timeout_secs,
        }
        .into_overrides(),
        suspend: SuspendForm {
            suspend_default_duration_secs: form.suspend_default_duration_secs,
        }
        .into_overrides(),
        emote_tracker: EmoteTrackerPotatForm {
            emote_tracker_potat_card_visible: form.emote_tracker_potat_card_visible,
            emote_tracker_potat_enabled: form.emote_tracker_potat_enabled,
            emote_tracker_potat_window_secs: form.emote_tracker_potat_window_secs,
            emote_tracker_potat_7tv_user_id: form.emote_tracker_potat_7tv_user_id,
        }
        .into_overrides(),
        alerts: AlertsForm {
            alerts_quake_card_visible: form.alerts_quake_card_visible,
            alerts_quake_enabled: form.alerts_quake_enabled,
            alerts_quake_poll_secs: form.alerts_quake_poll_secs,
            alerts_quake_min_mag_tenths: form.alerts_quake_min_mag_tenths,
            alerts_solar_card_visible: form.alerts_solar_card_visible,
            alerts_solar_enabled: form.alerts_solar_enabled,
            alerts_solar_poll_secs: form.alerts_solar_poll_secs,
            alerts_solar_min_class: form.alerts_solar_min_class,
            alerts_cme_card_visible: form.alerts_cme_card_visible,
            alerts_cme_enabled: form.alerts_cme_enabled,
            alerts_cme_poll_secs: form.alerts_cme_poll_secs,
            alerts_max_age_secs: form.alerts_max_age_secs,
        }
        .into_overrides(),
        web: WebRuntimeForm {
            web_session_ttl_secs: form.web_session_ttl_secs,
            web_mod_check_refresh_secs: form.web_mod_check_refresh_secs,
        }
        .into_overrides(),
        // Schedules are managed via the dedicated /schedules CRUD endpoint;
        // the settings form must not overwrite them.
        schedules: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tri_state_absent_is_none() {
        assert_eq!(tri_state(None), None);
    }

    #[test]
    fn tri_state_empty_clears() {
        assert_eq!(tri_state(Some(String::new())), Some(None));
    }

    #[test]
    fn tri_state_literal_none_is_not_cleared() {
        assert_eq!(
            tri_state(Some("none".to_string())),
            Some(Some("none".to_string()))
        );
    }

    #[test]
    fn tri_state_value_sets() {
        assert_eq!(
            tri_state(Some("gpt-4o".to_string())),
            Some(Some("gpt-4o".to_string()))
        );
    }

    #[test]
    fn opt_str_trims_names_and_rejects_blank_values() {
        assert_eq!(opt_str(""), None);
        assert_eq!(opt_str("   "), None);
        assert_eq!(opt_str("cheap"), Some("cheap".to_string()));
    }

    #[test]
    fn agent_profile_overrides_round_trip() {
        let aurora = AiAuroraForm {
            ai_aurora_profile: "cheap".to_string(),
            ..Default::default()
        }
        .into_overrides();
        assert_eq!(aurora.profile, Some(Some("cheap".to_string())));

        let dream = AiDreamerForm {
            ai_dreamer_profile: "cheap".to_string(),
            ..Default::default()
        }
        .into_overrides();
        assert_eq!(dream.profile, Some(Some("cheap".to_string())));

        let grok = AiGrokForm {
            ai_grok_profile: "cheap".to_string(),
            ai_grok_persona_name: Some(" Grok Prime ".to_string()),
        }
        .into_overrides();
        assert_eq!(grok.profile, Some(Some("cheap".to_string())));
        assert_eq!(grok.persona_name.as_deref(), Some("Grok Prime"));

        let media = AiMediaForm {
            ai_media_profile: "cheap".to_string(),
            ..Default::default()
        }
        .into_overrides();
        assert_eq!(media.profile, Some(Some("cheap".to_string())));

        let recap = AiRecapForm {
            ai_recap_profile: "cheap".to_string(),
            ..Default::default()
        }
        .into_overrides();
        assert_eq!(recap.profile, Some(Some("cheap".to_string())));

        let recap_cleared = AiRecapForm::default().into_overrides();
        assert_eq!(recap_cleared.profile, Some(None));
    }
}
