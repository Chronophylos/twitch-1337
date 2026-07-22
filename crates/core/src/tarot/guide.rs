pub const READING_GUIDE: &str = include_str!("reading_guide.md");

#[cfg(test)]
mod tests {
    use super::READING_GUIDE;

    #[test]
    fn reading_guide_mentions_positions_and_no_inventing() {
        assert!(!READING_GUIDE.is_empty());
        let lower = READING_GUIDE.to_lowercase();
        assert!(lower.contains("past"));
        assert!(lower.contains("present"));
        assert!(lower.contains("future"));
        assert!(lower.contains("reversed") || lower.contains("upright"));
        assert!(
            lower.contains("erfinden") || lower.contains("keine karten erfinden"),
            "guide should forbid inventing cards: {READING_GUIDE}"
        );
        assert!(
            !READING_GUIDE.contains('\u{2014}') && !READING_GUIDE.contains('\u{2013}'),
            "guide has forbidden dash"
        );
    }
}
