//! Landing grades from Volanta's per-touchdown landing events.
//!
//! The grade is the worse of the sink-rate and G-force grades, one step
//! worse per bounce. Crosswind, pitch and roll are reported but never change
//! the grade: they describe conditions and technique, not how hard the
//! aircraft met the runway.

use jiff::SignedDuration;
use jiff::civil::DateTime;

use super::client::FlightEvent;

/// Touchdowns this close to the first one belong to the same landing; each
/// extra one is a bounce.
const BOUNCE_WINDOW: SignedDuration = SignedDuration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Grade {
    Butter,
    Smooth,
    Solid,
    Firm,
    Hard,
    Workshop,
}

impl Grade {
    const ALL: [Self; 6] = [
        Self::Butter,
        Self::Smooth,
        Self::Solid,
        Self::Firm,
        Self::Hard,
        Self::Workshop,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Butter => "🧈 Butter",
            Self::Smooth => "✨ Sanft",
            Self::Solid => "👍 Solide",
            Self::Firm => "😬 Fest",
            Self::Hard => "💥 Hart",
            Self::Workshop => "🔧 Fall für die Werkstatt",
        }
    }

    /// Sink rate in fpm, as a positive number.
    fn from_fpm(fpm: f64) -> Self {
        Self::band(fpm, [60.0, 150.0, 300.0, 500.0, 800.0])
    }

    fn from_g(g: f64) -> Self {
        Self::band(g, [1.15, 1.30, 1.50, 2.00, 2.50])
    }

    /// The first grade whose upper bound `value` stays below.
    fn band(value: f64, bounds: [f64; 5]) -> Self {
        bounds
            .iter()
            .position(|b| value < *b)
            .map_or(Self::Workshop, |i| Self::ALL[i])
    }

    fn worse_by(self, steps: usize) -> Self {
        let i = Self::ALL.iter().position(|g| *g == self).unwrap_or(0);
        Self::ALL[(i + steps).min(Self::ALL.len() - 1)]
    }
}

/// One graded landing.
#[derive(Debug, Clone, PartialEq)]
pub struct Rating {
    pub grade: Grade,
    pub rate_fpm: i64,
    pub g_force: Option<f64>,
    pub bounces: usize,
    /// Crosswind component in knots.
    pub crosswind_kt: Option<i64>,
    pub pitch_deg: Option<f64>,
    pub roll_deg: Option<f64>,
}

/// Grade the most recent landing in `events` after `after`, `None` when
/// Volanta recorded no touchdown since.
///
/// `after` is the takeoff the landing must follow, so a previous circuit's
/// landing is never passed off as the current one while Volanta has not
/// recorded the newest touchdown yet. Touchdowns are grouped into landings:
/// a touchdown within [`BOUNCE_WINDOW`] of a landing's first touchdown is a
/// bounce of it. The newest landing is graded on its first touchdown.
pub fn rate_latest(events: &[FlightEvent], after: Option<jiff::Timestamp>) -> Option<Rating> {
    // Volanta's event times are UTC wall times without an offset.
    let after = after.map(|t| jiff::tz::Offset::UTC.to_datetime(t));
    let mut touchdowns: Vec<(DateTime, &FlightEvent)> = events
        .iter()
        .filter(|e| !e.is_hidden && e.landing_rate.is_some_and(f64::is_finite))
        .filter_map(|e| Some((e.time.as_deref()?.parse::<DateTime>().ok()?, e)))
        .filter(|(t, _)| after.is_none_or(|after| *t > after))
        .collect();
    touchdowns.sort_by_key(|(t, _)| *t);

    let mut first = 0;
    for i in 1..touchdowns.len() {
        if touchdowns[i].0.duration_since(touchdowns[first].0) > BOUNCE_WINDOW {
            first = i;
        }
    }
    let (_, touchdown) = touchdowns.get(first)?;
    let bounces = touchdowns.len() - first - 1;

    let fpm = touchdown.landing_rate.map_or(0.0, f64::abs);
    let g_force = touchdown.g_force.filter(|g| g.is_finite());
    let grade = Grade::from_fpm(fpm)
        .max(g_force.map_or(Grade::Butter, Grade::from_g))
        .worse_by(bounces);

    #[expect(clippy::cast_possible_truncation, reason = "finite, rounded sink rate")]
    let rate_fpm = fpm.round() as i64;
    Some(Rating {
        grade,
        rate_fpm,
        g_force,
        bounces,
        crosswind_kt: crosswind(touchdown),
        pitch_deg: touchdown.pitch.filter(|p| p.is_finite()),
        roll_deg: touchdown.roll.filter(|r| r.is_finite()),
    })
}

/// Wind component across the aircraft's heading, in knots.
fn crosswind(e: &FlightEvent) -> Option<i64> {
    let (speed, from, heading) = (e.wind_speed?, e.wind_heading?, e.heading?);
    let across = speed * (from - heading).to_radians().sin().abs();
    #[expect(clippy::cast_possible_truncation, reason = "finite, rounded knots")]
    across.is_finite().then(|| across.round() as i64)
}

