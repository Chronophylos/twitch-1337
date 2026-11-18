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
    fn should_flag_usage_cases() {
        let deny = ["custombot".to_string()];
        // (login, deny_list, expected): own bot login, builtin clanker,
        // configured deny-list entry, normal chatter.
        let cases: &[(&str, &[String], bool)] = &[
            ("mybot", &[], true),
            ("supibot", &[], true),
            ("custombot", &deny, true),
            ("alice", &[], false),
        ];
        for &(login, deny_list, expected) in cases {
            assert_eq!(
                should_flag_usage(login, "mybot", deny_list),
                expected,
                "{login}"
            );
        }
    }
}
