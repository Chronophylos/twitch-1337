//! Cross-field and semantic validation for a resolved `Settings` snapshot.
//! Constant per-knob ranges live on the fields (`#[knob(min, max)]`) and run
//! first through `Knobs::check_knob_ranges`.

use std::collections::BTreeSet;

use super::knob::Knobs;
use super::provider::Efforts;
use super::spoiler::is_valid_game_name;
use super::{AiProvider, AiSettings, FieldError, ProfileKind, ReplyEmotesSettings, Settings};

/// Bootstrap-side context required for cross-field validation. `channel`
/// is the IRC channel from config.toml, used to enforce that
/// `twitch.admin_channel` differs from it.
#[derive(Debug, Clone)]
pub struct ValidationContext {
    pub channel: String,
}

impl Settings {
    pub fn validate(&self, ctx: &ValidationContext) -> Result<(), Vec<FieldError>> {
        self.validate_with(ctx, true)
    }

    /// Boot-time validation keeps rejecting malformed values but tolerates
    /// agent pointers whose profile row disappeared. Runtime resolution skips
    /// only that agent; every save still uses [`Self::validate`] as the hard
    /// referential-integrity gate.
    pub(crate) fn validate_for_load(&self, ctx: &ValidationContext) -> Result<(), Vec<FieldError>> {
        self.validate_with(ctx, false)
    }

    fn validate_with(
        &self,
        ctx: &ValidationContext,
        validate_agent_profile_targets: bool,
    ) -> Result<(), Vec<FieldError>> {
        let mut errs = Vec::new();
        self.check_knob_ranges("", &mut errs);
        validate_reply_emotes(&self.reply_emotes, &mut errs);
        validate_ai(&self.ai, validate_agent_profile_targets, &mut errs);
        validate_spoiler(self, validate_agent_profile_targets, &mut errs);
        if let Some(v) = self.twitch.admin_channel.as_deref() {
            let t = v.trim();
            if t.is_empty() {
                errs.push(FieldError {
                    field: "twitch.admin_channel".into(),
                    message: "must not be blank when set".into(),
                });
            } else if t == ctx.channel {
                errs.push(FieldError {
                    field: "twitch.admin_channel".into(),
                    message: format!("must differ from twitch.channel ({:?})", ctx.channel),
                });
            }
        }
        for (idx, id) in self.twitch.hidden_admins.iter().enumerate() {
            if id.trim().is_empty() {
                errs.push(FieldError {
                    field: format!("twitch.hidden_admins[{idx}]"),
                    message: "must not be blank".into(),
                });
            }
        }
        for (idx, id) in self.twitch.viewer_allowlist.iter().enumerate() {
            if id.trim().is_empty() {
                errs.push(FieldError {
                    field: format!("twitch.viewer_allowlist[{idx}]"),
                    message: "must not be blank".into(),
                });
            }
        }
        for (idx, login) in self.twitch.emote_usage_deny_list.iter().enumerate() {
            if login.trim().is_empty() {
                errs.push(FieldError {
                    field: format!("twitch.emote_usage_deny_list[{idx}]"),
                    message: "must not be blank".into(),
                });
            }
        }
        let potat = &self.emote_tracker.potat_attribution;
        if potat.enabled && potat.potat_7tv_user_id.trim().is_empty() {
            errs.push(FieldError {
                field: "emote_tracker.potat_attribution.potat_7tv_user_id".into(),
                message: "must not be blank when attribution is enabled".into(),
            });
        }
        if self.aviationstack.enabled {
            if reqwest::Url::parse(&self.aviationstack.base_url).is_err() {
                errs.push(FieldError {
                    field: "aviationstack.base_url".into(),
                    message: format!(
                        "must be a valid URL (got {:?})",
                        self.aviationstack.base_url
                    ),
                });
            }
            if self.aviationstack.timeout_secs == 0 {
                errs.push(FieldError {
                    field: "aviationstack.timeout_secs".into(),
                    message: "must be > 0".into(),
                });
            }
        }
        // Schedules
        let mut seen_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (idx, sc) in self.schedules.iter().enumerate() {
            let prefix = format!("schedules[{idx}]");
            // Per-row validation lives on `Schedule`.
            errs.extend(sc.validate(&prefix));
            // Uniqueness check (cross-row, so it stays here).
            if !sc.name.trim().is_empty() && !seen_names.insert(sc.name.trim()) {
                errs.push(FieldError {
                    field: format!("{prefix}.name"),
                    message: format!("duplicate name {:?}", sc.name.trim()),
                });
            }
        }
        validate_alerts(&self.alerts, &mut errs);
        validate_command_execution(&self.commands.execution, &mut errs);
        validate_observability(&self.observability, &mut errs);
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }
}

fn validate_reply_emotes(settings: &ReplyEmotesSettings, errs: &mut Vec<FieldError>) {
    let pools = [
        ("reply_emotes.invalid", settings.invalid.as_slice()),
        ("reply_emotes.denied", settings.denied.as_slice()),
        ("reply_emotes.cooldown", settings.cooldown.as_slice()),
        ("reply_emotes.timeout", settings.timeout.as_slice()),
        ("reply_emotes.limit", settings.limit.as_slice()),
        ("reply_emotes.failure", settings.failure.as_slice()),
        ("reply_emotes.unavailable", settings.unavailable.as_slice()),
    ];

    for (field, pool) in pools {
        if pool.len() > 20 {
            errs.push(FieldError {
                field: field.into(),
                message: format!("must contain at most 20 entries (got {})", pool.len()),
            });
        }

        let mut seen = BTreeSet::new();
        for emote in pool {
            if emote.is_empty() {
                errs.push(FieldError {
                    field: field.into(),
                    message: "entries must not be empty".into(),
                });
            }
            if emote
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
            {
                errs.push(FieldError {
                    field: field.into(),
                    message:
                        "entries must be single tokens without whitespace or control characters"
                            .into(),
                });
            }
            if emote.chars().count() > 25 {
                errs.push(FieldError {
                    field: field.into(),
                    message: "entries must contain at most 25 characters".into(),
                });
            }
            if !seen.insert(emote) {
                errs.push(FieldError {
                    field: field.into(),
                    message: format!("duplicate entry {emote:?}"),
                });
            }
        }
    }
}

