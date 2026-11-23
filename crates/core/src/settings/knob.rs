//! Runtime half of `#[derive(Knobs)]`: the one table every settings knob is
//! declared in.
//!
//! The derive (crate `twitch-1337-settings-derive`) reads `#[knob(...)]`
//! attributes on the effective settings structs and generates the sparse
//! `*Overrides` mirror plus a [`Knobs`] impl. This module holds the traits,
//! the form-value parsers those impls call, and the flattened [`KnobMeta`]
//! table the dashboard reads for row names, bounds, and restart badges.
//!
//! The dotted settings path (`ai.web.max_rounds`) is the single identifier:
//! HTML `name`/`id`, `FieldError::field`, and the table key are all the same
//! string. A struct with bool fields renders a hidden `<path>.__shown`
//! sentinel so an unchecked box (absent from the POST) reads as `false`
//! while a card that was not rendered at all reads as "no change".

use std::collections::BTreeMap;
use std::fmt::Display;
use std::sync::LazyLock;

use super::{FieldError, Settings, SettingsOverrides};
/// The derive shares the trait's name (separate namespaces), so one
/// `use super::knob::Knobs` brings in both.
pub use twitch_1337_settings_derive::Knobs;

/// Implemented by `#[derive(Knobs)]` for every effective settings struct.
pub trait Knobs: Sized {
    /// Generated sparse mirror: every knob is `Option`, `None` = default.
    type Overrides: Default + Clone;

    /// Effective value: each override that is set wins over `defaults`.
    fn resolve_knobs(defaults: &Self, overrides: &Self::Overrides) -> Self;

    /// Fold `patch` into `into`; a set field in `patch` replaces the stored
    /// one, an unset field leaves it alone.
    fn merge_knobs(into: &mut Self::Overrides, patch: &Self::Overrides);

    /// Parse the dotted-path form fields under `prefix` into a patch.
    /// Unparseable values push a `FieldError` and stay unset.
    fn knobs_from_form(
        prefix: &str,
        form: &FormInput<'_>,
        errs: &mut Vec<FieldError>,
    ) -> Self::Overrides;

    /// Push a `FieldError` for every knob outside its `min`/`max`.
    fn check_knob_ranges(&self, prefix: &str, errs: &mut Vec<FieldError>);

    /// Append one [`KnobMeta`] per knob under `prefix`, in declaration order.
    fn visit_knobs(prefix: &str, out: &mut Vec<KnobMeta>);
}

/// A settings struct that lives behind an `Option` in its parent: `None`
/// means the feature is off. Derived by `#[knobs(presence)]`.
pub trait PresenceKnobs: Knobs + Default + Clone {
    fn resolve_presence(defaults: Option<&Self>, overrides: &Self::Overrides) -> Option<Self>;
}

/// Parses one submitted form value. `Err` becomes "invalid value".
pub trait FormValue: Sized {
    #[expect(
        clippy::result_unit_err,
        reason = "the caller maps every failure to the same field error"
    )]
    fn parse_form(raw: &str) -> Result<Self, ()>;
}

macro_rules! form_value_from_str {
    ($($t:ty),*) => {$(
        impl FormValue for $t {
            fn parse_form(raw: &str) -> Result<Self, ()> {
                raw.trim().parse().map_err(drop)
            }
        }
    )*};
}

form_value_from_str!(u8, u32, u64, usize);

impl FormValue for f64 {
    fn parse_form(raw: &str) -> Result<Self, ()> {
        raw.trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or(())
    }
}

impl FormValue for String {
    fn parse_form(raw: &str) -> Result<Self, ()> {
        Ok(raw.trim().to_owned())
    }
}

impl FormValue for Option<String> {
    fn parse_form(raw: &str) -> Result<Self, ()> {
        let v = raw.trim();
        Ok((!v.is_empty()).then(|| v.to_owned()))
    }
}

/// One entry per line; blank lines dropped.
impl FormValue for Vec<String> {
    fn parse_form(raw: &str) -> Result<Self, ()> {
        Ok(raw
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect())
    }
}

impl FormValue for bytesize::ByteSize {
    fn parse_form(raw: &str) -> Result<Self, ()> {
        raw.trim().parse().map_err(drop)
    }
}

/// Submitted form pairs keyed by name. A repeated key is a client bug (two
/// inputs sharing a name), reported as "duplicate field"; the first wins.
pub struct FormInput<'a> {
    map: BTreeMap<&'a str, &'a str>,
}

