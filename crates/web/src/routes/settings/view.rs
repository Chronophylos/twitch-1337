//! The settings page render concern: the `ShowTpl` template struct and the
//! `GET /settings` handler. The save handler reuses `ShowTpl` to re-render the
//! page with field errors, so the struct and its fields are visible to the
//! parent module.

use askama::Template;
use axum::extract::{Extension, State};
use axum::response::Response;
use tower_cookies::Cookies;
use twitch_1337_core::settings::ai::{AiEmotes, AiPrefill, AiWeb};
use twitch_1337_core::settings::{FieldError, KnobMeta, Settings};

use crate::auth::Role;
use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::WebError;
use crate::flash;
use crate::routes::render;
use crate::state::WebState;

#[derive(Template)]
#[template(path = "settings/index.html")]
pub(super) struct ShowTpl {
    pub(super) csrf: String,
    pub(super) flash: Option<String>,
    pub(super) user_login: String,
    pub(super) user_avatar_url: Option<String>,
    pub(super) current_page: &'static str,
    pub(super) is_mod: bool,
    pub(super) is_broadcaster: bool,
    pub(super) is_owner: bool,
    pub(super) current: Settings,
    pub(super) defaults: Settings,
    pub(super) errors: Vec<FieldError>,
    pub(super) recap_parked: bool,
}

/// A sidebar group: its id, label, and the sections it links to.
pub(super) struct NavGroup {
    pub(super) id: &'static str,
    pub(super) label: &'static str,
    pub(super) items: &'static [NavItem],
}

/// One sidebar link. `slug` is the card's `sec-<slug>` anchor, `target` its
/// `data-section`.
pub(super) struct NavItem {
    pub(super) slug: &'static str,
    pub(super) target: &'static str,
    pub(super) label: &'static str,
}

const fn item(slug: &'static str, target: &'static str, label: &'static str) -> NavItem {
    NavItem {
        slug,
        target,
        label,
    }
}

/// Sidebar sections, in page order. Group counts are derived from this.
const NAV: &[NavGroup] = &[
    NavGroup {
        id: "twitch",
        label: "Twitch",
        items: &[
            item("twitch_permissions", "twitch_permissions", "Permissions"),
            item("twitch_channels", "twitch_channels", "Channels"),
        ],
    },
    NavGroup {
        id: "agents",
        label: "Agents",
        items: &[
            item("ai-aurora", "ai_aurora", "Aurora"),
            item("ai-grok", "ai_grok", "Grok"),
            item("ai-recap", "ai_recap", "Recap"),
            item("ai-dreamer", "ai_dreamer", "Dreamer"),
            item("ai-media", "ai_media", "Media"),
        ],
    },
    NavGroup {
        id: "shared",
        label: "Shared",
        items: &[
            item("ai-history", "ai_history", "History"),
            item("ai-prefill", "ai_prefill", "Prefill"),
            item("ai-memory", "ai_memory", "Memory"),
            item("ai-emotes", "ai_emotes", "Emotes"),
            item("ai-web", "ai_web", "Web"),
        ],
    },
    NavGroup {
        id: "commands",
        label: "Commands",
        items: &[
            item("cooldowns", "cooldowns", "Cooldowns"),
            item("commands_execution", "commands_execution", "Execution"),
            item("reply_emotes", "reply_emotes", "Error-Emotes"),
            item("link_correction", "link_correction", "Link corrections"),
            item("bash", "bash", "Bash"),
            item("pings", "pings", "Pings"),
            item("suspend", "suspend", "Suspend"),
            item("spoiler", "spoiler", "Spoiler check"),
        ],
    },
    NavGroup {
        id: "emote_tracker",
        label: "Emote Tracker",
        items: &[item(
            "emote_tracker_potat",
            "emote_tracker_potat",
            "Potat attribution",
        )],
    },
    NavGroup {
        id: "flight_tracking",
        label: "Flight Tracking",
        items: &[item("aviationstack", "aviationstack", "Aviationstack")],
    },
    NavGroup {
        id: "alerts",
        label: "Alerts",
        items: &[
            item("alerts_quake", "alerts_quake", "Earthquakes"),
            item("alerts_solar", "alerts_solar", "Solar flares"),
            item("alerts_cme", "alerts_cme", "CMEs"),
            item("alerts_f1", "alerts_f1", "F1"),
        ],
    },
    NavGroup {
        id: "system",
        label: "System",
        items: &[
            item("web_runtime", "web_runtime", "Dashboard"),
            item("observability", "observability", "Observability"),
        ],
    },
];

/// An optional settings section as its card renders it: the saved value, or
/// the Rust default while the card is off, next to that default.
pub(super) struct Effective<T> {
    pub(super) cur: T,
    pub(super) def: T,
}

fn effective<T: Clone + Default>(saved: Option<&T>) -> Effective<T> {
    Effective {
        cur: saved.cloned().unwrap_or_default(),
        def: T::default(),
    }
}

impl ShowTpl {
    fn nav(&self) -> &'static [NavGroup] {
        NAV
    }

    /// Table entry for a row: its dotted path names the control, its bounds
    /// and restart flag drive `min`/`max` and the badge.
    pub(super) fn knob(&self, path: &str) -> &'static KnobMeta {
        twitch_1337_core::settings::knob(path)
    }

    fn ai_web(&self) -> Effective<AiWeb> {
        effective(self.current.ai.web.as_ref())
    }

    fn ai_emotes(&self) -> Effective<AiEmotes> {
        effective(self.current.ai.emotes.as_ref())
    }

    fn ai_prefill(&self) -> Effective<AiPrefill> {
        effective(self.current.ai.prefill.as_ref())
    }
}

pub(super) async fn recap_parked(state: &WebState) -> bool {
    match &state.recap {
        Some(handle) => handle.parked().await,
        None => false,
    }
}

pub(super) async fn show(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
) -> Result<Response, WebError> {
    let current = (*state.settings.load()).clone();
    let defaults = state.settings_store.defaults().clone();
    let recap_parked = recap_parked(&state).await;
    render(&ShowTpl {
        csrf: csrf::encode(&session.csrf_value),
        flash: flash::take(&cookies),
        user_login: session.user_login.clone(),
        user_avatar_url: session.avatar_url.clone(),
        current_page: crate::nav::SETTINGS,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, Role::Owner),
        current,
        defaults,
        errors: Vec::new(),
        recap_parked,
    })
}
