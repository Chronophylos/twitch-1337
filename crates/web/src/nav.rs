//! Sidebar navigation page identifiers and their breadcrumb trails.
//!
//! Each authed handler passes one of these to its `Tpl.current_page`
//! field; the sidebar template (`templates/sidebar.html`) compares
//! against the same literals to apply `class="active"`. Defining them
//! once here turns a typo on the Rust side into a name-resolution
//! error — the template still uses raw strings, but those are reviewed
//! together at the top of `sidebar.html` and any drift is visible in
//! a single file.

pub const LEADERBOARD: &str = "leaderboard";
pub const PINGS: &str = "pings";
pub const MEMORY_TREE: &str = "memory";
pub const MEMORY_SOUL: &str = "memory_soul";
pub const MEMORY_LORE: &str = "memory_lore";
pub const MEMORY_USERS: &str = "memory_users";
pub const MEMORY_SCRATCH: &str = "memory_scratch";
pub const SCHEDULES: &str = "schedules";
pub const AI_PROFILES: &str = "ai_profiles";
pub const FLIGHTS: &str = "flights";
pub const EMOTES: &str = "emotes";
pub const EMOTES_AUDIT: &str = "emotes_audit";
pub const EMOTES_USAGE: &str = "emotes_usage";
pub const EMOTES_MEANINGS: &str = "emotes_meanings";
pub const LOGS: &str = "logs";
pub const SETTINGS: &str = "settings";

/// One breadcrumb: the page's label and where it lives.
pub struct Crumb {
    pub label: &'static str,
    pub href: &'static str,
}

const fn crumb(label: &'static str, href: &'static str) -> Crumb {
    Crumb { label, href }
}

const MEMORY: Crumb = crumb("Memory", "/memory");
const EMOTES_ROOT: Crumb = crumb("Emotes", "/emotes");

/// Breadcrumb trail for a page, root first, ending at the page itself.
/// `base.html` renders every crumb but the last as a link; a detail page
/// (ping editor, Character Sheet, …) links the whole trail and appends its
/// own leaf (see `templates/_crumbs.html`).
pub fn crumbs(page: &str) -> &'static [Crumb] {
    match page {
        LEADERBOARD => const { &[crumb("Leaderboard", "/leaderboard")] },
        PINGS => const { &[crumb("Pings", "/pings")] },
        FLIGHTS => const { &[crumb("Flights", "/flights")] },
        SCHEDULES => const { &[crumb("Schedules", "/schedules")] },
        MEMORY_TREE => &[MEMORY],
        MEMORY_SOUL => const { &[MEMORY, crumb("SOUL", "/memory/soul")] },
        MEMORY_LORE => const { &[MEMORY, crumb("LORE", "/memory/lore")] },
        MEMORY_USERS => const { &[MEMORY, crumb("Character Sheets", "/memory/users")] },
        MEMORY_SCRATCH => const { &[MEMORY, crumb("Scratch", "/memory/scratch")] },
        EMOTES => &[EMOTES_ROOT],
        EMOTES_AUDIT => const { &[EMOTES_ROOT, crumb("Audit", "/emotes/audit")] },
        EMOTES_USAGE => const { &[EMOTES_ROOT, crumb("Usage", "/emotes/usage")] },
        EMOTES_MEANINGS => const { &[EMOTES_ROOT, crumb("Meanings", "/emotes/meanings")] },
        AI_PROFILES => const { &[crumb("AI Profiles", "/ai-profiles")] },
        SETTINGS => const { &[crumb("Settings", "/settings")] },
        LOGS => const { &[crumb("Logs", "/logs")] },
        _ => &[],
    }
}