impl<'a> FormInput<'a> {
    pub fn new(pairs: &'a [(String, String)], errs: &mut Vec<FieldError>) -> Self {
        let mut map = BTreeMap::new();
        for (k, v) in pairs {
            if map.contains_key(k.as_str()) {
                errs.push(FieldError {
                    field: k.clone(),
                    message: "duplicate field".into(),
                });
            } else {
                map.insert(k.as_str(), v.as_str());
            }
        }
        Self { map }
    }

    pub fn get(&self, key: &str) -> Option<&'a str> {
        self.map.get(key).copied()
    }

    pub fn has(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    fn keys(&self) -> impl Iterator<Item = &'a str> + '_ {
        self.map.keys().copied()
    }
}

/// One knob in the flattened table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnobMeta {
    /// Dotted settings path; also the form field name.
    pub path: String,
    /// Posted by the settings form.
    pub form: bool,
    /// A field: any change needs a restart. A presence toggle
    /// (`presence == true`): enabling or disabling needs a restart.
    pub restart: bool,
    /// The `<section>.enabled` toggle of an `Option` section.
    pub presence: bool,
    pub min: Option<String>,
    pub max: Option<String>,
}

impl KnobMeta {
    /// Row label: the last path segment.
    pub fn label(&self) -> &str {
        self.path.rsplit('.').next().unwrap_or(&self.path)
    }

    /// Whether the row shows the "restart required" badge.
    pub fn badge(&self) -> bool {
        self.restart
    }
}

/// Every knob, in declaration order.
pub fn knobs() -> &'static [KnobMeta] {
    static TABLE: LazyLock<Vec<KnobMeta>> = LazyLock::new(|| {
        let mut v = Vec::new();
        Settings::visit_knobs("", &mut v);
        v
    });
    &TABLE
}

/// Look up one knob by dotted path. An unknown path logs and returns an
/// empty entry instead of panicking: a template typo must not take the
/// dashboard down, and the route drift test catches it.
pub fn knob(path: &str) -> &'static KnobMeta {
    static UNKNOWN: LazyLock<KnobMeta> = LazyLock::new(|| KnobMeta {
        path: String::new(),
        form: false,
        restart: false,
        presence: false,
        min: None,
        max: None,
    });
    knobs().iter().find(|k| k.path == path).unwrap_or_else(|| {
        tracing::error!(%path, "unknown settings knob path");
        &UNKNOWN
    })
}

/// Parse a dashboard POST into a settings patch. A missing key leaves the
/// knob unchanged; an unknown, duplicate, or unparseable key is an error.
pub fn overrides_from_form(pairs: &[(String, String)]) -> (SettingsOverrides, Vec<FieldError>) {
    let mut errs = Vec::new();
    let form = FormInput::new(pairs, &mut errs);
    let patch = Settings::knobs_from_form("", &form, &mut errs);
    for key in form.keys() {
        let known = key == "_csrf"
            || key.ends_with(".__shown")
            || knobs().iter().any(|k| k.form && k.path == key);
        if !known {
            errs.push(FieldError {
                field: key.to_owned(),
                message: "unknown field".into(),
            });
        }
    }
    (patch, errs)
}

/// Knobs flagged `restart` whose resolved value changed between `before`
/// and `after`, in table order. A presence section reports when it toggles
/// in either direction; its inner fields report only while it stays on.
pub fn restart_required(before: &Settings, after: &Settings) -> Vec<String> {
    let (b, a) = match (serde_json::to_value(before), serde_json::to_value(after)) {
        (Ok(b), Ok(a)) => (b, a),
        (Err(error), _) | (_, Err(error)) => {
            tracing::error!(?error, "settings snapshot did not serialize to JSON");
            return Vec::new();
        }
    };
    let null = serde_json::Value::Null;
    let at = |v: &'_ serde_json::Value, ptr: &str| -> serde_json::Value {
        v.pointer(ptr).cloned().unwrap_or(serde_json::Value::Null)
    };
    let mut out = Vec::new();
    for k in knobs().iter().filter(|k| k.restart) {
        if k.presence {
            let section = k.path.strip_suffix(".enabled").unwrap_or(&k.path);
            let ptr = pointer(section);
            match (at(&b, &ptr) == null, at(&a, &ptr) == null) {
                (true, false) => out.push(format!("{section} (enabling requires restart)")),
                (false, true) => out.push(format!("{section} (disabling requires restart)")),
                _ => {}
            }
        } else {
            let ptr = pointer(&k.path);
            let parent = ptr.rsplit_once('/').map_or("", |(p, _)| p);
            if !parent.is_empty() && (at(&b, parent) == null || at(&a, parent) == null) {
                continue;
            }
            if at(&b, &ptr) != at(&a, &ptr) {
                out.push(k.path.clone());
            }
        }
    }
    out
}

