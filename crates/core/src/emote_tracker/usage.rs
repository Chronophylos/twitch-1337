use std::collections::HashMap;

/// Map whitespace-separated chat tokens to emote ids using an active-name index.
///
/// One output id per matching token, in chat order. Names are matched exactly
/// (case-sensitive). Tokens that do not appear in `active_names` are skipped.
pub fn match_usage_tokens(text: &str, active_names: &HashMap<String, String>) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|token| active_names.get(token).cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::match_usage_tokens;

    #[test]
    fn exact_match_only_whitespace_tokens() {
        let mut map = HashMap::new();
        map.insert("Kappa".into(), "e1".into());
        map.insert("Pog".into(), "e2".into());
        let ids = match_usage_tokens("hello Kappa world Pog", &map);
        assert_eq!(ids, vec!["e1", "e2"]);
    }

    #[test]
    fn duplicate_tokens_emit_duplicate_ids() {
        let mut map = HashMap::new();
        map.insert("Kappa".into(), "e1".into());
        let ids = match_usage_tokens("Kappa Kappa", &map);
        assert_eq!(ids, vec!["e1", "e1"]);
    }

    #[test]
    fn no_substring_match() {
        let mut map = HashMap::new();
        map.insert("Kappa".into(), "e1".into());
        assert!(match_usage_tokens("Kappa123", &map).is_empty());
        assert!(match_usage_tokens("xKappa", &map).is_empty());
    }

    #[test]
    fn case_sensitive_name_match() {
        let mut map = HashMap::new();
        map.insert("Kappa".into(), "e1".into());
        assert!(match_usage_tokens("kappa", &map).is_empty());
    }
}
