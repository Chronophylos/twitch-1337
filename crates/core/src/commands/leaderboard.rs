use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::RwLock;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{Command, CommandContext, ExecutionClass};
use crate::twitch::chat_sender::fits_outbound_reply;
pub use crate::twitch::handlers::tracker_1337::PersonalBest;

/// Places `!lb` shows at most.
const TOP_PLACES: usize = 10;
const EMPTY_REPLY: &str = "Noch keine Einträge vorhanden";

/// The one 1337 ranking: best time ascending, login as tie-break so equal
/// times rank deterministically. `!lb`, the public `/1337` page, and the
/// dashboard `/leaderboard` all rank through this so they cannot disagree.
pub fn ranking(leaderboard: &HashMap<String, PersonalBest>) -> Vec<(&str, &PersonalBest)> {
    let mut ranked: Vec<_> = leaderboard
        .iter()
        .map(|(login, pb)| (login.as_str(), pb))
        .collect();
    ranked.sort_by(|(a_name, a), (b_name, b)| a.ms.cmp(&b.ms).then_with(|| a_name.cmp(b_name)));
    ranked
}

pub struct LeaderboardCommand {
    leaderboard: Arc<RwLock<HashMap<String, PersonalBest>>>,
    /// `<public_url>/1337`, `None` without a dashboard.
    page_url: Option<String>,
}

impl LeaderboardCommand {
    pub fn new(
        leaderboard: Arc<RwLock<HashMap<String, PersonalBest>>>,
        public_url: Option<&str>,
    ) -> Self {
        Self {
            leaderboard,
            page_url: public_url.map(|url| format!("{}/1337", url.trim_end_matches('/'))),
        }
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
        "1337-Leaderboard, Top 10"
    }

    fn usage(&self) -> &str {
        "!lb"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Inline
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        let response = format_top(&*self.leaderboard.read().await, self.page_url.as_deref());
        ctx.sender.reply(&ctx.privmsg, response).await;

        Ok(())
    }
}