fn pointer(path: &str) -> String {
    format!("/{}", path.replace('.', "/"))
}

// ---------------------------------------------------------------------------
// Helpers called by generated code.
// ---------------------------------------------------------------------------

/// `prefix.name`, or `name` at the root.
pub fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

pub fn parse_field<T: FormValue>(
    form: &FormInput<'_>,
    path: &str,
    errs: &mut Vec<FieldError>,
) -> Option<T> {
    let raw = form.get(path)?;
    match T::parse_form(raw) {
        Ok(v) => Some(v),
        Err(()) => {
            errs.push(FieldError {
                field: path.to_owned(),
                message: "invalid value".into(),
            });
            None
        }
    }
}

/// A checkbox: only meaningful when its struct's `__shown` sentinel was
/// posted, then absent means `false`.
pub fn parse_bool(form: &FormInput<'_>, prefix: &str, path: &str) -> Option<bool> {
    form.has(&join(prefix, "__shown")).then(|| form.has(path))
}

pub fn parse_required_profile(
    form: &FormInput<'_>,
    path: &str,
    errs: &mut Vec<FieldError>,
) -> Option<Option<String>> {
    let v = form.get(path)?.trim();
    if v.is_empty() {
        errs.push(FieldError {
            field: path.to_owned(),
            message: "must not be empty".into(),
        });
        return None;
    }
    Some(Some(v.to_owned()))
}

