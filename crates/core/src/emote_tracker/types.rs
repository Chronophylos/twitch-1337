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

/// One persisted row from `set_events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetEventRow {
    pub id: i64,
    pub at: i64,
    pub kind: SetEventKind,
    pub emote_id: Option<String>,
    pub old_name: Option<String>,
    pub new_name: Option<String>,
    pub actor_7tv_id: Option<String>,
    pub actor_username: Option<String>,
    pub actor_display_name: Option<String>,
    pub gap_from: Option<i64>,
    pub gap_to: Option<i64>,
    /// Potat history requester login (lowercase), when attributed.
    pub requester_login: Option<String>,
    /// Twitch user id of the requester, when known.
    pub requester_twitch_id: Option<String>,
    /// Mediation product label (`"potatbotat"`) when requester is set.
    pub via: Option<String>,
    /// How attribution was resolved (`"potat_history"`), when set.
    pub attribution_method: Option<String>,
}

impl SetEventRow {
    /// Rows found by diffing a REST snapshot (startup, reconnect) carry
    /// neither a 7TV actor nor a requester, and their `at` is detection time.
    // ponytail: inferred from missing actor; a live EventAPI change without an
    // actor reads as approximate too. Add a stored `source` column if that bites.
    pub fn is_snapshot_diff(&self) -> bool {
        self.actor_7tv_id.is_none()
            && self.actor_username.is_none()
            && self.requester_login.is_none()
    }
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
    /// Potat history requester login (lowercase), when attributed.
    pub requester_login: Option<String>,
    /// Twitch user id of the requester, when known.
    pub requester_twitch_id: Option<String>,
    /// Mediation product label (`"potatbotat"`) when requester is set.
    pub via: Option<String>,
    /// How attribution was resolved (`"potat_history"`), when set.
    pub attribution_method: Option<String>,
}

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

/// One row from `usage_events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEventRow {
    pub id: i64,
    pub at: i64,
    pub emote_id: String,
    pub twitch_user_id: String,
    pub login: Option<String>,
    pub flagged: bool,
}

/// Usage count for one emote within a query window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmoteUsageCount {
    /// 7TV emote id.
    pub emote_id: String,
    /// Current (or last-known) channel-set name.
    pub name: String,
    /// Unflagged uses in the window.
    pub count: i64,
    /// Most recent use in the window (UTC unix ms).
    pub last_used_at: i64,
    /// `false` when the emote is not active, including global-only FK rows.
    pub active: bool,
    /// Whether lifecycle history distinguishes a channel tombstone from a
    /// global-only FK row.
    pub has_channel_lifecycle: bool,
}

/// Usage of one active emote, including emotes with zero uses in the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeastUsedEmote {
    /// 7TV emote id.
    pub emote_id: String,
    /// Current channel-set name.
    pub name: String,
    /// Unflagged uses in the window (0 = never used).
    pub count: i64,
    /// Most recent use in the window, if any (UTC unix ms).
    pub last_used_at: Option<i64>,
    /// Start of the emote's current stay on the set, if recorded.
    pub tenure: Option<SetTenure>,
}

/// Start of an emote's current stay on the set: the newest bootstrap/add row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetTenure {
    /// Already on the set when tracking began; the real add date is unknown.
    TrackingStart,
    /// Added at `at` (UTC unix ms). `approximate` marks a snapshot-diff row
    /// (see [`SetEventRow::is_snapshot_diff`]) stamped at detection time.
    Added { at: i64, approximate: bool },
}

/// Per-viewer count inside one emote's usage breakdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserUsageCount {
    /// Twitch user id (stable grouping key).
    pub twitch_user_id: String,
    /// Login at their most recent use in the window, when known.
    pub login: Option<String>,
    /// Unflagged uses of one emote in the window.
    pub count: i64,
    /// Most recent use in the window (UTC unix ms).
    pub last_used_at: i64,
}

#[derive(Debug, Clone, Default)]
pub struct SetEventListFilter {
    pub kind: Option<SetEventKind>,
    pub emote_id: Option<String>,
    pub name_substr: Option<String>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    /// With `emote_id`, also match rows whose old/new name is exactly this:
    /// 7TV version swaps reuse a name under a new emote id.
    pub emote_name: Option<String>,
    /// Only add/remove/rename rows; drops bootstrap and gap markers.
    pub lifecycle_only: bool,
    /// Requester login / Twitch id or 7TV actor username (case-insensitive).
    pub user: Option<String>,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Debug, Clone, Default)]
pub struct UsageEventListFilter {
    pub emote_id: Option<String>,
    pub twitch_user_id: Option<String>,
    pub login_substr: Option<String>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub include_flagged: bool,
    pub limit: u32,
    /// Keyset cursor `(at, id)`: only events strictly older in `(at DESC,
    /// id DESC)` order. Carries the id because one chat message stamps all
    /// its emotes with the same `at`, so a timestamp alone would skip rows.
    pub before: Option<(i64, i64)>,
}

/// One of the four text fields of an Emote Meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MeaningField {
    Appearance,
    Meaning,
    Usage,
    Avoid,
}

impl MeaningField {
    pub const ALL: [Self; 4] = [Self::Appearance, Self::Meaning, Self::Usage, Self::Avoid];