/// Top 10 by PB: full sentence for #1, `N. name Xms` for the rest, then
/// ` · Alle: <page_url>` with a dashboard. Places are dropped from the end
/// until the reply, including its transport prefix, passes the outbound clamp
/// untouched. If even #1 and the link do not fit, the link is omitted.
fn format_top(leaderboard: &HashMap<String, PersonalBest>, page_url: Option<&str>) -> String {
    let link = page_url
        .map(|url| format!(" · Alle: {url}"))
        .unwrap_or_default();
    let ranked = ranking(leaderboard);
    let Some(((first, pb), rest)) = ranked.split_first() else {
        let reply = format!("{EMPTY_REPLY}{link}");
        if fits_outbound_reply(&reply) {
            return reply;
        }
        tracing::warn!(
            reply_bytes = reply.len(),
            "leaderboard head and link exceed the outbound reply limit; omitting link"
        );
        return EMPTY_REPLY.to_owned();
    };

    let head = format!(
        "Der schnellste 1337 ist {first} mit {}ms am {}",
        pb.ms,
        pb.date.strftime("%d.%m.%Y")
    );
    let places: Vec<String> = rest
        .iter()
        .take(TOP_PLACES - 1)
        .enumerate()
        .map(|(i, (login, pb))| format!(", {}. {login} {}ms", i + 2, pb.ms))
        .collect();

    let mut shown = places.len();
    loop {
        let mut reply = head.clone();
        for place in places.iter().take(shown) {
            reply.push_str(place);
        }
        reply.push_str(&link);
        if fits_outbound_reply(&reply) {
            return reply;
        }
        if shown == 0 {
            tracing::warn!(
                reply_bytes = reply.len(),
                "leaderboard head and link exceed the outbound reply limit; omitting link"
            );
            return head;
        }
        shown -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::twitch::chat_sender::{fits_outbound_reply, sanitize_outbound};

    fn board(entries: &[(&str, u64)]) -> HashMap<String, PersonalBest> {
        let date = jiff::civil::date(2026, 1, 15);
        entries
            .iter()
            .map(|&(login, ms)| (login.to_owned(), PersonalBest { ms, date }))
            .collect()
    }

    /// Twelve entries `p01`..`p12` with 100ms, 200ms, ..., inserted in
    /// reverse so the map order is not the ranking.
    fn twelve() -> HashMap<String, PersonalBest> {
        let entries: Vec<(String, u64)> = (1..=12u64)
            .rev()
            .map(|i| (format!("p{i:02}"), i * 100))
            .collect();
        let refs: Vec<(&str, u64)> = entries.iter().map(|(l, ms)| (l.as_str(), *ms)).collect();
        board(&refs)
    }

    #[test]
    fn twelve_entries_without_dashboard_list_places_one_to_ten() {
        assert_eq!(
            format_top(&twelve(), None),
            "Der schnellste 1337 ist p01 mit 100ms am 15.01.2026, 2. p02 200ms, \
             3. p03 300ms, 4. p04 400ms, 5. p05 500ms, 6. p06 600ms, 7. p07 700ms, \
             8. p08 800ms, 9. p09 900ms, 10. p10 1000ms"
        );
    }

    #[test]
    fn dashboard_appends_the_public_page_link() {
        let cmd = LeaderboardCommand::new(Arc::default(), Some("https://bot.example/"));
        let page_url = cmd.page_url.as_deref();
        assert_eq!(page_url, Some("https://bot.example/1337"));

        let reply = format_top(&twelve(), page_url);
        assert!(
            reply.ends_with(", 10. p10 1000ms · Alle: https://bot.example/1337"),
            "{reply}"
        );
    }

    #[test]
    fn max_length_logins_and_public_url_keep_first_and_link() {
        // Ten maximum-length (25-char) Twitch logins and the longest accepted
        // public URL. Lower places must yield before either #1 or the link.
        let stem = "x".repeat(24);
        let entries: Vec<(String, u64)> = (0..10u64)
            .map(|i| (format!("{stem}{i}"), 9_000 + i))
            .collect();
        let refs: Vec<(&str, u64)> = entries.iter().map(|(l, ms)| (l.as_str(), *ms)).collect();
        let public_url = format!("https://bot.example/{}", "a".repeat(236));
        assert_eq!(public_url.len(), 256);
        let cmd = LeaderboardCommand::new(Arc::default(), Some(&public_url));

        let reply = format_top(&board(&refs), cmd.page_url.as_deref());

        assert!(fits_outbound_reply(&reply), "{} bytes: {reply}", reply.len());
        assert_eq!(sanitize_outbound(&reply), reply, "outbound clamp kicked in");
        assert!(!reply.contains('…'), "{reply}");
        let link = format!(" · Alle: {public_url}/1337");
        assert!(reply.ends_with(&link), "{reply}");
        let first = format!("Der schnellste 1337 ist {stem}0 mit 9000ms");
        assert!(reply.starts_with(&first), "{reply}");
        // Something had to go, and only from the end.
        assert!(!reply.contains(", 10. "), "{reply}");
        let shown = reply.matches(", ").count() + 1;
        for place in 2..=shown {
            assert!(
                reply.contains(&format!(", {place}. ")),
                "{place} missing: {reply}"
            );
        }
        // As many places as fit: one more would exceed the reply budget.
        let next = format!(", {}. {stem}{shown} {}ms", shown + 1, 9_000 + shown);
        let without_link = reply.strip_suffix(&link).expect("link checked above");
        let with_next = format!("{without_link}{next}{link}");
        assert!(!fits_outbound_reply(&with_next), "{with_next}");
    }

    #[test]
    fn oversized_link_is_omitted_instead_of_clamped() {
        let page_url = format!("https://{}/1337", "a".repeat(480));
        let reply = format_top(&board(&[("a", 100), ("b", 200)]), Some(&page_url));
        assert_eq!(reply, "Der schnellste 1337 ist a mit 100ms am 15.01.2026");
        assert!(fits_outbound_reply(&reply));
        assert!(!reply.contains('…'));
        assert_eq!(format_top(&HashMap::new(), Some(&page_url)), EMPTY_REPLY);
    }

    #[test]
    fn fewer_than_ten_entries_show_all() {
        assert_eq!(
            format_top(&board(&[("c", 300), ("a", 100), ("b", 200)]), None),
            "Der schnellste 1337 ist a mit 100ms am 15.01.2026, 2. b 200ms, 3. c 300ms"
        );
    }

    #[test]
    fn empty_board_says_so_with_and_without_link() {
        assert_eq!(
            format_top(&HashMap::new(), None),
            "Noch keine Einträge vorhanden"
        );
        assert_eq!(
            format_top(&HashMap::new(), Some("https://bot.example/1337")),
            "Noch keine Einträge vorhanden · Alle: https://bot.example/1337"
        );
    }

    #[test]
    fn ranking_breaks_ties_by_login() {
        let board = board(&[("bob", 200), ("carol", 100), ("alice", 200)]);
        let logins: Vec<&str> = ranking(&board).into_iter().map(|(l, _)| l).collect();
        assert_eq!(logins, ["carol", "alice", "bob"]);
    }
}
