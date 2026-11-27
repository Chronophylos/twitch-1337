//! Scheduled messages.
//!
//! One task: sleep until the earliest next fire of any enabled schedule, a
//! settings change, or shutdown; fire whatever is due; repeat. Every wake
//! reads the current settings, so a dashboard edit applies at once without
//! restarting anything.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use tracing::{Instrument as _, debug, info, info_span, warn};
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::lifecycle::Lifecycle;
use crate::ping::{PingHandle, TriggerDecision};
use crate::schedule::{Schedule, ScheduleRuntimeStore};
use crate::settings::SettingsHandle;
use crate::twitch::ChatSender;
use crate::util::{clock::Clock, to_berlin};

/// What a schedule needs to fire its optional ping: the ping actor, the
/// settings for the default cooldown, and the bot login used as `{sender}`.
#[derive(Clone)]
pub struct SchedulePing {
    pub handle: PingHandle,
    pub settings: SettingsHandle,
    pub bot_login: String,
}

impl SchedulePing {
    async fn fire<T, L>(&self, sender: &ChatSender<T, L>, channel: &str, ping_name: &str)
    where
        T: Transport,
        L: LoginCredentials,
    {
        let cooldown = Duration::from_secs(self.settings.load().pings.cooldown);
        // `public = true`: the bot is not a member of the ping it fires.
        match self
            .handle
            .try_record_trigger(ping_name.to_owned(), self.bot_login.clone(), cooldown, true)
            .await
        {
            TriggerDecision::Fire(lines) => {
                for line in lines {
                    sender.say(channel.to_owned(), line).await;
                }
            }
            TriggerDecision::OnCooldown(remaining) => {
                debug!(ping = %ping_name, ?remaining, "scheduled ping on cooldown, skipped");
            }
            TriggerDecision::Skip => {
                warn!(ping = %ping_name, "scheduled ping missing or has no members, skipped");
            }
        }
    }
}

/// Enabled schedules with their next fire strictly after `after`, soonest
/// first (ties by name, so a shared slot fires in a stable order). A schedule
/// with no further fire (past `end_date`) is left out.
fn upcoming(schedules: &[Schedule], after: Timestamp) -> Vec<(Timestamp, &Schedule)> {
    let after = to_berlin(after);
    let mut out: Vec<_> = schedules
        .iter()
        .filter(|schedule| schedule.enabled)
        .filter_map(|schedule| {
            let at = schedule.next_active_fire(after.clone())?.timestamp();
            Some((at, schedule))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    out
}

/// Run every enabled schedule until shutdown.
///
/// A slot fires at most once, and a missed one is never caught up: a wake
/// fires each schedule whose next slot has passed once, then counts on from
/// that wake. Shutdown lets an in-flight fire finish its `say()`, stops before
/// the next one, and flushes the runtime counters.
pub async fn run_scheduler<T, L>(
    sender: Arc<ChatSender<T, L>>,
    settings: SettingsHandle,
    runtime: Arc<ScheduleRuntimeStore>,
    channel: String,
    clock: Arc<dyn Clock>,
    lifecycle: Lifecycle,
    ping: SchedulePing,
) where
    T: Transport,
    L: LoginCredentials,
{
    info!("Scheduler started");
    let mut changes = settings.subscribe();
    // Slots at or before `cursor` are done: fired, or passed before startup
    // or before the edit that introduced them.
    let mut cursor = clock.now_utc();
    runtime.retain_schedules(&settings.load().schedules).await;

    'run: loop {
        let next = upcoming(&settings.load().schedules, cursor)
            .first()
            .map(|(at, _)| *at);
        let sleep = async {
            match next {
                Some(at) => clock.sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => break 'run,
            () = sleep => {}
            () = changes.next() => {
                // Count from now, so an added or edited schedule never fires
                // for a slot that already passed.
                cursor = clock.now_utc();
                runtime.retain_schedules(&settings.load().schedules).await;
                continue;
            }
        }

        let now = clock.now_utc();
        let snapshot = settings.load();
        for (_, schedule) in upcoming(&snapshot.schedules, cursor)
            .into_iter()
            .take_while(|(at, _)| *at <= now)
        {
            if lifecycle.is_cancelled() {
                break 'run;
            }
            fire(schedule, &sender, &runtime, &channel, clock.as_ref(), &ping).await;
        }
        cursor = now;
    }

    runtime.flush().await;
    info!("Scheduler stopped");
}

/// Send one schedule's message, record the fire, then fire its ping.
async fn fire<T, L>(
    schedule: &Schedule,
    sender: &ChatSender<T, L>,
    runtime: &ScheduleRuntimeStore,
    channel: &str,
    clock: &dyn Clock,
    ping: &SchedulePing,
) where
    T: Transport,
    L: LoginCredentials,
{
    // One root trace per fire; the sleep before it stays outside.
    let span = info_span!(
        parent: None,
        "schedule.fire",
        bot.schedule.name = %schedule.name,
        ping.name = schedule.ping.as_deref(),
    );
    async {
        sender
            .say(channel.to_owned(), schedule.message.clone())
            .await;
        runtime.record_fire(&schedule.name, clock.now_utc()).await;
        if let Some(ping_name) = &schedule.ping {
            ping.fire(sender, channel, ping_name).await;
        }
    }
    .instrument(span)
    .await;
}