    /// Column name in `emote_meanings`, also the form / JSON key.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Appearance => "appearance",
            Self::Meaning => "meaning",
            Self::Usage => "usage",
            Self::Avoid => "avoid",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == s)
    }

    /// Bit of this field in `emote_meanings.curated`.
    pub const fn bit(self) -> u8 {
        match self {
            Self::Appearance => 1,
            Self::Meaning => 2,
            Self::Usage => 4,
            Self::Avoid => 8,
        }
    }
}

/// Set of Curated Fields: fields a mod edited, which automation never
/// overwrites (ADR-0014). Stored as the `emote_meanings.curated` bitmask.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CuratedFields(u8);

impl CuratedFields {
    const ALL_BITS: u8 = 0b1111;

    /// Unknown bits (a newer build) are dropped.
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & Self::ALL_BITS)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, field: MeaningField) -> bool {
        self.0 & field.bit() != 0
    }

    /// `true` when every field is curated, so automation has nothing to do.
    pub const fn is_all(self) -> bool {
        self.0 == Self::ALL_BITS
    }
}

/// Longest accepted Emote Meaning field, in characters.
pub const MEANING_FIELD_MAX_CHARS: usize = 300;

/// Why an Emote Meaning field value was rejected. `Display` is for logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MeaningTextError {
    #[error("appearance must not be empty")]
    EmptyAppearance,
    #[error("{field} exceeds {MEANING_FIELD_MAX_CHARS} characters")]
    TooLong { field: &'static str },
    #[error("{field} contains control characters")]
    ControlChar { field: &'static str },
}

/// Validate and normalize one field value, shared by the generator and the
/// editor: trimmed, at most [`MEANING_FIELD_MAX_CHARS`] characters, no
/// control characters, `appearance` non-empty. A blank optional field
/// becomes `None`.
pub fn validate_meaning_text(
    field: MeaningField,
    value: &str,
) -> Result<Option<String>, MeaningTextError> {
    let value = value.trim();
    if value.chars().any(char::is_control) {
        return Err(MeaningTextError::ControlChar {
            field: field.as_str(),
        });
    }
    if value.chars().count() > MEANING_FIELD_MAX_CHARS {
        return Err(MeaningTextError::TooLong {
            field: field.as_str(),
        });
    }
    if value.is_empty() {
        return match field {
            MeaningField::Appearance => Err(MeaningTextError::EmptyAppearance),
            _ => Ok(None),
        };
    }
    Ok(Some(value.to_owned()))
}

/// One row from `emote_meanings` (semantics; no FK to `emotes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmoteMeaningRow {
    pub emote_id: String,
    pub appearance: String,
    pub meaning: Option<String>,
    pub usage: Option<String>,
    pub avoid: Option<String>,
    pub updated_at: i64,
    /// Emote name at the last generation (catalog name for migrated seed
    /// rows; `None` for seeded globals).
    pub name: Option<String>,
    pub curated: CuratedFields,
    /// When the Emotes agent last wrote the row (UTC ms); `None` = never.
    pub generated_at: Option<i64>,
    /// Chat lines the last generation saw; 0 = generated without context.
    pub context_lines: i64,
    /// Post-window usage count the last generation saw.
    pub usage_count: i64,
}

impl EmoteMeaningRow {
    /// Value of one text field (`None` when the optional field is empty).
    pub fn field(&self, field: MeaningField) -> Option<&str> {
        match field {
            MeaningField::Appearance => Some(self.appearance.as_str()),
            MeaningField::Meaning => self.meaning.as_deref(),
            MeaningField::Usage => self.usage.as_deref(),
            MeaningField::Avoid => self.avoid.as_deref(),
        }
    }
}

/// Validated generator output for one emote plus its generation metadata.
/// Written by [`super::EmoteTrackerHandle::apply_generated_meaning`], which
/// skips every curated field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedMeaning {
    pub emote_id: String,
    pub name: String,
    pub appearance: String,
    pub meaning: Option<String>,
    pub usage: Option<String>,
    pub avoid: Option<String>,
    /// Generation time (UTC ms).
    pub generated_at: i64,
    pub context_lines: i64,
    pub usage_count: i64,
}

/// One chat message that used an emote, for context sampling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageSample {
    /// Message time (UTC ms).
    pub at: i64,
    pub twitch_user_id: String,
    pub login: Option<String>,
}

/// Which usage counts toward Emote Meaning context: unflagged, outside every
/// try-out window, not by a deny-listed login, at or after `since_ms`.
#[derive(Debug, Clone, Default)]
pub struct PostWindowFilter {
    pub since_ms: i64,
    /// Try-out window length after each `add` set event (ms).
    pub tryout_ms: i64,
    /// Logins excluded regardless of the stored `flagged` bit (lowercase).
    pub deny_logins: Vec<String>,
}

/// Input for inserting one `usage_events` chat-usage row.
///
/// One event per matching token (`Kappa Kappa` → two rows). `emote_id` must
/// already exist in `emotes` (FK). [`Self::flagged`] marks bot/clanker
/// traffic so default queries can exclude it. Timestamps are UTC unix ms.
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
