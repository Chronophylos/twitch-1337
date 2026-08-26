//! Sparse override patch-merge: fold a submitted `SettingsOverrides` patch into
//! the on-disk override tree, field by field. `Some` in the patch wins; `None`
//! leaves the existing value untouched (except list/option fields, which the
//! patch replaces wholesale when present).

use crate::settings::overrides::{AiBehaviorOverrides, SettingsOverrides};

pub(super) fn strip_ai_agents_legacy(into: &mut SettingsOverrides) {
    let legacy_active = into
        .ai
        .active_profile
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string();

    materialize_required_profile(&mut into.ai.aurora.profile, &legacy_active);
    materialize_required_profile(&mut into.ai.grok.profile, &legacy_active);
    materialize_required_profile(&mut into.ai.dreamer.profile, &legacy_active);
    materialize_required_profile(&mut into.ai.media.profile, &legacy_active);

    if into.ai.recap.profile.is_none() {
        into.ai.recap.profile = into.ai.history.recap_profile.clone();
    }
    if into.ai.recap.batch_size.is_none() {
        into.ai.recap.batch_size = into.ai.history.batch_size;
    }
    if into.ai.recap.max_notes.is_none() {
        into.ai.recap.max_notes = into.ai.history.max_notes;
    }
    if into.ai.recap.merge_width.is_none() {
        into.ai.recap.merge_width = into.ai.history.merge_width;
    }

    if into.ai.aurora.max_turn_rounds.is_none() {
        into.ai.aurora.max_turn_rounds = into.ai.behavior.max_turn_rounds;
    }
    if into.ai.aurora.max_writes_per_turn.is_none() {
        into.ai.aurora.max_writes_per_turn = into.ai.behavior.max_writes_per_turn;
    }
    if into.ai.aurora.persona_name.is_none() {
        into.ai.aurora.persona_name = into.ai.behavior.persona_name.clone();
    }
    if into.ai.aurora.min_response_delay_ms.is_none() {
        into.ai.aurora.min_response_delay_ms = into.ai.behavior.min_response_delay_ms;
    }
    if into.ai.aurora.reply_target_chars.is_none() {
        into.ai.aurora.reply_target_chars = into.ai.behavior.reply_target_chars;
    }
    if into.ai.aurora.reply_soft_chars.is_none() {
        into.ai.aurora.reply_soft_chars = into.ai.behavior.reply_soft_chars;
    }
    if into.ai.aurora.reply_hard_chars.is_none() {
        into.ai.aurora.reply_hard_chars = into.ai.behavior.reply_hard_chars;
    }

    into.ai.active_profile = None;
    into.ai.behavior = AiBehaviorOverrides::default();
    into.ai.history.recap_profile = None;
    into.ai.history.batch_size = None;
    into.ai.history.max_notes = None;
    into.ai.history.merge_width = None;
}

fn materialize_required_profile(slot: &mut Option<Option<String>>, legacy_active: &str) {
    if slot.is_none() {
        *slot = Some(Some(legacy_active.to_string()));
    }
}