/// `✨ Sanft (163 fpm, 1.10 G, 2× aufgesetzt, 3 kt Seitenwind, Pitch 2.7°, Roll -0.3°)`.
pub fn render(r: &Rating) -> String {
    let mut parts = vec![format!("{} fpm", r.rate_fpm)];
    if let Some(g) = r.g_force {
        parts.push(format!("{g:.2} G"));
    }
    if r.bounces > 0 {
        parts.push(format!("{}× aufgesetzt", r.bounces + 1));
    }
    if let Some(kt) = r.crosswind_kt.filter(|kt| *kt > 0) {
        parts.push(format!("{kt} kt Seitenwind"));
    }
    if let Some(p) = r.pitch_deg {
        parts.push(format!("Pitch {p:.1}°"));
    }
    if let Some(roll) = r.roll_deg {
        parts.push(format!("Roll {roll:.1}°"));
    }
    format!("{} ({})", r.grade.label(), parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touchdown(time: &str, rate: f64, g: f64) -> FlightEvent {
        FlightEvent {
            time: Some(time.into()),
            landing_rate: Some(rate),
            g_force: Some(g),
            pitch: Some(2.745),
            roll: Some(-0.291),
            heading: Some(251.3),
            wind_heading: Some(236.5),
            wind_speed: Some(12.0),
            is_hidden: false,
        }
    }

    #[test]
    fn grade_is_the_worse_of_fpm_and_g() {
        let rate = |fpm, g| rate_latest(&[touchdown("2026-11-23T20:37:34.4", fpm, g)], None);
        let r = rate(-163.3, 1.10).unwrap();
        assert_eq!(r.grade, Grade::Solid, "163 fpm is solid even at 1.10 G");
        assert_eq!(
            rate(-40.0, 1.6).unwrap().grade,
            Grade::Firm,
            "a 1.6 G butter is firm"
        );
        assert_eq!(rate(-59.9, 1.149).unwrap().grade, Grade::Butter);
        assert_eq!(rate(-800.0, 1.0).unwrap().grade, Grade::Workshop);
    }

    #[test]
    fn bounces_worsen_and_only_the_newest_landing_counts() {
        let events = [
            // An earlier landing (touch-and-go), long before.
            touchdown("2026-11-23T20:00:00", -900.0, 3.0),
            touchdown("2026-11-23T20:37:30", -50.0, 1.05),
            touchdown("2026-11-23T20:37:36", -120.0, 1.2),
            touchdown("2026-11-23T20:37:44", -80.0, 1.1),
        ];
        let r = rate_latest(&events, None).unwrap();
        assert_eq!(
            r.rate_fpm, 50,
            "graded on the newest landing's first touchdown"
        );
        assert_eq!(r.bounces, 2);
        assert_eq!(r.grade, Grade::Solid, "butter, two bounces worse");
    }

    #[test]
    fn non_landing_events_and_empty_lists_rate_nothing() {
        let other = FlightEvent {
            time: Some("2026-11-23T19:56:47".into()),
            ..FlightEvent::default()
        };
        assert_eq!(rate_latest(&[other], None), None);
        assert_eq!(rate_latest(&[], None), None);
    }

    /// A landing from before the last takeoff is not this landing's rating.
    #[test]
    fn touchdowns_before_the_takeoff_are_ignored() {
        let events = [touchdown("2026-11-23T20:00:00", -50.0, 1.05)];
        let takeoff: jiff::Timestamp = "2026-11-23T20:10:00Z".parse().unwrap();
        assert_eq!(rate_latest(&events, Some(takeoff)), None);
        let earlier: jiff::Timestamp = "2026-11-23T19:50:00Z".parse().unwrap();
        assert!(rate_latest(&events, Some(earlier)).is_some());
    }

    /// The gate-spawn ground contact Volanta hides is never a landing.
    #[test]
    fn hidden_spawn_contact_is_not_a_landing() {
        // As recorded at BIKF for flight 01a20dda-6e22-76a1-89f5-0130addbad96.
        let spawn = FlightEvent {
            is_hidden: true,
            ..touchdown("2026-11-24T14:08:30.906628", 0.1297, 0.0002)
        };
        assert_eq!(rate_latest(std::slice::from_ref(&spawn), None), None);
        let landing = touchdown("2026-11-24T18:01:00", -528.0, 1.187);
        let r = rate_latest(&[spawn, landing], None).unwrap();
        assert_eq!((r.rate_fpm, r.grade), (528, Grade::Hard));
    }

    #[test]
    fn renders_the_line() {
        let r = rate_latest(
            &[
                touchdown("2026-11-23T20:37:34.4", -163.3, 1.104),
                touchdown("2026-11-23T20:37:40.0", -90.0, 1.05),
            ],
            None,
        )
        .unwrap();
        assert_eq!(
            render(&r),
            "😬 Fest (163 fpm, 1.10 G, 2× aufgesetzt, 3 kt Seitenwind, Pitch 2.7°, Roll -0.3°)"
        );
    }
}