pub fn check_range<T: PartialOrd + Display + Copy>(
    prefix: &str,
    name: &str,
    v: T,
    min: Option<T>,
    max: Option<T>,
    errs: &mut Vec<FieldError>,
) {
    let message = match (min, max) {
        (Some(lo), Some(hi)) if v < lo || v > hi => format!("must be {lo}..={hi} (got {v})"),
        (Some(lo), None) if v < lo => format!("must be >= {lo} (got {v})"),
        (None, Some(hi)) if v > hi => format!("must be <= {hi} (got {v})"),
        _ => return,
    };
    errs.push(FieldError {
        field: join(prefix, name),
        message,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{AiEmotes, AiWeb};

    fn form(pairs: &[(&str, &str)]) -> (SettingsOverrides, Vec<FieldError>) {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        overrides_from_form(&owned)
    }

    fn err(field: &str, message: &str) -> FieldError {
        FieldError {
            field: field.into(),
            message: message.into(),
        }
    }

    #[test]
    fn merge_replaces_set_fields_and_keeps_unset_ones() {
        let mut into = SettingsOverrides::default();
        into.twitch.admin_channel = Some(Some("a".into()));
        into.cooldowns.ai = Some(5);
        let mut patch = SettingsOverrides::default();
        patch.twitch.admin_channel = Some(None);
        Settings::merge_knobs(&mut into, &patch);
        assert_eq!(into.twitch.admin_channel, Some(None));
        assert_eq!(into.cooldowns.ai, Some(5));
    }

    #[test]
    fn empty_overrides_resolve_to_defaults() {
        let d = Settings::compiled_defaults();
        assert_eq!(
            Settings::resolve_knobs(&d, &SettingsOverrides::default()),
            d
        );
    }

    #[test]
    fn presence_section_turns_on_with_overrides_and_off_again() {
        let d = Settings::compiled_defaults();
        assert!(d.ai.emotes.is_none());
        let mut o = SettingsOverrides::default();
        o.ai.emotes.enabled = Some(true);
        o.ai.emotes.max_prompt_emotes = Some(7);
        let r = Settings::resolve_knobs(&d, &o);
        assert_eq!(
            r.ai.emotes,
            Some(AiEmotes {
                max_prompt_emotes: 7,
                ..AiEmotes::default()
            })
        );
        o.ai.emotes.enabled = Some(false);
        assert!(Settings::resolve_knobs(&d, &o).ai.emotes.is_none());
    }

    #[test]
    fn blank_required_profile_falls_back_to_default() {
        let d = Settings::compiled_defaults();
        let mut o = SettingsOverrides::default();
        o.ai.aurora.profile = Some(Some("  ".into()));
        assert_eq!(Settings::resolve_knobs(&d, &o).ai.aurora.profile, "default");
    }

    #[test]
    fn form_parses_dotted_paths() {
        let (o, errs) = form(&[("_csrf", "x"), ("cooldowns.ai", "45")]);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(o.cooldowns.ai, Some(45));
    }

    #[test]
    fn form_rejects_bad_unknown_and_duplicate_keys() {
        assert_eq!(
            form(&[("cooldowns.ai", "")]).1,
            vec![err("cooldowns.ai", "invalid value")]
        );
        assert_eq!(
            form(&[("cooldown_ai", "45")]).1,
            vec![err("cooldown_ai", "unknown field")]
        );
        assert_eq!(
            form(&[("schedules", "x")]).1,
            vec![err("schedules", "unknown field")]
        );
        let (o, errs) = form(&[("cooldowns.ai", "1"), ("cooldowns.ai", "2")]);
        assert_eq!(errs, vec![err("cooldowns.ai", "duplicate field")]);
        assert_eq!(o.cooldowns.ai, Some(1));
        assert_eq!(
            form(&[("ai.media.max_image_size", "abc")]).1,
            vec![err("ai.media.max_image_size", "invalid value")]
        );
        assert_eq!(
            form(&[("ai.aurora.profile", " ")]).1,
            vec![err("ai.aurora.profile", "must not be empty")]
        );
    }

    #[test]
    fn checkbox_needs_its_shown_sentinel() {
        assert_eq!(form(&[("pings.__shown", "1")]).0.pings.public, Some(false));
        assert_eq!(
            form(&[("pings.__shown", "1"), ("pings.public", "1")])
                .0
                .pings
                .public,
            Some(true)
        );
        assert_eq!(form(&[]).0.pings.public, None);
        let (o, errs) = form(&[("ai.emotes.__shown", "1"), ("ai.emotes.enabled", "1")]);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(o.ai.emotes.enabled, Some(true));
    }

    #[test]
    fn empty_inputs_clear_lists_and_optional_strings() {
        let (o, errs) = form(&[
            ("twitch.hidden_admins", ""),
            ("twitch.admin_channel", ""),
            ("ai.recap.profile", ""),
        ]);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(o.twitch.hidden_admins, Some(vec![]));
        assert_eq!(o.twitch.admin_channel, Some(None));
        assert_eq!(o.ai.recap.profile, Some(None));
    }

    #[test]
    fn restart_required_reports_enabling_and_field_changes() {
        let before = Settings::compiled_defaults();
        let mut after = before.clone();
        after.ai.emotes = Some(AiEmotes::default());
        assert_eq!(
            restart_required(&before, &after),
            vec!["ai.emotes (enabling requires restart)"]
        );

        let mut after = before.clone();
        after.aviationstack.timeout_secs = before.aviationstack.timeout_secs + 10;
        assert_eq!(
            restart_required(&before, &after),
            vec!["aviationstack.timeout_secs"]
        );

        let mut after = before.clone();
        after.ai.prefill = Some(crate::settings::AiPrefill {
            base_url: "https://elsewhere.example".into(),
            ..Default::default()
        });
        assert_eq!(
            restart_required(&before, &after),
            vec!["ai.prefill (enabling requires restart)"]
        );
    }

    #[test]
    fn restart_required_reports_web_connection_knob_changes() {
        let mut before = Settings::compiled_defaults();
        before.ai.web = Some(AiWeb::default());
        type Edit = fn(&mut Settings);
        let cases: [(&str, Edit); 4] = [
            ("ai.web.base_url", |s| {
                s.ai.web.as_mut().unwrap().base_url = "https://search.example".into();
            }),
            ("ai.web.timeout", |s| {
                s.ai.web.as_mut().unwrap().timeout += 1;
            }),
            ("ai.web.cache_ttl_secs", |s| {
                s.ai.web.as_mut().unwrap().cache_ttl_secs += 1;
            }),
            ("ai.web.cache_capacity", |s| {
                s.ai.web.as_mut().unwrap().cache_capacity += 1;
            }),
        ];

        for (path, edit) in cases {
            let mut after = before.clone();
            edit(&mut after);
            assert_eq!(restart_required(&before, &after), vec![path]);
        }
    }

    #[test]
    fn restart_required_reports_disabling_emotes() {
        let mut before = Settings::compiled_defaults();
        before.ai.emotes = Some(AiEmotes::default());
        let after = Settings::compiled_defaults();

        assert_eq!(
            restart_required(&before, &after),
            vec!["ai.emotes (disabling requires restart)"]
        );
    }

    #[test]
    fn table_paths_are_unique_and_skip_non_form_knobs() {
        let mut seen = std::collections::HashSet::new();
        for k in knobs() {
            assert!(seen.insert(k.path.as_str()), "duplicate {}", k.path);
        }
        let form_paths: Vec<_> = knobs().iter().filter(|k| k.form).map(|k| &k.path).collect();
        for p in ["schedules", "ai.profiles", "schema_version"] {
            assert!(
                !form_paths.iter().any(|f| *f == p),
                "{p} must not be a form knob"
            );
        }
        assert!(form_paths.iter().any(|f| *f == "ai.web.enabled"));
        assert_eq!(knob("cooldowns.ai").label(), "ai");
    }
}
