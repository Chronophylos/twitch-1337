//! Authenticated dashboard tier.
//!
//! Ordered so `session.role >= required` is the gate predicate; inserting
//! a new tier later means slotting it in at the right ordinal.

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Role {
    Viewer,
    Mod,
    Owner,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Mod => "mod",
            Role::Owner => "owner",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Role;

    #[test]
    fn tiers_are_ordered_viewer_mod_owner() {
        assert!(Role::Viewer < Role::Mod);
        assert!(Role::Mod >= Role::Viewer);
        assert!(Role::Viewer >= Role::Viewer);
        assert!(Role::Owner > Role::Mod);
        assert!(Role::Owner > Role::Viewer);
    }
}
