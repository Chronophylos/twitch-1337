//! `!vol`: Volanta watchlist status, and opt-in tracking of one pilot's
//! flight. Answers from the handler's last feed snapshot; never calls
//! Volanta itself.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::aviation::volanta::client::LiveFlight;
use crate::aviation::volanta::state::flight_number;
use crate::aviation::volanta::{PilotStatus, TrackOutcome, UntrackOutcome, VolantaHub};
use crate::commands::{
    Command, CommandContext, ExecutionClass,
    reply_emotes::{ReplyEmoteCategory, cooldown_reply, error_reply},
};
use crate::cooldown::PerUserCooldown;
use crate::settings::SettingsHandle;
use crate::util::clock::Clock;

const USAGE: &str = "Nutzung: !vol [pilot] | !vol track <pilot> | !vol untrack <pilot>";

pub struct VolCommand {
    hub: Arc<VolantaHub>,
    settings: SettingsHandle,
    clock: Arc<dyn Clock>,
    cooldown: PerUserCooldown,
}

impl VolCommand {
    pub fn new(hub: Arc<VolantaHub>, settings: SettingsHandle, clock: Arc<dyn Clock>) -> Self {
        Self {
            hub,
            cooldown: PerUserCooldown::live(settings.clone(), |s| {
                std::time::Duration::from_secs(s.cooldowns.vol)
            }),
            settings,
            clock,
        }
    }

    /// The watchlist's spelling of `name`, if it is on the list.
    fn listed(&self, name: &str) -> Option<String> {
        self.settings
            .load()
            .volanta
            .pilots
            .iter()
            .find(|p| p.eq_ignore_ascii_case(name))
            .cloned()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Request<'a> {
    Overview,
    Status(&'a str),
    Track(&'a str),
    Untrack(&'a str),
    Usage,
}

fn parse(args: &[String]) -> Request<'_> {
    match args {
        [] => Request::Overview,
        [sub, pilot] if sub.eq_ignore_ascii_case("track") => Request::Track(pilot),
        [sub, pilot] if sub.eq_ignore_ascii_case("untrack") => Request::Untrack(pilot),
        [sub] if sub.eq_ignore_ascii_case("track") || sub.eq_ignore_ascii_case("untrack") => {
            Request::Usage
        }
        [pilot] => Request::Status(pilot),
        _ => Request::Usage,
    }
}

fn route(f: &LiveFlight) -> String {
    match (&f.origin_icao, &f.destination_icao) {
        (Some(o), Some(d)) => format!("{o} → {d}"),
        (Some(o), None) => format!("ab {o}"),
        (None, Some(d)) => format!("nach {d}"),
        (None, None) => "ohne Flugplan".to_owned(),
    }
}

/// `am Boden` or `in der Luft`; `None` without a position.
fn phase(f: &LiveFlight) -> Option<&'static str> {
    f.position.as_ref().map(|p| {
        if p.on_ground {
            "am Boden"
        } else {
            "in der Luft"
        }
    })
}

/// `name (VY1179)`, or just `name` without a flight number or callsign.
fn who(name: &str, f: &LiveFlight) -> String {
    match flight_number(f) {
        Some(n) => format!("{name} ({n})"),
        None => name.to_owned(),
    }
}

/// `Chris (VY1179): EDLP → EDDG, in der Luft, 12300 ft, 210 kt, A320 D-AIZA`.
fn live_line(name: &str, f: &LiveFlight) -> String {
    let mut parts = vec![route(f)];
    parts.extend(phase(f).map(str::to_owned));
    if let Some(pos) = &f.position {
        if let Some(alt) = pos.altitude.filter(|a| a.is_finite()) {
            parts.push(format!("{alt:.0} ft"));
        }
        if let Some(gs) = pos.ground_speed.filter(|g| g.is_finite()) {
            parts.push(format!("{gs:.0} kt"));
        }
    }
    let aircraft: Vec<&str> = [&f.aircraft_icao, &f.aircraft_registration]
        .into_iter()
        .filter_map(|s| s.as_deref())
        .collect();
    if !aircraft.is_empty() {
        parts.push(aircraft.join(" "));
    }
    format!("{}: {}", who(name, f), parts.join(", "))
}

#[async_trait]
impl<T, L> Command<T, L> for VolCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "vol"
    }

    fn name(&self) -> &str {
        "!vol"
    }

    fn description(&self) -> &str {
        "Volanta-Piloten: wer fliegt gerade, Flug im Chat verfolgen"
    }

