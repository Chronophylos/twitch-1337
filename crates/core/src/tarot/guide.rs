use crate::tarot::SpreadKind;

const GUIDE_SINGLE: &str = include_str!("guide_single.md");
const GUIDE_GUIDANCE: &str = include_str!("guide_guidance.md");
const GUIDE_FUTURE: &str = include_str!("guide_future.md");

/// Reading guide for the chosen spread only (not the full multi-spread doc).
pub fn guide_for(spread: SpreadKind) -> &'static str {
    match spread {
        SpreadKind::Single => GUIDE_SINGLE,
        SpreadKind::Guidance => GUIDE_GUIDANCE,
        SpreadKind::Future => GUIDE_FUTURE,
    }
}
