/// Kind of channel-set lifecycle event stored in `set_events.kind`.
///
/// Cold start uses per-emote [`Self::Bootstrap`] rows (not fake adds).
/// [`Self::Gap`] marks a continuity break (disconnect / set switch); its
/// `emote_id` may be `NULL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetEventKind {
    /// Initial catalog snapshot when the tracker first sees an emote.
    Bootstrap,
    /// Emote added to the channel set after bootstrap.
    Add,
    /// Emote removed from the channel set (catalog row soft-deactivated).
    Remove,
    /// Emote kept on the set but renamed (`old_name` → `new_name`).
    Rename,
    /// Explicit gap marker when ingest continuity was broken.
    Gap,
}

impl SetEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bootstrap => "bootstrap",
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Rename => "rename",
            Self::Gap => "gap",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bootstrap" => Some(Self::Bootstrap),
            "add" => Some(Self::Add),
            "remove" => Some(Self::Remove),
            "rename" => Some(Self::Rename),
            "gap" => Some(Self::Gap),
            _ => None,
        }
    }
}

/// One row from the `emotes` catalog (live membership + tombstones).
///
/// Canonical identity is [`Self::emote_id`] (7TV id). [`Self::active`] is
/// soft-delete: removed emotes stay forever with `active = false` so event
/// FKs remain valid. Timestamps are UTC unix milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmoteRow {
    /// 7TV emote id — stable across renames.
    pub emote_id: String,
    /// Current (or last-known, if inactive) channel-set name.
    pub name: String,
    /// `true` if currently on the channel set; `false` is a tombstone.
    pub active: bool,
    /// Last catalog update time (UTC unix ms).
    pub updated_at: i64,
}

/// Input for inserting one `set_events` audit row.
///
/// Non-gap kinds should pass an `emote_id` that already exists in `emotes`
/// (caller upserts first). Gap events may leave `emote_id` / name / actor
/// fields empty and use [`Self::gap_from`] / [`Self::gap_to`] instead.
/// Timestamps are UTC unix milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSetEvent {
    /// Event time (UTC unix ms).
    pub at: i64,
    pub kind: SetEventKind,
    /// Target emote; `None` for gap markers.
    pub emote_id: Option<String>,
    /// Previous name (rename); optional elsewhere.
    pub old_name: Option<String>,
    /// New name (rename / add / bootstrap); optional elsewhere.
    pub new_name: Option<String>,
    /// 7TV actor who performed the set change, when known.
    pub actor_7tv_id: Option<String>,
    pub actor_username: Option<String>,
    pub actor_display_name: Option<String>,
    /// Gap window start (UTC unix ms), when `kind` is [`SetEventKind::Gap`].
    pub gap_from: Option<i64>,
    /// Gap window end (UTC unix ms), when `kind` is [`SetEventKind::Gap`].
    pub gap_to: Option<i64>,
}

/// Input for inserting one `usage_events` chat-usage row.
///
/// One event per matching token (`Kappa Kappa` → two rows). `emote_id` must
/// already exist in `emotes` (FK). [`Self::flagged`] marks bot/clanker
/// traffic so default queries can exclude it. Timestamps are UTC unix ms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogMutation {
    Upsert {
        emote_id: String,
        name: String,
        active: bool,
    },
    Deactivate {
        emote_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetChangeBatch {
    pub updated_at: i64,
    pub mutations: Vec<CatalogMutation>,
    pub events: Vec<NewSetEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUsageEvent {
    /// Observation time (UTC unix ms).
    pub at: i64,
    /// 7TV emote id that matched the chat token.
    pub emote_id: String,
    /// Twitch user id of the chatter.
    pub twitch_user_id: String,
    /// Twitch login at observation time, when available.
    pub login: Option<String>,
    /// `true` when the speaker is a flagged actor (bot / clanker denylist).
    pub flagged: bool,
}
