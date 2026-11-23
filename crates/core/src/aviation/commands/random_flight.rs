use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use tracing::{debug, instrument};
use twitch_irc::{login::LoginCredentials, message::PrivmsgMessage, transport::Transport};

use crate::commands::{
    Command, CommandCancellation, CommandContext, CommandTaskGroup, ExecutionClass,
    reply_emotes::{ReplyEmoteCategory, error_reply},
};
use crate::settings::SettingsHandle;
use crate::twitch::ChatSender;
use crate::util::parse_flight_duration;

pub struct RandomFlightCommand;

/// Bounds the blocking generator so one pathological request cannot hold an
/// execution slot.
pub(crate) const GENERATION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Outcome of waiting for a blocking generation task under a deadline.
pub(crate) enum Generation<T> {
    Done(T),
    /// The task ended without sending a result (e.g. it panicked).
    Failed(tokio::sync::oneshot::error::RecvError),
    TimedOut,
}

pub(crate) async fn await_generation<T>(
    receiver: tokio::sync::oneshot::Receiver<T>,
    deadline: std::time::Duration,
) -> Generation<T> {
    match tokio::time::timeout(deadline, receiver).await {
        Ok(Ok(value)) => Generation::Done(value),
        Ok(Err(e)) => Generation::Failed(e),
        Err(_) => Generation::TimedOut,
    }
}

#[async_trait]
impl<T, L> Command<T, L> for RandomFlightCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "random_flight"
    }

    fn name(&self) -> &str {
        "!fl"
    }

    fn description(&self) -> &str {
        "Zufälliger Flugplan für Flugzeugtyp und Dauer"
    }

    fn usage(&self) -> &str {
        "!fl <ICAO-Typ> <dauer>"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Background
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        flight_command(
            &ctx.privmsg,
            &ctx.sender,
            &ctx.settings,
            ctx.args.first().map(String::as_str),
            ctx.args.get(1).map(String::as_str),
            ctx.cancellation.clone(),
        )
        .await
    }
}

#[instrument(skip_all, fields(twitch.user.login = %privmsg.sender.login))]
pub(crate) async fn flight_command<T, L>(
    privmsg: &PrivmsgMessage,
    sender: &Arc<ChatSender<T, L>>,
    settings: &SettingsHandle,
    aircraft_code: Option<&str>,
    duration_str: Option<&str>,
    cancellation: CommandCancellation,
) -> Result<()>
where
    T: Transport,
    L: LoginCredentials,
{
    cancellation.check()?;
    const USAGE_MSG: &str = "Gib mir nen Flugzeug und ne Zeit, z.B. !fl A20N 1h";

    let (Some(aircraft_code), Some(duration_str)) = (aircraft_code, duration_str) else {
        cancellation.check()?;
        sender
            .reply(
                privmsg,
                error_reply(settings, ReplyEmoteCategory::Invalid, USAGE_MSG),
            )
            .await;
        return Ok(());
    };

    let Some(aircraft) = twitch_1337_data::flight::aircraft_by_icao_type(aircraft_code) else {
        cancellation.check()?;
        sender
            .reply(
                privmsg,
                error_reply(
                    settings,
                    ReplyEmoteCategory::Invalid,
                    "Das Flugzeug kenn ich nich",
                ),
            )
            .await;
        return Ok(());
    };

    let Some(duration) = parse_flight_duration(duration_str) else {
        cancellation.check()?;
        sender
            .reply(
                privmsg,
                error_reply(settings, ReplyEmoteCategory::Invalid, USAGE_MSG),
            )
            .await;
        return Ok(());
    };

    // Can take many retries internally. Command execution always runs inside
    // the active manager task group, which owns and joins this blocking child.
    let group = CommandTaskGroup::current()
        .ok_or_else(|| eyre::eyre!("flight command has no active command task group"))?;
    let receiver = group.spawn_blocking(move || {
        random_flight::generate_flight_plan(&random_flight::FlightRequest {
            aircraft,
            block_time: duration,
            departure: None,
            direction: None,
        })
    })?;
    let result = match await_generation(receiver, GENERATION_DEADLINE).await {
        Generation::Done(result) => result,
        Generation::Failed(e) => {
            return Err(eyre::Report::new(e))
                .wrap_err("Flight plan generation task ended unexpectedly");
        }
        Generation::TimedOut => {
            debug!("Flight plan generation hit its deadline");
            cancellation.check()?;
            sender
                .reply(
                    privmsg,
                    error_reply(
                        settings,
                        ReplyEmoteCategory::Invalid,
                        "Hab in 10 Sekunden keine Route gefunden, versuch's nochmal",
                    ),
                )
                .await;
            return Ok(());
        }
    };
    cancellation.check()?;

    let fp = match result {
        Ok(fp) => fp,
        Err(random_flight::Error::BlockTimeTooLong { max }) => {
            sender
                .reply(
                    privmsg,
                    error_reply(
                        settings,
                        ReplyEmoteCategory::Invalid,
                        format!(
                            "Mit dem {} schaffste maximal {}, da geht keine Minute mehr",
                            aircraft.icao_type,
                            crate::cooldown::format_duration_hm(max)
                        ),
                    ),
                )
                .await;
            return Ok(());
        }
        Err(e) => {
            debug!(error = ?e, "Flight plan generation failed");
            sender
                .reply(
                    privmsg,
                    error_reply(
                        settings,
                        ReplyEmoteCategory::Invalid,
                        "Hab keine Route gefunden, versuch mal ne andere Zeit",
                    ),
                )
                .await;
            return Ok(());
        }
    };

    let time_str = crate::cooldown::format_duration_hm(fp.block_time);

    cancellation.check()?;
    let response = format!(
        "{} → {} | {:.0} nm | {} | FL{} | {}",
        fp.departure.icao,
        fp.arrival.icao,
        fp.distance_nm,
        time_str,
        fp.cruise_altitude_ft / 100,
        fp.simbrief_url(),
    );

    sender.reply(privmsg, response).await;

    Ok(())
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn silent_sender_times_out() {
        let (_tx, rx) = tokio::sync::oneshot::channel::<u8>();
        let out = await_generation(rx, Duration::from_millis(50)).await;
        assert!(matches!(out, Generation::TimedOut));
    }

    #[tokio::test(start_paused = true)]
    async fn sent_value_is_returned() {
        let (tx, rx) = tokio::sync::oneshot::channel::<u8>();
        tx.send(7).unwrap();
        let out = await_generation(rx, Duration::from_millis(50)).await;
        assert!(matches!(out, Generation::Done(7)));
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_sender_is_failed() {
        let (tx, rx) = tokio::sync::oneshot::channel::<u8>();
        drop(tx);
        let out = await_generation(rx, Duration::from_millis(50)).await;
        assert!(matches!(out, Generation::Failed(_)));
    }
}
