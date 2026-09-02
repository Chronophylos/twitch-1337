//! Unshorten and de-track mydealz.de links posted in chat.
//!
//! `https://www.mydealz.de/share-deal-from-app/2834021` redirects to the real
//! deal page and tacks `utm_*` onto it. Follow the redirect, drop the query,
//! post the clean URL back.

use std::sync::Arc;

use reqwest::Client;
use tokio::sync::broadcast;
use tracing::debug;
use twitch_irc::{
    login::LoginCredentials, message::PrivmsgMessage, message::ServerMessage, transport::Transport,
};
use url::Url;

use crate::{
    APP_USER_AGENT,
    twitch::{ChatSender, echo::EchoGuard},
};

/// mydealz URLs in `text`, in order of appearance.
fn mydealz_urls(text: &str) -> Vec<Url> {
    text.split_whitespace()
        .filter_map(|w| Url::parse(w).ok())
        .filter(|u| matches!(u.host_str(), Some("mydealz.de" | "www.mydealz.de")))
        .collect()
}

/// Resolve redirects and strip the query. Returns `None` when the result is
/// the same URL the user already posted.
async fn clean(http: &Client, url: &Url) -> Option<Url> {
    // ponytail: whole query dropped, not just utm_* — mydealz deal pages
    // carry nothing else worth keeping.
    let mut resolved = http.head(url.clone()).send().await.ok()?.url().clone();
    resolved.set_query(None);
    resolved.set_fragment(None);
    (&resolved != url).then_some(resolved)
}

pub async fn run_mydealz_handler<T, L>(
    mut broadcast_rx: broadcast::Receiver<ServerMessage>,
    sender: Arc<ChatSender<T, L>>,
    channel: String,
    echo: Arc<EchoGuard>,
) where
    T: Transport,
    L: LoginCredentials,
{
    // mydealz answers 403 without a User-Agent.
    let http = Client::builder()
        .user_agent(APP_USER_AGENT)
        .build()
        .expect("http client build (TLS init) failed");
    loop {
        let msg = match broadcast_rx.recv().await {
            Ok(m) => m,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return,
        };
        let ServerMessage::Privmsg(p) = msg else {
            continue;
        };
        if p.channel_login != channel || echo.is_mouth(p.sender.id.as_str(), &p.sender.login) {
            continue;
        }
        handle_message(&http, &sender, &p).await;
    }
}

async fn handle_message<T, L>(http: &Client, sender: &ChatSender<T, L>, p: &PrivmsgMessage)
where
    T: Transport,
    L: LoginCredentials,
{
    for url in mydealz_urls(&p.message_text) {
        match clean(http, &url).await {
            Some(cleaned) => sender.reply(p, format!("🔗 {cleaned}")).await,
            None => debug!(%url, "mydealz link already clean"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_only_mydealz_links() {
        let found = mydealz_urls(
            "schau mal https://www.mydealz.de/share-deal-from-app/2834021 und https://example.com/x",
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path(), "/share-deal-from-app/2834021");
    }

    /// Hits the live site; `#[ignore]`d so CI stays offline.
    /// Run with `cargo nextest run -E 'test(unshortens_live_share_link)' --run-ignored all`.
    #[tokio::test]
    #[ignore = "network"]
    async fn unshortens_live_share_link() {
        crate::util::install_crypto_provider();
        let url = Url::parse("https://www.mydealz.de/share-deal-from-app/2834021").unwrap();
        let http = Client::builder()
            .user_agent(crate::APP_USER_AGENT)
            .build()
            .unwrap();
        let cleaned = clean(&http, &url).await.expect("resolved");
        assert_eq!(cleaned.host_str(), Some("www.mydealz.de"));
        assert!(cleaned.path().starts_with("/deals/"));
        assert!(cleaned.query().is_none());
    }

    #[test]
    fn ignores_non_urls_and_lookalike_hosts() {
        assert!(mydealz_urls("mydealz.de/deals/foo https://evil-mydealz.de/deals/x").is_empty());
    }
}
