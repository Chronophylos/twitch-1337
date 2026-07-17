#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetEventKind {
    Bootstrap,
    Add,
    Remove,
    Rename,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmoteRow {
    pub emote_id: String,
    pub name: String,
    pub active: bool,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSetEvent {
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUsageEvent {
    pub at: i64,
    pub emote_id: String,
    pub twitch_user_id: String,
    pub login: Option<String>,
    pub flagged: bool,
}