pub(super) fn merge_into(into: &mut SettingsOverrides, patch: &SettingsOverrides) {
    if let Some(v) = patch.cooldowns.ai {
        into.cooldowns.ai = Some(v);
    }
    if let Some(v) = patch.cooldowns.news {
        into.cooldowns.news = Some(v);
    }
    if let Some(v) = patch.cooldowns.up {
        into.cooldowns.up = Some(v);
    }
    if let Some(v) = patch.cooldowns.feedback {
        into.cooldowns.feedback = Some(v);
    }
    if let Some(v) = patch.cooldowns.doener {
        into.cooldowns.doener = Some(v);
    }
    if let Some(v) = patch.pings.cooldown {
        into.pings.cooldown = Some(v);
    }
    if let Some(v) = patch.pings.public {
        into.pings.public = Some(v);
    }
    // AI profiles (whole-list, like schedules)
    if patch.ai.profiles.is_some() {
        into.ai.profiles = patch.ai.profiles.clone();
    }
    // MIGRATION(ai-profiles): legacy connection block, still merged so an
    // existing settings.ron keeps resolving until it is rewritten.
    if let Some(v) = patch.ai.connection.backend {
        into.ai.connection.backend = Some(v);
    }
    if patch.ai.connection.base_url.is_some() {
        into.ai.connection.base_url = patch.ai.connection.base_url.clone();
    }
    if let Some(v) = patch.ai.connection.model.as_ref() {
        into.ai.connection.model = Some(v.clone());
    }
    if let Some(v) = patch.ai.connection.timeout {
        into.ai.connection.timeout = Some(v);
    }
    if patch.ai.connection.reasoning_effort.is_some() {
        into.ai.connection.reasoning_effort = patch.ai.connection.reasoning_effort.clone();
    }
    if patch.ai.connection.service_tier.is_some() {
        into.ai.connection.service_tier = patch.ai.connection.service_tier.clone();
    }
    // AI aurora
    if patch.ai.aurora.profile.is_some() {
        into.ai.aurora.profile = patch.ai.aurora.profile.clone();
    }
    if let Some(v) = patch.ai.aurora.max_turn_rounds {
        into.ai.aurora.max_turn_rounds = Some(v);
    }
    if let Some(v) = patch.ai.aurora.max_writes_per_turn {
        into.ai.aurora.max_writes_per_turn = Some(v);
    }
    if let Some(v) = &patch.ai.aurora.persona_name {
        into.ai.aurora.persona_name = Some(v.clone());
    }
    if let Some(v) = patch.ai.aurora.min_response_delay_ms {
        into.ai.aurora.min_response_delay_ms = Some(v);
    }
    if let Some(v) = patch.ai.aurora.reply_target_chars {
        into.ai.aurora.reply_target_chars = Some(v);
    }
    if let Some(v) = patch.ai.aurora.reply_soft_chars {
        into.ai.aurora.reply_soft_chars = Some(v);
    }
    if let Some(v) = patch.ai.aurora.reply_hard_chars {
        into.ai.aurora.reply_hard_chars = Some(v);
    }
    // AI history
    if let Some(v) = patch.ai.history.length {
        into.ai.history.length = Some(v);
    }
    if let Some(v) = patch.ai.history.ai_channel_length {
        into.ai.history.ai_channel_length = Some(v);
    }
    if let Some(v) = patch.ai.history.n {
        into.ai.history.n = Some(v);
    }
    if let Some(v) = patch.ai.history.ai_channel_n {
        into.ai.history.ai_channel_n = Some(v);
    }
    // AI recap
    if patch.ai.recap.profile.is_some() {
        into.ai.recap.profile = patch.ai.recap.profile.clone();
    }
    if let Some(v) = patch.ai.recap.batch_size {
        into.ai.recap.batch_size = Some(v);
    }
    if let Some(v) = patch.ai.recap.max_notes {
        into.ai.recap.max_notes = Some(v);
    }
    if let Some(v) = patch.ai.recap.merge_width {
        into.ai.recap.merge_width = Some(v);
    }
    // AI memory
    if let Some(v) = patch.ai.memory.soul_bytes {
        into.ai.memory.soul_bytes = Some(v);
    }
    if let Some(v) = patch.ai.memory.lore_bytes {
        into.ai.memory.lore_bytes = Some(v);
    }
    if let Some(v) = patch.ai.memory.user_bytes {
        into.ai.memory.user_bytes = Some(v);
    }
    if let Some(v) = patch.ai.memory.scratch_bytes {
        into.ai.memory.scratch_bytes = Some(v);
    }
    if let Some(v) = patch.ai.memory.inject_byte_budget {
        into.ai.memory.inject_byte_budget = Some(v);
    }
    if let Some(v) = patch.ai.memory.max_scratch_files {
        into.ai.memory.max_scratch_files = Some(v);
    }
    // AI dreamer
    if let Some(v) = patch.ai.dreamer.enabled {
        into.ai.dreamer.enabled = Some(v);
    }
    if patch.ai.dreamer.profile.is_some() {
        into.ai.dreamer.profile = patch.ai.dreamer.profile.clone();
    }
    if let Some(v) = patch.ai.dreamer.run_at.as_ref() {
        into.ai.dreamer.run_at = Some(v.clone());
    }
    if let Some(v) = patch.ai.dreamer.timeout_secs {
        into.ai.dreamer.timeout_secs = Some(v);
    }
    if let Some(v) = patch.ai.dreamer.max_rounds {
        into.ai.dreamer.max_rounds = Some(v);
    }
    if let Some(v) = patch.ai.dreamer.max_writes_per_turn {
        into.ai.dreamer.max_writes_per_turn = Some(v);
    }
    // AI grok
    if patch.ai.grok.profile.is_some() {
        into.ai.grok.profile = patch.ai.grok.profile.clone();
    }
    if let Some(v) = &patch.ai.grok.persona_name {
        into.ai.grok.persona_name = Some(v.clone());
    }
    // AI prefill
    if let Some(v) = patch.ai.prefill.enabled {
        into.ai.prefill.enabled = Some(v);
    }
    if let Some(v) = patch.ai.prefill.base_url.as_ref() {
        into.ai.prefill.base_url = Some(v.clone());
    }
    if let Some(v) = patch.ai.prefill.threshold {
        into.ai.prefill.threshold = Some(v);
    }
    // AI web
    if let Some(v) = patch.ai.web.enabled {
        into.ai.web.enabled = Some(v);
    }
    if let Some(v) = patch.ai.web.base_url.as_ref() {
        into.ai.web.base_url = Some(v.clone());
    }
    if let Some(v) = patch.ai.web.timeout {
        into.ai.web.timeout = Some(v);
    }
    if let Some(v) = patch.ai.web.max_results {
        into.ai.web.max_results = Some(v);
    }
    if let Some(v) = patch.ai.web.max_rounds {
        into.ai.web.max_rounds = Some(v);
    }
    if let Some(v) = patch.ai.web.cache_ttl_secs {
        into.ai.web.cache_ttl_secs = Some(v);
    }
    if let Some(v) = patch.ai.web.cache_capacity {
        into.ai.web.cache_capacity = Some(v);
    }
    // AI emotes
    if let Some(v) = patch.ai.emotes.enabled {
        into.ai.emotes.enabled = Some(v);
    }
    if let Some(v) = patch.ai.emotes.include_global {
        into.ai.emotes.include_global = Some(v);
    }
    if let Some(v) = patch.ai.emotes.refresh_interval_secs {
        into.ai.emotes.refresh_interval_secs = Some(v);
    }
    if let Some(v) = patch.ai.emotes.max_prompt_emotes {
        into.ai.emotes.max_prompt_emotes = Some(v);
    }
    if let Some(v) = patch.ai.emotes.min_baseline_emotes {
        into.ai.emotes.min_baseline_emotes = Some(v);
    }
    if patch.ai.emotes.pinned_emotes.is_some() {
        into.ai.emotes.pinned_emotes = patch.ai.emotes.pinned_emotes.clone();
    }
    if patch.ai.emotes.base_url.is_some() {
        into.ai.emotes.base_url = patch.ai.emotes.base_url.clone();
    }
    // AI media
    if patch.ai.media.profile.is_some() {
        into.ai.media.profile = patch.ai.media.profile.clone();
    }
    if let Some(v) = patch.ai.media.timeout {
        into.ai.media.timeout = Some(v);
    }
    if let Some(v) = patch.ai.media.max_image_size {
        into.ai.media.max_image_size = Some(v);
    }
    if let Some(v) = patch.ai.media.max_pdf_size {
        into.ai.media.max_pdf_size = Some(v);
    }
    if let Some(v) = patch.ai.media.max_audio_size {
        into.ai.media.max_audio_size = Some(v);
    }
    if let Some(v) = patch.ai.media.max_video_size {
        into.ai.media.max_video_size = Some(v);
    }
    if let Some(v) = patch.ai.media.max_text_size {
        into.ai.media.max_text_size = Some(v);
    }
    // Twitch
    if let Some(v) = patch.twitch.expected_latency {
        into.twitch.expected_latency = Some(v);
    }
    if patch.twitch.hidden_admins.is_some() {
        into.twitch.hidden_admins = patch.twitch.hidden_admins.clone();
    }
    if patch.twitch.viewer_allowlist.is_some() {
        into.twitch.viewer_allowlist = patch.twitch.viewer_allowlist.clone();
    }
    if patch.twitch.emote_usage_deny_list.is_some() {
        into.twitch.emote_usage_deny_list = patch.twitch.emote_usage_deny_list.clone();
    }
    if patch.twitch.admin_channel.is_some() {
        into.twitch.admin_channel = patch.twitch.admin_channel.clone();
    }
    if patch.twitch.ai_channel.is_some() {
        into.twitch.ai_channel = patch.twitch.ai_channel.clone();
    }
    // Aviationstack
    if let Some(v) = patch.aviationstack.enabled {
        into.aviationstack.enabled = Some(v);
    }
    if let Some(v) = patch.aviationstack.base_url.as_ref() {
        into.aviationstack.base_url = Some(v.clone());
    }
    if let Some(v) = patch.aviationstack.timeout_secs {
        into.aviationstack.timeout_secs = Some(v);
    }
    // Suspend
    if let Some(v) = patch.suspend.default_duration_secs {
        into.suspend.default_duration_secs = Some(v);
    }
    // Emote tracker · Potat attribution
    if let Some(v) = patch.emote_tracker.potat_attribution.enabled {
        into.emote_tracker.potat_attribution.enabled = Some(v);
    }
    if let Some(v) = patch.emote_tracker.potat_attribution.window_secs {
        into.emote_tracker.potat_attribution.window_secs = Some(v);
    }
    if let Some(v) = patch
        .emote_tracker
        .potat_attribution
        .potat_7tv_user_id
        .as_ref()
    {
        into.emote_tracker.potat_attribution.potat_7tv_user_id = Some(v.clone());
    }
    // Web
    if let Some(v) = patch.web.session_ttl_secs {
        into.web.session_ttl_secs = Some(v);
    }
    if let Some(v) = patch.web.mod_check_refresh_secs {
        into.web.mod_check_refresh_secs = Some(v);
    }
    // Schedules — wholesale replace
    if patch.schedules.is_some() {
        into.schedules = patch.schedules.clone();
    }

    strip_ai_agents_legacy(into);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{Schedule, Trigger, WeekdaySet};
    use crate::settings::overrides::{
        AviationstackOverrides, SuspendOverrides, TwitchOverrides, WebRuntimeOverrides,
    };

    #[test]
    fn patch_round_trips_profile_on_grok() {
        let mut into = SettingsOverrides::default();
        let mut patch = SettingsOverrides::default();
        patch.ai.grok.profile = Some(Some("cheap".to_string()));
        merge_into(&mut into, &patch);
        assert_eq!(
            into.ai.grok.profile.as_ref().and_then(|v| v.as_deref()),
            Some("cheap")
        );
    }

    #[test]
    fn patch_round_trips_service_tier_on_connection() {
        let mut into = SettingsOverrides::default();
        let mut patch = SettingsOverrides::default();
        patch.ai.connection.service_tier = Some(Some("flex".to_string()));
        merge_into(&mut into, &patch);
        assert_eq!(
            into.ai
                .connection
                .service_tier
                .as_ref()
                .and_then(|v| v.as_deref()),
            Some("flex")
        );
    }

    #[test]
    fn merge_twitch_overrides_replaces_lists() {
        let mut into = SettingsOverrides::default();
        into.twitch.hidden_admins = Some(vec!["old".into()]);
        let patch = SettingsOverrides {
            twitch: TwitchOverrides {
                hidden_admins: Some(vec!["new1".into(), "new2".into()]),
                expected_latency: Some(150),
                ..Default::default()
            },
            ..Default::default()
        };
        merge_into(&mut into, &patch);
        assert_eq!(into.twitch.expected_latency, Some(150));
        assert_eq!(
            into.twitch.hidden_admins.as_deref(),
            Some(&vec!["new1".to_string(), "new2".to_string()][..])
        );
    }

    #[test]
    fn merge_aviationstack_suspend_web() {
        let mut into = SettingsOverrides::default();
        let patch = SettingsOverrides {
            aviationstack: AviationstackOverrides {
                enabled: Some(true),
                ..Default::default()
            },
            suspend: SuspendOverrides {
                default_duration_secs: Some(900),
            },
            web: WebRuntimeOverrides {
                session_ttl_secs: Some(3600),
                mod_check_refresh_secs: Some(60),
            },
            ..Default::default()
        };
        merge_into(&mut into, &patch);
        assert_eq!(into.aviationstack.enabled, Some(true));
        assert_eq!(into.suspend.default_duration_secs, Some(900));
        assert_eq!(into.web.session_ttl_secs, Some(3600));
        assert_eq!(into.web.mod_check_refresh_secs, Some(60));
    }

    fn make_schedule(name: &str, message: &str) -> Schedule {
        Schedule {
            name: name.into(),
            message: message.into(),
            trigger: Trigger::Interval {
                every: std::time::Duration::from_secs(3600),
                days: WeekdaySet::default(),
                active_from: None,
                active_to: None,
            },
            start_date: None,
            end_date: None,
            enabled: true,
        }
    }

    #[test]
    fn merge_replaces_schedules_wholesale() {
        let mut into = SettingsOverrides {
            schedules: Some(vec![make_schedule("old", "m")]),
            ..Default::default()
        };
        let patch = SettingsOverrides {
            schedules: Some(vec![make_schedule("new1", "a"), make_schedule("new2", "b")]),
            ..Default::default()
        };
        merge_into(&mut into, &patch);
        let v = into.schedules.expect("Some");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "new1");
        assert_eq!(v[1].name, "new2");
    }

    #[test]
    fn merge_skips_schedules_when_patch_is_none() {
        let mut into = SettingsOverrides {
            schedules: Some(vec![make_schedule("keep", "m")]),
            ..Default::default()
        };
        let patch = SettingsOverrides::default();
        merge_into(&mut into, &patch);
        let v = into.schedules.expect("Some");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "keep");
    }

    fn make_profile(name: &str, model: &str) -> crate::settings::AiProfile {
        crate::settings::AiProfile {
            name: name.to_string(),
            model: model.to_string(),
            ..Default::default()
        }
    }

    /// The patch list is deliberately *shorter* than the stored one: deleting
    /// a profile is the operation a wrong merge shape would silently undo.
    #[test]
    fn merge_replaces_profiles_wholesale() {
        let mut into = SettingsOverrides::default();
        into.ai.profiles = Some(vec![
            make_profile("old1", "a"),
            make_profile("old2", "b"),
            make_profile("old3", "c"),
        ]);
        let mut patch = SettingsOverrides::default();
        patch.ai.profiles = Some(vec![make_profile("kept", "z")]);

        merge_into(&mut into, &patch);

        let v = into.ai.profiles.expect("Some");
        assert_eq!(
            v.len(),
            1,
            "the two dropped rows must not survive the merge"
        );
        assert_eq!(v[0].name, "kept");
        assert_eq!(v[0].model, "z");
    }

    #[test]
    fn merge_skips_profiles_when_patch_is_none() {
        let mut into = SettingsOverrides::default();
        into.ai.profiles = Some(vec![make_profile("keep", "m")]);
        into.ai.active_profile = Some("keep".to_string());
        let patch = SettingsOverrides::default();

        merge_into(&mut into, &patch);

        let v = into.ai.profiles.expect("Some");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "keep");
        assert!(
            into.ai.active_profile.is_none(),
            "strip clears MIGRATION(ai-agents) active_profile on persist"
        );
    }
}