    fn usage(&self) -> &str {
        "!vol [pilot] | !vol track <pilot> | !vol untrack <pilot>"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let reply = |text: String| async { ctx.sender.reply(&ctx.privmsg, text).await };
        let err = |category, text: &str| error_reply(&self.settings, category, text);

        if !self.settings.load().volanta.enabled {
            reply(err(
                ReplyEmoteCategory::Unavailable,
                "Volanta-Tracking ist aus",
            ))
            .await;
            return Ok(());
        }
        if let Err(remaining) = self.cooldown.try_acquire(&ctx.privmsg.sender.login).await {
            reply(cooldown_reply(&self.settings, remaining)).await;
            return Ok(());
        }

        let request = parse(&ctx.args);
        let name = match request {
            Request::Overview => {
                let pilots = self.settings.load().volanta.pilots.clone();
                let line = match self.hub.flying(&pilots).await {
                    None => err(
                        ReplyEmoteCategory::Unavailable,
                        "Noch keine Volanta-Daten, gleich nochmal",
                    ),
                    Some(flying) if flying.is_empty() => {
                        "Gerade ist niemand von der Volanta-Liste unterwegs".to_owned()
                    }
                    Some(flying) => {
                        let list: Vec<String> = flying
                            .iter()
                            .map(|(name, f)| match phase(f) {
                                Some(p) => format!("{} ({}, {p})", who(name, f), route(f)),
                                None => format!("{} ({})", who(name, f), route(f)),
                            })
                            .collect();
                        format!("Unterwegs in Volanta: {}", list.join(" · "))
                    }
                };
                reply(line).await;
                return Ok(());
            }
            Request::Usage => {
                reply(err(ReplyEmoteCategory::Invalid, USAGE)).await;
                return Ok(());
            }
            Request::Status(n) | Request::Track(n) | Request::Untrack(n) => n,
        };
        let Some(pilot) = self.listed(name) else {
            reply(err(
                ReplyEmoteCategory::Invalid,
                &format!("{name} ist nicht auf der Volanta-Liste"),
            ))
            .await;
            return Ok(());
        };
        let unresolved = || {
            err(
                ReplyEmoteCategory::Invalid,
                &format!("{pilot} kenne ich bei Volanta noch nicht"),
            )
        };
        let now = self.clock.now_utc();

        let line = match request {
            Request::Status(_) => match self.hub.status(&pilot, now).await {
                PilotStatus::NoData => err(
                    ReplyEmoteCategory::Unavailable,
                    "Noch keine Volanta-Daten, gleich nochmal",
                ),
                PilotStatus::Unresolved => unresolved(),
                PilotStatus::Flying(f) => {
                    live_line(f.network_user_name.as_deref().unwrap_or(&pilot), &f)
                }
                PilotStatus::NoSignal { minutes } => {
                    format!("{pilot}: seit {minutes} min kein Signal")
                }
                PilotStatus::NotFlying => format!("{pilot} fliegt gerade nicht"),
            },
            Request::Track(_) => {
                match self.hub.track(&pilot, &ctx.privmsg.sender.login, now).await {
                    TrackOutcome::Tracking(f) => format!("Verfolge {pilot} ({})", route(&f)),
                    TrackOutcome::Armed => format!(
                        "{pilot} fliegt gerade nicht, ich verfolge den nächsten Flug (24 h)"
                    ),
                    TrackOutcome::AlreadyTracked { by } => err(
                        ReplyEmoteCategory::Invalid,
                        &format!("{pilot} wird schon verfolgt (von {by})"),
                    ),
                    TrackOutcome::Unresolved => unresolved(),
                }
            }
            Request::Untrack(_) => {
                let is_mod = ctx
                    .privmsg
                    .badges
                    .iter()
                    .any(|b| b.name == "moderator" || b.name == "broadcaster");
                match self
                    .hub
                    .untrack(&pilot, &ctx.privmsg.sender.login, is_mod)
                    .await
                {
                    UntrackOutcome::Removed => format!("Tracking von {pilot} beendet"),
                    UntrackOutcome::NotTracked => err(
                        ReplyEmoteCategory::Invalid,
                        &format!("{pilot} wird nicht verfolgt"),
                    ),
                    UntrackOutcome::NotAllowed { by } => err(
                        ReplyEmoteCategory::Denied,
                        &format!("Nur {by} oder Mods können das beenden"),
                    ),
                    UntrackOutcome::Unresolved => unresolved(),
                }
            }
            Request::Overview | Request::Usage => return Ok(()),
        };
        reply(line).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn parses_subcommands_case_insensitively() {
        assert_eq!(parse(&[]), Request::Overview);
        assert_eq!(parse(&args("EChris-135")), Request::Status("EChris-135"));
        assert_eq!(parse(&args("TRACK bob")), Request::Track("bob"));
        assert_eq!(parse(&args("untrack bob")), Request::Untrack("bob"));
        assert_eq!(parse(&args("track")), Request::Usage);
        assert_eq!(parse(&args("a b c")), Request::Usage);
    }

    #[test]
    fn status_line_shows_phase_and_telemetry_on_the_ground_too() {
        let flight: LiveFlight = serde_json::from_value(serde_json::json!({
            "id": "f1", "originIcao": "EDLP", "destinationIcao": "EDDG",
            "aircraftIcao": "C172", "aircraftRegistration": "D-EABC",
            "position": {"altitude": 699.6, "groundSpeed": 12.2, "onGround": true}
        }))
        .unwrap();
        assert_eq!(
            live_line("Chris", &flight),
            "Chris: EDLP → EDDG, am Boden, 700 ft, 12 kt, C172 D-EABC"
        );
    }

    #[test]
    fn status_line_names_the_flight_number_else_the_callsign() {
        let mut flight: LiveFlight = serde_json::from_value(serde_json::json!({
            "id": "f1", "originIcao": "EDLP", "destinationIcao": "EDDG",
            "callsign": "VLG8VJ", "flightNumber": "VY1179",
            "position": {"altitude": 32000, "groundSpeed": 450, "onGround": false}
        }))
        .unwrap();
        assert_eq!(
            live_line("Chris", &flight),
            "Chris (VY1179): EDLP → EDDG, in der Luft, 32000 ft, 450 kt"
        );
        flight.flight_number = None;
        assert!(live_line("Chris", &flight).starts_with("Chris (VLG8VJ): "));
    }
}
