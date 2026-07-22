#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpreadKind {
    ThreeCard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpreadError;

pub const THREE_CARD_LABELS: [&str; 3] = ["Past", "Present", "Future"];

pub fn parse_spread(raw: Option<&str>) -> Result<SpreadKind, SpreadError> {
    match raw {
        None => Ok(SpreadKind::ThreeCard),
        Some(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Err(SpreadError)
            } else if trimmed == "three_card" {
                Ok(SpreadKind::ThreeCard)
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
    fn default_and_three_card_ok() {
        assert!(matches!(parse_spread(None).unwrap(), SpreadKind::ThreeCard));
        assert!(matches!(
            parse_spread(Some("three_card")).unwrap(),
            SpreadKind::ThreeCard
        ));
    }

    #[test]
    fn unknown_spread_errors() {
        assert!(parse_spread(Some("celtic_cross")).is_err());
        assert!(parse_spread(Some("")).is_err());
    }

    #[test]
    fn three_card_labels_order() {
        assert_eq!(THREE_CARD_LABELS, ["Past", "Present", "Future"]);
    }
}
