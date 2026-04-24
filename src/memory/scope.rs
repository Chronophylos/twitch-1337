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
}
