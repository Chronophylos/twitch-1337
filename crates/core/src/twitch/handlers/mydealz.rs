//! Unshorten and de-track mydealz.de links posted in chat.
//!
//! `https://www.mydealz.de/share-deal-from-app/2834021` redirects to the real
//! deal page and tacks `utm_*` onto it. Follow the redirect, drop the tracking
//! params, post the clean URL back.

use std::{sync::Arc, time::Duration};

use reqwest::Client;
use tokio::sync::broadcast;
use tracing::{debug, error};
use twitch_irc::{
    login::LoginCredentials, message::PrivmsgMessage, message::ServerMessage, transport::Transport,
};
use url::Url;

use crate::{
    APP_USER_AGENT,
    twitch::{ChatSender, echo::EchoGuard},
};

/// One viewer message can name many links; only the first few are worth a
/// round trip and a reply. Keeps a spammed message from tripping Twitch's
/// 20-per-30s send limit.
const MAX_LINKS_PER_MESSAGE: usize = 2;

/// An unanswered request would block the whole handler loop, so this is not
/// optional.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

fn is_mydealz(url: &Url) -> bool {
    url.scheme() == "https" && matches!(url.host_str(), Some("mydealz.de" | "www.mydealz.de"))
}

/// Distinct mydealz URLs in `text`, in order of appearance, capped at
/// [`MAX_LINKS_PER_MESSAGE`].
fn mydealz_urls(text: &str) -> Vec<Url> {
    let mut out: Vec<Url> = Vec::new();
    for url in text
        .split_whitespace()
        .filter_map(|w| Url::parse(w).ok())
        .filter(is_mydealz)
    {
        if !out.contains(&url) {
            out.push(url);
        }
        if out.len() == MAX_LINKS_PER_MESSAGE {
            break;
        }
    }
    out
}

/// Drop `utm_*` params, keep everything else (a `?q=` search stays a search).
fn strip_tracking(url: &mut Url) {
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !k.starts_with("utm_"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(kept);
    }
}

/// Resolve redirects and strip tracking params. `None` when the request
/// failed, the response was not a success, the chain left mydealz, or the
/// result is the URL the user already posted.
async fn clean(http: &Client, url: &Url) -> Option<Url> {
    let response = http.head(url.clone()).send().await.ok()?;
    if !response.status().is_success() {
        debug!(%url, status = %response.status(), "mydealz link did not resolve");
        return None;
    }
    let mut resolved = response.url().clone();
    // Redirects are followed cross-host by default and mydealz has outbound
    // redirectors, so the destination is viewer-controlled until checked.
    if !is_mydealz(&resolved) {
        debug!(%url, %resolved, "mydealz link redirected off-site, not posting");
        return None;
    }
    strip_tracking(&mut resolved);
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
    let http = match Client::builder()
        .user_agent(APP_USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        // Park instead of returning: an exiting handle trips the bot's fault
        // shutdown, and this feature is not worth the bot.
        Err(error) => {
            error!(?error, "mydealz http client build failed, handler disabled");
            std::future::pending::<()>().await;
            return;
        }
    };
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
            None => debug!(%url, "nothing to clean"),
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

    #[test]
    fn ignores_non_urls_and_lookalike_hosts() {
        assert!(mydealz_urls("mydealz.de/deals/foo https://evil-mydealz.de/deals/x").is_empty());
        assert!(!is_mydealz(
            &Url::parse("http://www.mydealz.de/deals/x").unwrap()
        ));
        assert!(!is_mydealz(
            &Url::parse("https://attacker.example/x").unwrap()
        ));
    }

    #[test]
    fn dedupes_and_caps_links() {
        let one = "https://www.mydealz.de/deals/a";
        let text = format!("{one} {one} https://www.mydealz.de/deals/b https://mydealz.de/deals/c");
        let found = mydealz_urls(&text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].path(), "/deals/a");
        assert_eq!(found[1].path(), "/deals/b");
    }

    #[test]
    fn strips_utm_but_keeps_real_params() {
        let mut url =
            Url::parse("https://www.mydealz.de/search?q=lego&utm_source=mappshare&page=2").unwrap();
        strip_tracking(&mut url);
        assert_eq!(url.as_str(), "https://www.mydealz.de/search?q=lego&page=2");

        let mut only_tracking =
            Url::parse("https://www.mydealz.de/deals/x?utm_source=a&utm_campaign=b").unwrap();
        strip_tracking(&mut only_tracking);
        assert_eq!(only_tracking.as_str(), "https://www.mydealz.de/deals/x");
    }

    /// Hits the live site; `#[ignore]`d so CI stays offline.
    /// Run with `cargo nextest run -E 'test(unshortens_live_share_link)' --run-ignored all`.
    #[tokio::test]
    #[ignore = "network"]
    async fn unshortens_live_share_link() {
        crate::util::install_crypto_provider();
        let url = Url::parse("https://www.mydealz.de/share-deal-from-app/2834021").unwrap();
        let http = Client::builder()
            .user_agent(APP_USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap();
        let cleaned = clean(&http, &url).await.expect("resolved");
        assert_eq!(cleaned.host_str(), Some("www.mydealz.de"));
        assert!(cleaned.path().starts_with("/deals/"));
        assert!(cleaned.query().is_none());
    }
}
