use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    User { subject_id: String },
    Lore,
    Pref { subject_id: String },
}

impl Scope {
    pub fn tag(&self) -> &'static str {
        match self {
            Scope::User { .. } => "user",
            Scope::Lore => "lore",
            Scope::Pref { .. } => "pref",
        }
    }

    pub fn subject_id(&self) -> Option<&str> {
        match self {
            Scope::User { subject_id } | Scope::Pref { subject_id } => Some(subject_id),
            Scope::Lore => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UserRole {
    Regular,
    Moderator,
    Broadcaster,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    SelfClaim,
    ThirdParty,
    ModBroadcaster,
}

use twitch_irc::message::Badge;

/// Classify a Twitch user's role from their badge list
/// (typically `PrivmsgMessage::badges`). Broadcaster outranks moderator.
pub fn classify_role(badges: &[Badge]) -> UserRole {
    let mut role = UserRole::Regular;
    for b in badges {
        let rank = match b.name.as_str() {
            "broadcaster" => UserRole::Broadcaster,
            "moderator" => UserRole::Moderator,
            _ => continue,
        };
        if rank > role {
            role = rank;
        }
    }
    role
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_user_serializes_with_subject_id() {
        let scope = Scope::User {
            subject_id: "12345".to_string(),
        };
        let s = ron::to_string(&scope).unwrap();
        assert!(s.contains("User"));
        assert!(s.contains("12345"));
    }

    #[test]
    fn scope_lore_round_trips() {
        let scope = Scope::Lore;
        let s = ron::to_string(&scope).unwrap();
        let back: Scope = ron::from_str(&s).unwrap();
        assert_eq!(back, Scope::Lore);
    }

    #[test]
    fn user_role_broadcaster_outranks_moderator() {
        assert!(UserRole::Broadcaster > UserRole::Moderator);
        assert!(UserRole::Moderator > UserRole::Regular);
    }

    use twitch_irc::message::Badge;

    fn badges(names: &[&str]) -> Vec<Badge> {
        names
            .iter()
            .map(|b| Badge {
                name: (*b).to_string(),
                version: "1".to_string(),
            })
            .collect()
    }

    #[test]
    fn classify_role_regular_default() {
        assert_eq!(classify_role(&badges(&[])), UserRole::Regular);
    }

    #[test]
    fn classify_role_moderator_badge() {
        assert_eq!(classify_role(&badges(&["moderator"])), UserRole::Moderator);
    }

    #[test]
    fn classify_role_broadcaster_beats_moderator() {
        assert_eq!(
            classify_role(&badges(&["moderator", "broadcaster"])),
            UserRole::Broadcaster
        );
    }
}
