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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_no_forbidden_dashes(text: &str) {
        assert!(
            !text.contains('\u{2014}') && !text.contains('\u{2013}'),
            "guide has forbidden dash: {text}"
        );
    }

    #[test]
    fn single_guide_is_day_focused() {
        let g = guide_for(SpreadKind::Single).to_lowercase();
        assert!(g.contains("tageskarte") || g.contains("single"));
        assert!(g.contains("große arkana") || g.contains("grosse arkana"));
        assert!(g.contains("tagesdevise") || g.contains("erstgefühl") || g.contains("erstgefuehl"));
        assert!(!g.contains("ursache"));
        assert!(!g.contains("vergangenheit"));
        assert!(g.contains("erfinden"));
        assert_no_forbidden_dashes(guide_for(SpreadKind::Single));
    }

    #[test]
    fn guidance_guide_has_cause_obstacle_advice() {
        let g = guide_for(SpreadKind::Guidance).to_lowercase();
        assert!(g.contains("ursache") && g.contains("hindernis") && g.contains("rat"));
        assert!(g.contains("synoptisch") || g.contains("muster") || g.contains("fragebezogen"));
        assert!(!g.contains("vergangenheit"));
        assert!(g.contains("erfinden"));
        assert!(g.contains("fließend") || g.contains("fliessend") || g.contains("ineinander"));
        assert_no_forbidden_dashes(guide_for(SpreadKind::Guidance));
    }

    #[test]
    fn future_guide_has_timeline_and_bans_label_formula() {
        let g = guide_for(SpreadKind::Future).to_lowercase();
        assert!(g.contains("vergangenheit") && g.contains("gegenwart") && g.contains("zukunft"));
        assert!(g.contains("trend") || g.contains("kausal") || g.contains("geschichte"));
        assert!(!g.contains("ursache → hindernis") && !g.contains("ursache -> hindernis"));
        assert!(
            g.contains("kein `vergangenheit")
                || g.contains("kein vergangenheit")
                || g.contains("nicht als steife")
        );
        assert!(g.contains("erfinden"));
        assert_no_forbidden_dashes(guide_for(SpreadKind::Future));
    }
}
