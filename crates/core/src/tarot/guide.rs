pub const READING_GUIDE: &str = include_str!("reading_guide.md");

#[cfg(test)]
mod tests {
    use super::READING_GUIDE;

    #[test]
    fn reading_guide_covers_spreads_and_styles() {
        assert!(!READING_GUIDE.is_empty());
        let lower = READING_GUIDE.to_lowercase();
        assert!(lower.contains("single") && lower.contains("tageskarte"));
        assert!(lower.contains("guidance") && lower.contains("ursache"));
        assert!(lower.contains("hindernis") && lower.contains("rat"));
        assert!(lower.contains("future") && lower.contains("vergangenheit"));
        assert!(lower.contains("gegenwart") && lower.contains("zukunft"));
        assert!(lower.contains("reversed") || lower.contains("upright"));
        assert!(
            lower.contains("erfinden") || lower.contains("keine karten erfinden"),
            "guide should forbid inventing cards: {READING_GUIDE}"
        );
        assert!(
            !READING_GUIDE.contains('\u{2014}') && !READING_GUIDE.contains('\u{2013}'),
            "guide has forbidden dash"
        );
        assert!(
            lower.contains("nicht die label-formel")
                || lower.contains("kein `vergangenheit")
                || lower.contains("kein vergangenheit"),
            "guide should ban the clinical label formula: {READING_GUIDE}"
        );
        assert!(
            lower.contains("fließend")
                || lower.contains("fliessend")
                || lower.contains("ineinander"),
            "guide should push woven flowing replies: {READING_GUIDE}"
        );
        assert!(
            lower.contains("große arkana") || lower.contains("grosse arkana"),
            "guide should mention große arkana for day format: {READING_GUIDE}"
        );
    }
}