fn validate_ai(ai: &AiSettings, validate_agent_profile_targets: bool, errs: &mut Vec<FieldError>) {
    fn err(errs: &mut Vec<FieldError>, field: &str, msg: String) {
        errs.push(FieldError {
            field: field.into(),
            message: msg,
        });
    }

    if ai.aurora.persona_name.trim().is_empty() {
        err(errs, "ai.aurora.persona_name", "must not be empty".into());
    }
    if ai.grok.persona_name.trim().is_empty() {
        err(errs, "ai.grok.persona_name", "must not be empty".into());
    }
    if ai.aurora.reply_target_chars > ai.aurora.reply_soft_chars {
        err(
            errs,
            "ai.aurora.reply_target_chars",
            format!(
                "must be <= reply_soft_chars ({} > {})",
                ai.aurora.reply_target_chars, ai.aurora.reply_soft_chars
            ),
        );
    }
    if ai.aurora.reply_soft_chars > ai.aurora.reply_hard_chars {
        err(
            errs,
            "ai.aurora.reply_soft_chars",
            format!(
                "must be <= reply_hard_chars ({} > {})",
                ai.aurora.reply_soft_chars, ai.aurora.reply_hard_chars
            ),
        );
    }
    if ai.recap.merge_width < 2 || ai.recap.merge_width > ai.recap.max_notes {
        err(
            errs,
            "ai.recap.merge_width",
            format!(
                "must be 2..=max_notes (got {} vs max_notes {})",
                ai.recap.merge_width, ai.recap.max_notes
            ),
        );
    }
    if ai.history.length > 0 && ai.history.length < ai.history.n.saturating_add(ai.recap.batch_size)
    {
        err(
            errs,
            "ai.history.length",
            format!(
                "must be >= n + batch_size ({} < {} + {})",
                ai.history.length, ai.history.n, ai.recap.batch_size
            ),
        );
    }
    if ai.memory.inject_byte_budget < ai.memory.soul_bytes + ai.memory.lore_bytes {
        err(
            errs,
            "ai.memory.inject_byte_budget",
            "must be >= soul_bytes + lore_bytes".into(),
        );
    }
    for (field, value) in [
        ("ai.dreamer.run_at", ai.dreamer.run_at.as_str()),
        ("ai.emotes_agent.run_at", ai.emotes_agent.run_at.as_str()),
    ] {
        if jiff::civil::Time::strptime("%H:%M", value).is_err() {
            err(errs, field, format!("must be HH:MM (got {value:?})"));
        }
    }
    if ai.profiles.is_empty() {
        err(
            errs,
            "ai.profiles",
            "at least one profile is required".into(),
        );
    }
    // Agents drive an `LlmClient`; a Decision profile has none (ADR-0010).
    let require_chat = |errs: &mut Vec<FieldError>, field: &str, name: &str| {
        if ai
            .by_name(name)
            .is_some_and(|p| p.kind == ProfileKind::Decision)
        {
            err(
                errs,
                field,
                format!("{name:?} is a decision profile; pick a chat profile"),
            );
        }
    };
    for (field, name) in [
        ("ai.aurora.profile", ai.aurora.profile.as_str()),
        ("ai.grok.profile", ai.grok.profile.as_str()),
        ("ai.dreamer.profile", ai.dreamer.profile.as_str()),
        ("ai.emotes_agent.profile", ai.emotes_agent.profile.as_str()),
        ("ai.media.profile", ai.media.profile.as_str()),
    ] {
        if name.trim().is_empty() {
            err(errs, field, "must not be empty".into());
        } else if validate_agent_profile_targets && ai.by_name(name).is_none() {
            err(errs, field, format!("no profile named {name:?}"));
        } else if validate_agent_profile_targets {
            require_chat(errs, field, name);
        }
    }
    if let Some(name) = ai.recap.profile.as_deref() {
        if name.trim().is_empty() {
            err(
                errs,
                "ai.recap.profile",
                "must be non-blank when set".into(),
            );
        } else if validate_agent_profile_targets && ai.by_name(name).is_none() {
            err(
                errs,
                "ai.recap.profile",
                format!("no profile named {name:?}"),
            );
        } else if validate_agent_profile_targets {
            require_chat(errs, "ai.recap.profile", name);
        }
    }

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for (idx, p) in ai.profiles.iter().enumerate() {
        let at = |f: &str| format!("ai.profiles[{idx}].{f}");
        let caps = p.provider.caps();

        if p.name.trim().is_empty() {
            err(errs, &at("name"), "must not be blank".into());
        } else if !seen.insert(p.name.as_str()) {
            err(
                errs,
                &at("name"),
                format!("duplicate profile name {:?}", p.name),
            );
        }
        if p.kind == ProfileKind::Decision {
            if p.provider != AiProvider::OpenRouter {
                err(
                    errs,
                    &at("provider"),
                    "decision profiles require the open_router provider".into(),
                );
            }
            if p.reasoning_effort.is_some() {
                err(
                    errs,
                    &at("reasoning_effort"),
                    "must be unset for a decision profile".into(),
                );
            }
            if p.service_tier.is_some() {
                err(
                    errs,
                    &at("service_tier"),
                    "must be unset for a decision profile".into(),
                );
            }
        }
        if p.timeout == 0 || p.timeout > 600 {
            err(errs, &at("timeout"), "must be between 1 and 600".into());
        }
        match p.base_url.as_deref().map(str::trim) {
            None | Some("") => {
                if caps.default_base_url.is_none() {
                    err(
                        errs,
                        &at("base_url"),
                        format!(
                            "required for provider {:?} (no default)",
                            p.provider.as_str()
                        ),
                    );
                }
            }
            Some(u) => {
                if reqwest::Url::parse(u).is_err() {
                    err(errs, &at("base_url"), format!("not a valid URL: {u:?}"));
                }
            }
        }
        // Missing key names are a runtime resolve failure (`LlmRegistry`),
        // not a settings-file error. Requiring one here would quarantine
        // every pre-named-keys settings.ron (profiles folded from
        // `ai.connection` have `key: None` and a leftover `[ai].api_key`).
        // `/ai-profiles` create/edit still refuse a blank key for every
        // OpenAI-family provider with a model set.
        if let Some(k) = p.key.as_deref()
            && k.trim().is_empty()
        {
            err(errs, &at("key"), "must be non-empty when set".into());
        }
        if let Some(v) = p.reasoning_effort.as_deref() {
            if v.trim().is_empty() {
                err(
                    errs,
                    &at("reasoning_effort"),
                    "must be non-empty when set".into(),
                );
            } else if !caps.allows_effort(v.trim()) {
                err(
                    errs,
                    &at("reasoning_effort"),
                    match caps.efforts {
                        Efforts::Fixed(list) => format!(
                            "{:?} does not accept {v:?} (allowed: {})",
                            p.provider.as_str(),
                            list.join(", ")
                        ),
                        Efforts::None | Efforts::FreeText => {
                            format!("{:?} has no reasoning effort setting", p.provider.as_str())
                        }
                    },
                );
            }
        }
        if let Some(v) = p.service_tier.as_deref() {
            let trimmed = v.trim();
            if !caps.service_tier {
                err(
                    errs,
                    &at("service_tier"),
                    format!("{:?} has no service tier setting", p.provider.as_str()),
                );
            } else if !matches!(trimmed, "flex" | "priority") {
                err(
                    errs,
                    &at("service_tier"),
                    format!("must be one of 'flex' | 'priority' (got {v:?})"),
                );
            }
        }
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
    if let Some(web) = &ai.web
        && reqwest::Url::parse(&web.base_url).is_err()
    {
        err(
            errs,
            "ai.web.base_url",
            format!("must be a valid URL (got {:?})", web.base_url),
        );
    }
    if let Some(em) = &ai.emotes {
        if em.min_baseline_emotes > em.max_prompt_emotes {
            err(
                errs,
                "ai.emotes.min_baseline_emotes",
                "must be <= max_prompt_emotes".into(),
            );
        }
        // Pinned core is injected before scoring/baseline; if it alone exceeds
        // the window cap there is no room left for any scored or baseline emote.
        // (Names absent from the glossary are dropped with a warning at the
        // provider, not rejected here — see SevenTvEmoteProvider.)
        if em.pinned_emotes.len() > em.max_prompt_emotes {
            err(
                errs,
                "ai.emotes.pinned_emotes",
                format!(
                    "must have <= max_prompt_emotes entries (got {}, max {})",
                    em.pinned_emotes.len(),
                    em.max_prompt_emotes
                ),
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

fn validate_spoiler(s: &Settings, validate_profile_target: bool, errs: &mut Vec<FieldError>) {
    let sp = &s.spoiler;
    if !is_valid_game_name(&sp.default_game) {
        errs.push(FieldError {
            field: "spoiler.default_game".into(),
            message: "must be 1-40 chars: letters, digits, spaces, : - '".into(),
        });
    }
    // Empty means the command is off. Like the agent pointers, the target
    // check only gates saves; boot load stays lenient.
    if validate_profile_target && !sp.profile.is_empty() {
        match s.ai.by_name(&sp.profile) {
            None => errs.push(FieldError {
                field: "spoiler.profile".into(),
                message: format!("no profile named {:?}", sp.profile),
            }),
            Some(p) if p.kind != ProfileKind::Decision => errs.push(FieldError {
                field: "spoiler.profile".into(),
                message: format!(
                    "{:?} is a chat profile; pick a decision profile",
                    sp.profile
                ),
            }),
            Some(_) => {}
        }
    }
}

fn validate_alerts(a: &crate::settings::AlertsSettings, errs: &mut Vec<FieldError>) {
    if crate::alerts::class_rank(&a.solar.min_class).is_none() {
        errs.push(FieldError {
            field: "alerts.solar.min_class".into(),
            message: format!(
                "must be a GOES class like A, B, C, M, X or M6, M6.5 (got {:?})",
                a.solar.min_class
            ),
        });
    }
}

/// The OTel filters are parsed by the same function the live reload uses, so
/// a saved string can never fail to apply.
fn validate_observability(o: &crate::settings::ObservabilitySettings, errs: &mut Vec<FieldError>) {
    for (field, directives) in [
        ("observability.otel.logs_filter", &o.otel.logs_filter),
        ("observability.otel.traces_filter", &o.otel.traces_filter),
    ] {
        if let Err(error) = crate::util::observability::parse_otel_filter(directives) {
            errs.push(FieldError {
                field: field.into(),
                message: format!(
                    "{error} (got {directives:?}; expected comma-separated \
                     `target=level` directives like `warn,twitch_1337=debug`)"
                ),
            });
        }
    }
}

/// A per-user or per-command limit above its global counterpart could never
/// be reached, so it is rejected instead of silently capped. The error names
/// the narrower knob, the one the owner most likely just raised.
fn validate_command_execution(
    e: &crate::settings::CommandExecutionSettings,
    errs: &mut Vec<FieldError>,
) {
    for (field, value, global_field, global) in [
        (
            "max_active_per_user",
            e.max_active_per_user,
            "max_active",
            e.max_active,
        ),
        (
            "max_active_per_command",
            e.max_active_per_command,
            "max_active",
            e.max_active,
        ),
        (
            "max_pending_per_user",
            e.max_pending_per_user,
            "max_pending",
            e.max_pending,
        ),
        (
            "max_pending_per_command",
            e.max_pending_per_command,
            "max_pending",
            e.max_pending,
        ),
    ] {
        if value > global {
            errs.push(FieldError {
                field: format!("commands.execution.{field}"),
                message: format!(
                    "must be <= commands.execution.{global_field} ({global}) (got {value})"
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{AiProfile, AiProvider, ProfileKind};

    fn ctx() -> ValidationContext {
        ValidationContext {
            channel: "test".into(),
        }
    }

    fn profile(provider: AiProvider) -> AiProfile {
        AiProfile {
            name: "p".into(),
            key: Some("k".into()),
            provider,
            base_url: Some("https://example.test/v1".into()),
            model: "m".into(),
            timeout: 30,
            reasoning_effort: None,
            service_tier: None,
            ..Default::default()
        }
    }

    #[test]
    fn table_ranges_reject_out_of_bound_presence_fields() {
        let mut s = Settings::compiled_defaults();
        s.ai.web = Some(crate::settings::AiWeb {
            max_rounds: 12,
            ..Default::default()
        });
        let errs = s
            .validate(&ctx())
            .expect_err("max_rounds 12 is out of range");
        assert!(
            errs.contains(&FieldError {
                field: "ai.web.max_rounds".into(),
                message: "must be 1..=6 (got 12)".into(),
            }),
            "{errs:?}"
        );
    }

    #[test]
    fn otel_filters_reject_bad_directives_and_blank() {
        let mut s = Settings::compiled_defaults();
        s.observability.otel.logs_filter = "warn,twitch_1337=loud".into();
        s.observability.otel.traces_filter = "  ".into();
        let errs = s.validate(&ctx()).expect_err("bad filters must fail");
        let fields: Vec<&str> = errs.iter().map(|e| e.field.as_str()).collect();
        assert_eq!(
            fields,
            [
                "observability.otel.logs_filter",
                "observability.otel.traces_filter"
            ],
            "{errs:?}"
        );
        assert!(errs[0].message.contains("twitch_1337=loud"), "{errs:?}");
        assert!(errs[1].message.contains("off"), "{errs:?}");

        s.observability.otel.logs_filter = "off".into();
        s.observability.otel.traces_filter = "info,hyper=debug".into();
        s.validate(&ctx()).expect("well-formed directives pass");
    }

    fn decision_profile() -> AiProfile {
        AiProfile {
            name: "d".into(),
            kind: ProfileKind::Decision,
            ..profile(AiProvider::OpenRouter)
        }
    }

    /// Settings whose six agent pointers all name the profile "p", except the
    /// one picked by `idx`, which names the Decision profile "d". Returns the
    /// settings and the offending field path.
    fn agent_on_decision(idx: usize) -> (Settings, &'static str) {
        let mut s = with_profiles(
            vec![profile(AiProvider::OpenRouter), decision_profile()],
            "p",
        );
        s.ai.recap.profile = Some("p".into());
        let field = match idx {
            0 => {
                s.ai.aurora.profile = "d".into();
                "ai.aurora.profile"
            }
            1 => {
                s.ai.grok.profile = "d".into();
                "ai.grok.profile"
            }
            2 => {
                s.ai.dreamer.profile = "d".into();
                "ai.dreamer.profile"
            }
            3 => {
                s.ai.emotes_agent.profile = "d".into();
                "ai.emotes_agent.profile"
            }
            4 => {
                s.ai.media.profile = "d".into();
                "ai.media.profile"
            }
            _ => {
                s.ai.recap.profile = Some("d".into());
                "ai.recap.profile"
            }
        };
        (s, field)
    }

    #[test]
    fn agent_on_decision_profile_is_rejected() {
        for idx in 0..6 {
            let (s, field) = agent_on_decision(idx);
            let errs = s.validate(&ctx()).expect_err("must fail");
            assert!(
                errs.iter()
                    .any(|e| e.field == field && e.message.contains("chat profile")),
                "{field}: {errs:?}"
            );
        }
    }

    #[test]
    fn load_tolerates_agent_on_decision_profile() {
        for idx in 0..6 {
            let (s, field) = agent_on_decision(idx);
            s.validate_for_load(&ctx())
                .unwrap_or_else(|e| panic!("{field}: {e:?}"));
        }
    }

    #[test]
    fn decision_profile_rejects_non_openrouter_provider() {
        let mut d = decision_profile();
        d.provider = AiProvider::OpenAi;
        let errs = with_profiles(vec![profile(AiProvider::OpenRouter), d], "p")
            .validate(&ctx())
            .expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "ai.profiles[1].provider"));
    }

    #[test]
    fn decision_profile_ignores_effort_and_tier() {
        let mut d = decision_profile();
        d.reasoning_effort = Some("high".into());
        d.service_tier = Some("flex".into());
        let errs = with_profiles(vec![profile(AiProvider::OpenRouter), d], "p")
            .validate(&ctx())
            .expect_err("must fail");
        assert!(
            errs.iter()
                .any(|e| e.field == "ai.profiles[1].reasoning_effort")
        );
        assert!(
            errs.iter()
                .any(|e| e.field == "ai.profiles[1].service_tier")
        );
    }

    fn with_profiles(list: Vec<AiProfile>, active: &str) -> Settings {
        let mut s = Settings::compiled_defaults();
        s.ai.profiles = list;
        s.ai.aurora.profile = active.to_string();
        s.ai.grok.profile = active.to_string();
        s.ai.dreamer.profile = active.to_string();
        s.ai.emotes_agent.profile = active.to_string();
        s.ai.media.profile = active.to_string();
        s
    }

    #[test]
    fn validate_rejects_unknown_aurora_profile() {
        let mut s = Settings::compiled_defaults();
        s.ai.aurora.profile = "ghost".into();
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "ai.aurora.profile"));
    }

    #[test]
    fn validate_allows_empty_recap_profile() {
        let mut s = Settings::compiled_defaults();
        s.ai.recap.profile = None;
        let errs = s.validate(&ctx()).err().unwrap_or_default();
        assert!(!errs.iter().any(|e| e.field.starts_with("ai.recap.profile")));
    }

    #[test]
    fn validate_rejects_empty_grok_persona_name() {
        let mut s = Settings::compiled_defaults();
        s.ai.grok.persona_name = "   ".into();
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "ai.grok.persona_name"));
    }

    #[test]
    fn allows_openai_profile_without_key_so_boot_does_not_quarantine() {
        let mut p = profile(AiProvider::OpenRouter);
        p.key = None;
        with_profiles(vec![p], "p")
            .validate(&ctx())
            .expect("key names are required on /ai-profiles, not on settings.ron load");
    }

    #[test]
    fn rejects_xhigh_on_stock_openai() {
        let mut p = profile(AiProvider::OpenAi);
        p.reasoning_effort = Some("xhigh".into());
        let errs = with_profiles(vec![p], "p").validate(&ctx()).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.field == "ai.profiles[0].reasoning_effort")
        );
    }

    #[test]
    fn accepts_a_vendor_specific_effort_on_a_compatible_endpoint() {
        let mut p = profile(AiProvider::OpenAiCompatible);
        p.reasoning_effort = Some("turbo-brain".into());
        assert!(with_profiles(vec![p], "p").validate(&ctx()).is_ok());
    }

    #[test]
    fn rejects_any_effort_on_ollama() {
        let mut p = profile(AiProvider::Ollama);
        p.reasoning_effort = Some("medium".into());
        let errs = with_profiles(vec![p], "p").validate(&ctx()).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.field == "ai.profiles[0].reasoning_effort")
        );
    }

    #[test]
    fn rejects_service_tier_where_the_provider_has_none() {
        let mut p = profile(AiProvider::OpenAi);
        p.service_tier = Some("flex".into());
        let errs = with_profiles(vec![p], "p").validate(&ctx()).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.field == "ai.profiles[0].service_tier")
        );
    }

    #[test]
    fn rejects_duplicate_and_blank_names() {
        let mut a = profile(AiProvider::OpenAi);
        a.name = "dup".into();
        let mut b = profile(AiProvider::OpenAi);
        b.name = "dup".into();
        let errs = with_profiles(vec![a, b], "dup")
            .validate(&ctx())
            .unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.profiles[1].name"));

        let mut blank = profile(AiProvider::OpenAi);
        blank.name = "   ".into();
        let errs = with_profiles(vec![blank], "   ")
            .validate(&ctx())
            .unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.profiles[0].name"));
    }

    #[test]
    fn rejects_an_empty_profile_list_and_a_dangling_agent_pointer() {
        let errs = with_profiles(vec![], "x").validate(&ctx()).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.profiles"));

        let errs = with_profiles(vec![profile(AiProvider::OpenAi)], "nope")
            .validate(&ctx())
            .unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.aurora.profile"));
    }

    #[test]
    fn requires_a_base_url_where_the_provider_has_no_default() {
        let mut p = profile(AiProvider::OpenAiCompatible);
        p.base_url = None;
        let errs = with_profiles(vec![p], "p").validate(&ctx()).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.profiles[0].base_url"));
    }

    #[test]
    fn rejects_a_subagent_profile_that_does_not_exist() {
        let mut s = with_profiles(vec![profile(AiProvider::OpenAi)], "p");
        s.ai.dreamer.profile = "ghost".into();
        let errs = s.validate(&ctx()).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.dreamer.profile"));
    }

    #[test]
    fn rejects_a_recap_profile_that_does_not_exist() {
        let mut s = with_profiles(vec![profile(AiProvider::OpenAi)], "p");
        s.ai.recap.profile = Some("ghost".into());
        let errs = s.validate(&ctx()).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "ai.recap.profile"));
    }

    #[test]
    fn validate_collects_multiple_errors() {
        let mut s = Settings::compiled_defaults();
        s.cooldowns.ai = 0;
        s.pings.cooldown = 0;
        let errs = s.validate(&ctx()).expect_err("both bounds violated");
        let fields: Vec<&str> = errs.iter().map(|e| e.field.as_str()).collect();
        assert!(fields.contains(&"cooldowns.ai"));
        assert!(fields.contains(&"pings.cooldown"));
    }

    #[test]
    fn validate_accepts_compiled_defaults() {
        Settings::compiled_defaults()
            .validate(&ctx())
            .expect("compiled defaults pass validate()");
    }

    #[test]
    fn validate_rejects_empty_persona_name() {
        let mut s = Settings::compiled_defaults();
        s.ai.aurora.persona_name = "   ".into();
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "ai.aurora.persona_name"));
    }

    #[test]
    fn history_rejects_length_below_n_plus_batch() {
        let mut s = Settings::compiled_defaults();
        s.ai.history.n = 20;
        s.ai.recap.batch_size = 40;
        s.ai.history.length = 50; // 50 < 60
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(
            errs.iter().any(|e| e.field == "ai.history.length"),
            "{errs:?}"
        );
    }

    #[test]
    fn recap_rejects_merge_width_one() {
        let mut s = Settings::compiled_defaults();
        s.ai.recap.merge_width = 1;
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "ai.recap.merge_width"));
    }

    #[test]
    fn recap_rejects_blank_profile() {
        let mut s = Settings::compiled_defaults();
        s.ai.recap.profile = Some("   ".into());
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "ai.recap.profile"));
    }

    #[test]
    fn validate_rejects_max_turn_rounds_out_of_range() {
        let mut s = Settings::compiled_defaults();
        s.ai.aurora.max_turn_rounds = 0;
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "ai.aurora.max_turn_rounds"));
    }

    fn reply_len_err(s: Settings) -> Vec<crate::settings::FieldError> {
        s.validate(&ctx()).expect_err("must fail")
    }

    #[test]
    fn validate_rejects_reply_target_out_of_range() {
        let mut s = Settings::compiled_defaults();
        s.ai.aurora.reply_target_chars = 0;
        let errs = reply_len_err(s);
        assert!(
            errs.iter()
                .any(|e| e.field == "ai.aurora.reply_target_chars")
        );
    }

    #[test]
    fn validate_rejects_reply_hard_above_helix() {
        let mut s = Settings::compiled_defaults();
        s.ai.aurora.reply_hard_chars = 501;
        let errs = reply_len_err(s);
        assert!(errs.iter().any(|e| e.field == "ai.aurora.reply_hard_chars"));
    }

    #[test]
    fn validate_rejects_target_greater_than_soft() {
        let mut s = Settings::compiled_defaults();
        s.ai.aurora.reply_target_chars = 400;
        s.ai.aurora.reply_soft_chars = 300;
        s.ai.aurora.reply_hard_chars = 490;
        let errs = reply_len_err(s);
        assert!(
            errs.iter()
                .any(|e| e.field == "ai.aurora.reply_target_chars")
        );
    }

    #[test]
    fn validate_rejects_soft_greater_than_hard() {
        let mut s = Settings::compiled_defaults();
        s.ai.aurora.reply_target_chars = 200;
        s.ai.aurora.reply_soft_chars = 400;
        s.ai.aurora.reply_hard_chars = 300;
        let errs = reply_len_err(s);
        assert!(errs.iter().any(|e| e.field == "ai.aurora.reply_soft_chars"));
    }

    #[test]
    fn validate_rejects_inject_budget_below_soul_plus_lore() {
        let mut s = Settings::compiled_defaults();
        s.ai.memory.soul_bytes = 4096;
        s.ai.memory.lore_bytes = 12288;
        s.ai.memory.inject_byte_budget = 1024;
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(
            errs.iter()
                .any(|e| e.field == "ai.memory.inject_byte_budget")
        );
    }

    #[test]
    fn validate_rejects_malformed_dreamer_run_at() {
        let mut s = Settings::compiled_defaults();
        s.ai.dreamer.run_at = "not-a-time".into();
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "ai.dreamer.run_at"));
    }

    #[test]
    fn validate_rejects_malformed_emotes_agent_run_at() {
        let mut s = Settings::compiled_defaults();
        s.ai.emotes_agent.run_at = "25:99".into();
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "ai.emotes_agent.run_at"));
    }

    #[test]
    fn validate_rejects_invalid_connection_base_url() {
        let mut s = Settings::compiled_defaults();
        s.ai.profiles[0].base_url = Some("not a url".into());
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "ai.profiles[0].base_url"));
    }

    #[test]
    fn validate_rejects_unknown_service_tier_value() {
        let mut s = Settings::compiled_defaults();
        s.ai.profiles[0].service_tier = Some("turbo".to_string());
        let err = s.validate(&ctx()).expect_err("'turbo' must be rejected");
        assert!(
            err.iter().any(|e| e.field == "ai.profiles[0].service_tier"),
            "expected field error for ai.profiles[0].service_tier, got {err:?}"
        );
    }

    #[test]
    fn validate_rejects_expected_latency_over_1000() {
        let mut s = Settings::compiled_defaults();
        s.twitch.expected_latency = 1500;
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "twitch.expected_latency"));
    }

    #[test]
    fn validate_rejects_admin_channel_equal_to_bootstrap_channel() {
        let mut s = Settings::compiled_defaults();
        s.twitch.admin_channel = Some("test".into());
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "twitch.admin_channel"));
    }

    #[test]
    fn validate_rejects_suspend_out_of_range() {
        let mut s = Settings::compiled_defaults();
        s.suspend.default_duration_secs = 0;
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(
            errs.iter()
                .any(|e| e.field == "suspend.default_duration_secs")
        );
    }

    fn spoiler_errs(s: &Settings) -> Vec<FieldError> {
        s.validate(&ctx()).err().unwrap_or_default()
    }

    #[test]
    fn validate_rejects_spoiler_confidence_floor_over_100() {
        let mut s = Settings::compiled_defaults();
        s.spoiler.confidence_floor = 101;
        assert!(
            spoiler_errs(&s)
                .iter()
                .any(|e| e.field == "spoiler.confidence_floor")
        );
        s.spoiler.confidence_floor = 100;
        assert!(spoiler_errs(&s).is_empty());
        s.spoiler.confidence_floor = 0;
        assert!(spoiler_errs(&s).is_empty());
    }

    #[test]
    fn validate_rejects_bad_spoiler_default_game() {
        let mut s = Settings::compiled_defaults();
        s.spoiler.default_game = "x.".into();
        assert!(
            spoiler_errs(&s)
                .iter()
                .any(|e| e.field == "spoiler.default_game")
        );
    }

    #[test]
    fn validate_rejects_spoiler_and_servers_cooldown_zero() {
        let mut s = Settings::compiled_defaults();
        s.cooldowns.spoiler = 0;
        s.cooldowns.servers = 0;
        let errs = spoiler_errs(&s);
        assert!(errs.iter().any(|e| e.field == "cooldowns.spoiler"));
        assert!(errs.iter().any(|e| e.field == "cooldowns.servers"));
    }

    #[test]
    fn spoiler_profile_must_be_decision_profile() {
        let mut s = with_profiles(
            vec![profile(AiProvider::OpenRouter), decision_profile()],
            "p",
        );
        s.spoiler.profile = "p".into();
        assert!(
            spoiler_errs(&s)
                .iter()
                .any(|e| e.field == "spoiler.profile")
        );
        s.spoiler.profile = "d".into();
        assert!(spoiler_errs(&s).is_empty());
        s.spoiler.profile = String::new();
        assert!(spoiler_errs(&s).is_empty());
        s.spoiler.profile = "ghost".into();
        assert!(
            spoiler_errs(&s)
                .iter()
                .any(|e| e.field == "spoiler.profile")
        );
    }

    #[test]
    fn editing_pointed_decision_profile_to_chat_fails() {
        let mut s = with_profiles(
            vec![profile(AiProvider::OpenRouter), decision_profile()],
            "p",
        );
        s.spoiler.profile = "d".into();
        assert!(s.validate(&ctx()).is_ok());
        let dec = s.ai.profiles.iter_mut().find(|p| p.name == "d").unwrap();
        dec.kind = ProfileKind::Chat;
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "spoiler.profile"));
        // Boot load stays lenient for pointer targets.
        assert!(s.validate_for_load(&ctx()).is_ok());
    }

    #[test]
    fn validate_rejects_potat_window_out_of_range() {
        let mut s = Settings::compiled_defaults();
        s.emote_tracker.potat_attribution.window_secs = 2;
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(
            errs.iter()
                .any(|e| e.field == "emote_tracker.potat_attribution.window_secs")
        );
    }

    #[test]
    fn validate_rejects_blank_potat_7tv_id_when_enabled() {
        let mut s = Settings::compiled_defaults();
        s.emote_tracker.potat_attribution.potat_7tv_user_id = "  ".into();
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(
            errs.iter()
                .any(|e| e.field == "emote_tracker.potat_attribution.potat_7tv_user_id")
        );
    }

    #[test]
    fn validate_rejects_invalid_aviationstack_url() {
        let mut s = Settings::compiled_defaults();
        s.aviationstack.enabled = true;
        s.aviationstack.base_url = "not a url".into();
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "aviationstack.base_url"));
    }

    #[test]
    fn validate_disabled_aviationstack_ignores_invalid_base_url() {
        let mut s = Settings::compiled_defaults();
        s.aviationstack.enabled = false;
        s.aviationstack.base_url = "not a url".into();
        s.aviationstack.timeout_secs = 0;
        // Neither base_url nor timeout_secs should produce errors when disabled.
        s.validate(&ctx())
            .expect("disabled aviationstack must not fail URL/timeout checks");
    }

    #[test]
    fn validate_rejects_blank_admin_channel() {
        let mut s = Settings::compiled_defaults();
        s.twitch.admin_channel = Some("".into());
        let errs = s.validate(&ctx()).expect_err("blank channel must fail");
        assert!(
            errs.iter().any(|e| e.field == "twitch.admin_channel"),
            "expected error for blank admin_channel; got {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_pinned_emotes_exceeding_max() {
        use crate::settings::ai::AiEmotes;
        let mut s = Settings::compiled_defaults();
        s.ai.emotes = Some(AiEmotes {
            max_prompt_emotes: 2,
            min_baseline_emotes: 0,
            pinned_emotes: vec!["A".into(), "B".into(), "C".into()],
            ..AiEmotes::default()
        });
        let errs = s.validate(&ctx()).expect_err("3 pins > max 2 must fail");
        assert!(errs.iter().any(|e| e.field == "ai.emotes.pinned_emotes"));
    }

    #[test]
    fn validate_accepts_pinned_emotes_within_max() {
        use crate::settings::ai::AiEmotes;
        let mut s = Settings::compiled_defaults();
        s.ai.emotes = Some(AiEmotes {
            max_prompt_emotes: 20,
            pinned_emotes: vec!["PepeLa".into(), "okjj".into()],
            ..AiEmotes::default()
        });
        s.validate(&ctx()).expect("2 pins <= max 20 must pass");
    }

    #[test]
    fn validate_rejects_out_of_range_dreamer_write_cap() {
        let mut s = Settings::compiled_defaults();
        s.ai.dreamer.max_writes_per_turn = 0;
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(
            errs.iter()
                .any(|e| e.field == "ai.dreamer.max_writes_per_turn")
        );

        s.ai.dreamer.max_writes_per_turn = 65;
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(
            errs.iter()
                .any(|e| e.field == "ai.dreamer.max_writes_per_turn")
        );
    }

    #[test]
    fn validate_rejects_web_session_ttl_out_of_range() {
        let mut s = Settings::compiled_defaults();
        s.web.session_ttl_secs = 60; // below 1h
        let errs = s.validate(&ctx()).expect_err("must fail");
        assert!(errs.iter().any(|e| e.field == "web.session_ttl_secs"));
    }

    fn schedule(name: &str) -> crate::schedule::Schedule {
        crate::schedule::Schedule {
            ping: None,
            name: name.into(),
            message: "hi".into(),
            trigger: crate::schedule::Trigger::Interval {
                every: std::time::Duration::from_secs(3600),
                days: crate::schedule::WeekdaySet::default(),
                active_from: None,
                active_to: None,
            },
            start_date: None,
            end_date: None,
            enabled: true,
        }
    }

    #[test]
    fn validate_rejects_duplicate_schedule_name() {
        let mut s = Settings::compiled_defaults();
        s.schedules = vec![schedule("a"), schedule("a")];
        let errs = s.validate(&ctx()).expect_err("duplicate name must fail");
        assert!(errs.iter().any(|e| e.field == "schedules[1].name"));
    }

    #[test]
    fn validate_rejects_single_active_time_field() {
        let mut sc = schedule("x");
        sc.trigger = crate::schedule::Trigger::Interval {
            every: std::time::Duration::from_secs(3600),
            days: crate::schedule::WeekdaySet::default(),
            active_from: Some(jiff::civil::time(9, 0, 0, 0)),
            active_to: None,
        };
        let mut s = Settings::compiled_defaults();
        s.schedules = vec![sc];
        let errs = s
            .validate(&ctx())
            .expect_err("orphan active_from must fail");
        assert!(
            errs.iter()
                .any(|e| e.field == "schedules[0].trigger.active_to")
        );
    }

    #[test]
    fn validate_accepts_disabled_schedule() {
        let mut s = Settings::compiled_defaults();
        s.schedules = vec![crate::schedule::Schedule {
            enabled: false,
            ..schedule("x")
        }];
        s.validate(&ctx())
            .expect("disabled schedule with valid required fields must pass");
    }

    #[test]
    fn validate_rejects_end_date_before_start_date() {
        let mut s = Settings::compiled_defaults();
        s.schedules = vec![crate::schedule::Schedule {
            start_date: Some(jiff::civil::date(2027, 1, 1)),
            end_date: Some(jiff::civil::date(2026, 1, 1)),
            ..schedule("x")
        }];
        let errs = s
            .validate(&ctx())
            .expect_err("inverted date range must fail");
        assert!(errs.iter().any(|e| e.field == "schedules[0].end_date"));
    }

    /// Schedule names end up in dashboard URL paths and form values.
    #[test]
    fn validate_rejects_unsafe_schedule_names() {
        for name in ["morning/evening", "foo&bar", "foo bar", "foo\nbar"] {
            let mut s = Settings::compiled_defaults();
            s.schedules = vec![schedule(name)];
            let errs = s.validate(&ctx()).expect_err(name);
            assert!(
                errs.iter().any(|e| e.field == "schedules[0].name"),
                "{name:?}: {errs:?}"
            );
        }
    }

    #[test]
    fn emote_usage_deny_list_rejects_blank_entries() {
        let mut s = Settings::compiled_defaults();
        s.twitch.emote_usage_deny_list = vec!["ok".into(), "  ".into()];
        let errs = s.validate(&ctx()).expect_err("blank login must fail");
        assert!(
            errs.iter()
                .any(|e| e.field.contains("emote_usage_deny_list"))
        );
    }

    #[test]
    fn alerts_rejects_bad_class_and_out_of_range() {
        let mut s = Settings::compiled_defaults();
        s.alerts.solar.min_class = "Q".into();
        s.alerts.quake.min_mag_tenths = 500;
        s.alerts.quake.poll_secs = 5;
        s.alerts.cme.poll_secs = 5;
        let errs = s.validate(&ctx()).expect_err("must fail");
        let fields: Vec<&str> = errs.iter().map(|e| e.field.as_str()).collect();
        assert!(fields.contains(&"alerts.solar.min_class"));
        assert!(fields.contains(&"alerts.quake.min_mag_tenths"));
        assert!(fields.contains(&"alerts.quake.poll_secs"));
        assert!(fields.contains(&"alerts.cme.poll_secs"));
    }

    #[test]
    fn command_execution_defaults_validate() {
        let s = Settings::compiled_defaults();
        assert!(s.validate(&ctx()).is_ok());
        let e = &s.commands.execution;
        assert_eq!(
            (
                e.max_active,
                e.max_pending,
                e.max_active_per_user,
                e.max_pending_per_user,
                e.max_active_per_command,
                e.max_pending_per_command,
            ),
            (32, 128, 3, 8, 8, 8)
        );
    }

    type CommandExecutionCase = (
        &'static str,
        fn(&mut crate::settings::CommandExecutionSettings),
    );

    #[test]
    fn command_execution_ranges_reject_out_of_bound_values() {
        // Each knob just outside its constant range. The check looks for the
        // range message (`lo..=hi`) on that knob's dotted path, independent
        // of any cross-field error the same value also triggers.
        let cases: [CommandExecutionCase; 9] = [
            ("max_active", |e| e.max_active = 0),
            ("max_active", |e| e.max_active = 129),
            ("max_pending", |e| e.max_pending = 1025),
            ("max_active_per_user", |e| e.max_active_per_user = 0),
            ("max_active_per_user", |e| e.max_active_per_user = 33),
            ("max_pending_per_user", |e| e.max_pending_per_user = 65),
            ("max_active_per_command", |e| e.max_active_per_command = 0),
            ("max_active_per_command", |e| {
                e.max_active = 128;
                e.max_active_per_command = 65;
            }),
            ("max_pending_per_command", |e| {
                e.max_pending_per_command = 65;
            }),
        ];
        for (knob, mutate) in cases {
            let mut s = Settings::compiled_defaults();
            mutate(&mut s.commands.execution);
            let errs = s.validate(&ctx()).expect_err(knob);
            let field = format!("commands.execution.{knob}");
            assert!(
                errs.iter()
                    .any(|e| e.field == field && e.message.contains("..=")),
                "{knob}: {errs:?}"
            );
        }
    }

    #[test]
    fn command_execution_zero_pending_is_valid() {
        let mut s = Settings::compiled_defaults();
        let e = &mut s.commands.execution;
        e.max_pending = 0;
        e.max_pending_per_user = 0;
        e.max_pending_per_command = 0;
        assert!(s.validate(&ctx()).is_ok());
    }

    #[test]
    fn command_execution_scoped_limits_cannot_exceed_global() {
        let cases: [CommandExecutionCase; 4] = [
            ("max_active_per_user", |e| {
                e.max_active = 4;
                e.max_active_per_user = 5;
                e.max_active_per_command = 4;
            }),
            ("max_active_per_command", |e| {
                e.max_active = 4;
                e.max_active_per_user = 4;
                e.max_active_per_command = 5;
            }),
            ("max_pending_per_user", |e| {
                e.max_pending = 4;
                e.max_pending_per_user = 5;
                e.max_pending_per_command = 4;
            }),
            ("max_pending_per_command", |e| {
                e.max_pending = 4;
                e.max_pending_per_user = 4;
                e.max_pending_per_command = 5;
            }),
        ];
        for (knob, mutate) in cases {
            let mut s = Settings::compiled_defaults();
            mutate(&mut s.commands.execution);
            let errs = s.validate(&ctx()).expect_err(knob);
            assert_eq!(
                errs,
                [FieldError {
                    field: format!("commands.execution.{knob}"),
                    message: errs[0].message.clone(),
                }],
                "{knob}: only the scoped knob is flagged"
            );
            assert!(
                errs[0].message.contains("<= commands.execution.max_"),
                "{knob}: {errs:?}"
            );
        }

        // Equal to the global is allowed.
        let mut s = Settings::compiled_defaults();
        let e = &mut s.commands.execution;
        e.max_active = 3;
        e.max_active_per_user = 3;
        e.max_active_per_command = 3;
        e.max_pending = 8;
        assert!(s.validate(&ctx()).is_ok());
    }

    #[test]
    fn reply_emote_pools_reject_invalid_entries() {
        let cases = [
            vec![String::new()],
            vec!["two words".into()],
            vec!["bad\u{1}token".into()],
            vec!["x".repeat(26)],
            vec!["Same".into(), "Same".into()],
            (0..21).map(|n| format!("Emote{n}")).collect(),
        ];

        for pool in cases {
            let mut s = Settings::compiled_defaults();
            s.reply_emotes.invalid = pool;
            let errs = s.validate(&ctx()).expect_err("invalid pool must fail");
            assert!(
                errs.iter().any(|e| e.field == "reply_emotes.invalid"),
                "{errs:?}"
            );
        }
    }

    #[test]
    fn reply_emote_pools_accept_empty_and_twenty_unicode_entries() {
        let mut s = Settings::compiled_defaults();
        s.reply_emotes.invalid.clear();
        s.reply_emotes.denied = (0..20).map(|n| format!("Emöte{n}")).collect();

        s.validate(&ctx()).expect("valid pools must pass");
    }
}
