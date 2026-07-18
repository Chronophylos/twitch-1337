//! Shared bot/clanker login filters for 1337 tracker and emote usage flagging.

const CLANKERS: &[&str] = &[
    "supibot",
    "potatbotat",
    "streamelements",
    "koknuts",
    "thedagothur",
];

/// Returns true when `login` is a known third-party bot account ignored by 1337.
pub fn is_clanker(login: &str) -> bool {
    CLANKERS.contains(&login)
}

/// Returns true when usage from this login should be stored with `flagged = 1`.
pub fn should_flag_usage(login: &str, bot_username: &str, deny_list: &[String]) -> bool {
    login == bot_username || is_clanker(login) || deny_list.iter().any(|d| d == login)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_clanker_matches_full_builtin_list() {
        for login in CLANKERS {
            assert!(is_clanker(login), "{login}");
        }
        assert!(!is_clanker("alice"));
    }

    #[test]
    fn should_flag_bot_login() {
        assert!(should_flag_usage("mybot", "mybot", &[]));
    }

    #[test]
    fn should_flag_clanker() {
        assert!(should_flag_usage("supibot", "mybot", &[]));
    }

    #[test]
    fn should_flag_deny_list_entry() {
        assert!(should_flag_usage(
            "custombot",
            "mybot",
            &["custombot".into()]
        ));
    }

    #[test]
    fn should_not_flag_normal_chatter() {
        assert!(!should_flag_usage("alice", "mybot", &[]));
    }
}
