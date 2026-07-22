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
            lower.contains("invent")
                || lower.contains("do not invent")
                || lower.contains("don't invent")
        );
    }
}
