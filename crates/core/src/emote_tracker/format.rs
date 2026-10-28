use chrono::{TimeZone, Utc};
use chrono_tz::Europe::Berlin;

use super::types::{SetEventKind, SetEventRow};

/// Display rule for set-change actors: display name → username → 7TV id → `unbekannt`.
pub fn format_actor_display(
    actor_display_name: Option<&str>,
    actor_username: Option<&str>,
    actor_7tv_id: Option<&str>,
) -> String {
    if let Some(name) = actor_display_name.filter(|s| !s.is_empty()) {
        return name.to_owned();
    }
    if let Some(login) = actor_username.filter(|s| !s.is_empty()) {
        return login.to_owned();
    }
    if let Some(id) = actor_7tv_id.filter(|s| !s.is_empty()) {
        return id.to_owned();
    }
    "unbekannt".to_owned()
}

/// Attribution cell / chat clause: `{login} via PotatBotat` when both fields present.
///
/// `via` must be the stored lowercase product id `"potatbotat"`; the display label is
/// always title-case `PotatBotat`.
pub fn format_set_event_attribution(
    requester_login: Option<&str>,
    via: Option<&str>,
) -> Option<String> {
    let login = requester_login.filter(|s| !s.is_empty())?;
    let via = via.filter(|s| !s.is_empty())?;
    if via != "potatbotat" {
        return None;
    }
    Some(format!("{login} via PotatBotat"))
}

/// Past-participle verb for a lifecycle change; `None` for bootstrap/gap rows,
/// which are tracker bookkeeping, not changes.
pub fn lifecycle_verb(kind: SetEventKind) -> Option<&'static str> {
    match kind {
        SetEventKind::Add => Some("hinzugefügt"),
        SetEventKind::Remove => Some("entfernt"),
        SetEventKind::Rename => Some("umbenannt"),
        SetEventKind::Bootstrap | SetEventKind::Gap => None,
    }
}

/// Who made a set change, for `von {..}`: the chat requester when a bot
/// mediated it, else the 7TV editor (falling back to `unbekannt`).
pub fn format_set_event_by(event: &SetEventRow) -> String {
    format_set_event_attribution(event.requester_login.as_deref(), event.via.as_deref())
        .unwrap_or_else(|| {
            format_actor_display(
                event.actor_display_name.as_deref(),
                event.actor_username.as_deref(),
                event.actor_7tv_id.as_deref(),
            )
        })
}

/// Format UTC unix milliseconds as `DD.MM.YYYY HH:MM` in Europe/Berlin.
pub fn format_berlin_millis(at_ms: i64) -> String {
    match Utc.timestamp_millis_opt(at_ms).single() {
        Some(dt) => dt
            .with_timezone(&Berlin)
            .format("%d.%m.%Y %H:%M")
            .to_string(),
        None => "ungültige Zeit".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_actor_display_prefers_display_then_username_then_id() {
        assert_eq!(
            format_actor_display(Some("Cool Mod"), Some("coolmod"), Some("abc")),
            "Cool Mod"
        );
        assert_eq!(
            format_actor_display(None, Some("coolmod"), Some("abc")),
            "coolmod"
        );
        assert_eq!(format_actor_display(None, None, Some("abc")), "abc");
        assert_eq!(format_actor_display(None, None, None), "unbekannt");
        assert_eq!(format_actor_display(Some(""), Some(""), None), "unbekannt");
    }

    #[test]
    fn format_set_event_attribution_requires_login_and_via() {
        assert_eq!(
            format_set_event_attribution(Some("NozzleNols"), Some("potatbotat")).as_deref(),
            Some("NozzleNols via PotatBotat")
        );
        assert_eq!(format_set_event_attribution(Some("x"), None), None);
        assert_eq!(format_set_event_attribution(None, Some("potatbotat")), None);
        assert_eq!(format_set_event_attribution(Some("x"), Some("other")), None);
        assert_eq!(
            format_set_event_attribution(Some(""), Some("potatbotat")),
            None
        );
    }

    #[test]
    fn format_berlin_millis_uses_berlin_offset() {
        // 2025-01-04T15:53:20Z → 16:53 Berlin (CET, UTC+1)
        let s = format_berlin_millis(1_736_006_000_000);
        assert_eq!(s, "04.01.2025 16:53");
    }
}
