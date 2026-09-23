use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use tracing::{instrument, warn};
use twitch_irc::{login::LoginCredentials, message::PrivmsgMessage, transport::Transport};

use crate::commands::{
    Command, CommandCancellation, CommandContext, CommandId, CommandTaskGroup, ExecutionClass,
};
use crate::twitch::ChatSender;
use crate::util::parse_flight_duration;

pub struct RandomFlightCommand;

#[async_trait]
impl<T, L> Command<T, L> for RandomFlightCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> CommandId {
        CommandId::new("random_flight").expect("command ID is valid")
    }

    fn name(&self) -> &str {
        "!fl"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Background
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        flight_command(
            &ctx.privmsg,
            &ctx.sender,
            ctx.args.first().map(String::as_str),
            ctx.args.get(1).map(String::as_str),
            ctx.cancellation.clone(),
        )
        .await
    }
}

#[instrument(skip(privmsg, sender), fields(user = %privmsg.sender.login))]
pub(crate) async fn flight_command<T, L>(
    privmsg: &PrivmsgMessage,
    sender: &Arc<ChatSender<T, L>>,
    aircraft_code: Option<&str>,
    duration_str: Option<&str>,
    cancellation: CommandCancellation,
) -> Result<()>
where
    T: Transport,
    L: LoginCredentials,
{
    cancellation.check()?;
    const USAGE_MSG: &str = "Gib mir nen Flugzeug und ne Zeit, z.B. !fl A20N 1h FDM";

    let (Some(aircraft_code), Some(duration_str)) = (aircraft_code, duration_str) else {
        cancellation.check()?;
        sender.reply(privmsg, USAGE_MSG).await;
        return Ok(());
    };

    let Some(aircraft) = twitch_1337_data::flight::aircraft_by_icao_type(aircraft_code) else {
        cancellation.check()?;
        sender
            .reply(privmsg, "Das Flugzeug kenn ich nich FDM")
            .await;
        return Ok(());
    };

    let Some(duration) = parse_flight_duration(duration_str) else {
        cancellation.check()?;
        sender.reply(privmsg, USAGE_MSG).await;
        return Ok(());
    };

    // Can take many retries internally. The blocking handle is owned by the
    // manager's task group, so aborting the command parent cannot detach it.
    let generated = if let Some(group) = CommandTaskGroup::current() {
        group
            .spawn_blocking(move || {
                random_flight::generate_flight_plan(&random_flight::FlightRequest {
                    aircraft,
                    block_time: duration,
                    departure: None,
                    direction: None,
                })
            })
            .await
            .map_err(eyre::Report::new)
    } else {
        // The pre-manager inline dispatcher has no task-group scope yet. Keep
        // that path functional until the ingress split installs the manager.
        tokio::task::spawn_blocking(move || {
            random_flight::generate_flight_plan(&random_flight::FlightRequest {
                aircraft,
                block_time: duration,
                departure: None,
                direction: None,
            })
        })
        .await
        .map_err(|error| eyre::Report::new(error))
    };
    let result = generated.wrap_err("Flight plan generation task ended unexpectedly")?;
    cancellation.check()?;

    let fp = match result {
        Ok(fp) => fp,
        Err(random_flight::Error::BlockTimeTooLong { max }) => {
            sender
                .reply(
                    privmsg,
                    &format!(
                        "Mit dem {} schaffste maximal {}, da geht keine Minute mehr FDM",
                        aircraft.icao_type,
                        crate::cooldown::format_duration_hm(max)
                    ),
                )
                .await;
            return Ok(());
        }
        Err(e) => {
            warn!(error = ?e, "Flight plan generation failed");
            sender
                .reply(
                    privmsg,
                    "Hab keine Route gefunden, versuch mal ne andere Zeit FDM",
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
