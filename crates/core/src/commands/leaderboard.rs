use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::RwLock;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{Command, CommandContext, ExecutionClass};
pub use crate::twitch::handlers::tracker_1337::PersonalBest;

pub struct LeaderboardCommand {
    leaderboard: Arc<RwLock<HashMap<String, PersonalBest>>>,
}

impl LeaderboardCommand {
    pub fn new(leaderboard: Arc<RwLock<HashMap<String, PersonalBest>>>) -> Self {
        Self { leaderboard }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for LeaderboardCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "leaderboard"
    }

    fn name(&self) -> &str {
        "!lb"
    }

    fn description(&self) -> &str {
        "1337-Leaderboard, Top 5"
    }

    fn usage(&self) -> &str {
        "!lb"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let response = format_top5(&*self.leaderboard.read().await);
        ctx.sender.reply(&ctx.privmsg, response).await;

        Ok(())
    }
}

/// Top 5 by PB: full sentence for #1, `N. name Xms` for the rest.
fn format_top5(leaderboard: &HashMap<String, PersonalBest>) -> String {
    let mut ranked: Vec<_> = leaderboard.iter().collect();
    // Name as tie-break so equal times rank deterministically.
    ranked.sort_by(|(a_name, a), (b_name, b)| a.ms.cmp(&b.ms).then_with(|| a_name.cmp(b_name)));

    let Some(((username, pb), rest)) = ranked.split_first() else {
        return "Noch keine Einträge vorhanden".to_string();
    };

    let date = pb.date.strftime("%d.%m.%Y");
    let mut response = format!(
        "Der schnellste 1337 ist {username} mit {}ms am {date}",
        pb.ms
    );
    for (i, (name, pb)) in rest.iter().take(4).enumerate() {
        response.push_str(&format!(", {}. {name} {}ms", i + 2, pb.ms));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top5_first_full_rest_abbreviated() {
        let date = jiff::civil::date(2026, 1, 15);
        let board: HashMap<_, _> = [
            ("f", 600),
            ("a", 100),
            ("c", 300),
            ("b", 200),
            ("e", 500),
            ("d", 400),
        ]
        .into_iter()
        .map(|(n, ms)| (n.to_string(), PersonalBest { ms, date }))
        .collect();
        assert_eq!(
            format_top5(&board),
            "Der schnellste 1337 ist a mit 100ms am 15.01.2026, 2. b 200ms, 3. c 300ms, 4. d 400ms, 5. e 500ms"
        );
        assert_eq!(
            format_top5(&HashMap::new()),
            "Noch keine Einträge vorhanden"
        );
    }
}
