#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpreadKind {
    /// Tageskarte.
    Single,
    /// Thema / guidance: Ursache, Hindernis, Rat.
    Guidance,
    /// Zukunftsfrage: Vergangenheit, Gegenwart, Zukunft.
    Future,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpreadError;

pub const SINGLE_CARD_LABEL: &str = "Day";
pub const GUIDANCE_LABELS: [&str; 3] = ["Ursache", "Hindernis", "Rat"];
pub const FUTURE_LABELS: [&str; 3] = ["Vergangenheit", "Gegenwart", "Zukunft"];

impl SpreadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SpreadKind::Single => "single",
            SpreadKind::Guidance => "guidance",
            SpreadKind::Future => "future",
        }
    }

    pub fn three_card_labels(self) -> Option<[&'static str; 3]> {
        match self {
            SpreadKind::Guidance => Some(GUIDANCE_LABELS),
            SpreadKind::Future => Some(FUTURE_LABELS),
            SpreadKind::Single => None,
        }
    }
}

/// Default when omitted: `future` (classic timeline).
pub fn parse_spread(raw: Option<&str>) -> Result<SpreadKind, SpreadError> {
    match raw {
        None => Ok(SpreadKind::Future),
        Some(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Err(SpreadError)
            } else if trimmed == "single" {
                Ok(SpreadKind::Single)
            } else if trimmed == "guidance" {
                Ok(SpreadKind::Guidance)
            } else if trimmed == "future" || trimmed == "three_card" {
                // `three_card` kept as alias for older prompts / caches.
                Ok(SpreadKind::Future)
            } else {
                Err(SpreadError)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_named_spreads_ok() {
        assert!(matches!(parse_spread(None).unwrap(), SpreadKind::Future));
        assert!(matches!(
            parse_spread(Some("future")).unwrap(),
            SpreadKind::Future
        ));
        assert!(matches!(
            parse_spread(Some("three_card")).unwrap(),
            SpreadKind::Future
        ));
        assert!(matches!(
            parse_spread(Some("guidance")).unwrap(),
            SpreadKind::Guidance
        ));
        assert!(matches!(
            parse_spread(Some("single")).unwrap(),
            SpreadKind::Single
        ));
    }

    #[test]
    fn unknown_spread_errors() {
        assert!(parse_spread(Some("celtic_cross")).is_err());
        assert!(parse_spread(Some("")).is_err());
    }

    #[test]
    fn guidance_and_future_labels() {
        assert_eq!(GUIDANCE_LABELS, ["Ursache", "Hindernis", "Rat"]);
        assert_eq!(FUTURE_LABELS, ["Vergangenheit", "Gegenwart", "Zukunft"]);
    }
}
