//! Passive link corrections for Mydealz, Reddit share links and X/Twitter.
//!
//! Viewer messages are scanned for candidate links in order of appearance.
//! Mydealz links are resolved with a HEAD request and tracking parameters are
//! stripped; Reddit `/r/<sub>/s/<id>` share links are resolved to their
//! canonical permalink; public X/Twitter profile and status links are rewritten
//! locally to `xnotme.com`. Everything is live-tunable via
//! `link_correction` settings; disabled kinds never occupy a candidate slot.

use std::{collections::HashSet, sync::Arc, time::Duration};

use reqwest::Client;
use tokio::sync::broadcast;
use tracing::{Instrument as _, debug, error};
use twitch_irc::{
    login::LoginCredentials, message::PrivmsgMessage, message::ServerMessage, transport::Transport,
};
use url::Url;

use crate::{
    APP_USER_AGENT,
    lifecycle::Lifecycle,
    settings::{LinkCorrectionSettings, SettingsHandle},
    spoiler::url::{REDDIT_HOSTS, reddit_post},
    twitch::{ChatSender, echo::EchoGuard, handlers::Inbound},
    util::http::SendTraced as _,
};

/// An unanswered request would block the whole handler loop, so this is not
/// optional.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Mydealz hosts that carry real deal pages. All paths are eligible.
const MYDEALZ_HOSTS: &[&str] = &["mydealz.de", "www.mydealz.de"];

/// Public X/Twitter profile and status hosts. Mirrors, embeds and alternate
/// front-ends are ignored on purpose.
const X_HOSTS: &[&str] = &[
    "x.com",
    "www.x.com",
    "mobile.x.com",
    "twitter.com",
    "www.twitter.com",
    "mobile.twitter.com",
];

/// `x.com/<route>` paths that are not usernames.
const X_RESERVED: &[&str] = &[
    "about",
    "account",
    "accounts",
    "compose",
    "download",
    "explore",
    "follow",
    "followers",
    "following",
    "hashtag",
    "help",
    "home",
    "i",
    "intent",
    "jobs",
    "login",
    "logout",
    "messages",
    "notifications",
    "privacy",
    "search",
    "security",
    "settings",
    "share",
    "signup",
    "tos",
    "who_to_follow",
];

/// Trailing punctuation chat glues onto a link.
const TRAILING: &[char] = &[')', ']', '.', ',', '!', '?', '"', '\''];

/// Opening wrappers around a link (`(https://…)`, `"https://…"`).
const LEADING: &[char] = &['(', '[', '"', '\''];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CorrectionKind {
    Mydealz,
    Reddit,
    XTwitter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    kind: CorrectionKind,
    /// For X this is the already rewritten XNotMe target; for Reddit the
    /// canonicalized `www.reddit.com` share URL without query/fragment; for
    /// Mydealz the source minus fragment with the original query retained
    /// (so `clean_mydealz` can detect tracking-only changes on
    /// non-redirecting links). Mydealz dedupe keys are normalized separately
    /// by `mydealz_key`.
    url: Url,
}

fn is_mydealz(url: &Url) -> bool {
    url.scheme() == "https" && url.host_str().is_some_and(|h| MYDEALZ_HOSTS.contains(&h))
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

/// Mydealz candidate key: fragment gone, tracking stripped, host
/// canonicalized so equivalent links dedupe before the cap.
fn mydealz_key(url: &Url) -> Url {
    let mut key = url.clone();
    key.set_fragment(None);
    // A `mydealz.de` host is always a valid host name, so set_host only
    // fails on a cannot-be-a-base URL, which the accepted sources never are.
    if key.host_str() == Some("mydealz.de")
        && let Err(err) = key.set_host(Some("www.mydealz.de"))
    {
        debug!(%key, %err, "could not canonicalize mydealz host");
    }
    strip_tracking(&mut key);
    key
}

/// Path segments with trailing slashes trimmed. Empty interior segments are
/// invalid: `x.com//Alice` and `/r//rust/s/AbC1` are not canonical links.
fn path_segments(url: &Url) -> Option<Vec<&str>> {
    let mut segments: Vec<&str> = url.path_segments()?.collect();
    while segments.last() == Some(&"") {
        segments.pop();
    }
    (!segments.iter().any(|segment| segment.is_empty())).then_some(segments)
}

/// The X username rule: 1–15 ASCII alphanumerics/underscores, and never a
/// reserved route.
fn is_x_username(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 15
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !X_RESERVED.contains(&segment.to_ascii_lowercase().as_str())
}

/// X path whitelist: `/<user>`, `/<user>/status/<id>`, and either status form
/// plus `/photo/<n>` or `/video/<n>` with any positive decimal index (X
/// raised the photo cap from 4 to 20, and video has the same numbered
/// subpath). Returns the accepted path with trailing slashes trimmed, or
/// `None`.
fn x_path(url: &Url) -> Option<String> {
    let segments = path_segments(url)?;
    let positive_index =
        |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && s != "0";
    let path = match segments.as_slice() {
        [user] if is_x_username(user) => format!("/{user}"),
        [user, "status", id]
            if is_x_username(user) && !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) =>
        {
            format!("/{user}/status/{id}")
        }
        [user, "status", id, "photo", n]
            if is_x_username(user)
                && !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_digit())
                && positive_index(n) =>
        {
            format!("/{user}/status/{id}/photo/{n}")
        }
        [user, "status", id, "video", n]
            if is_x_username(user)
                && !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_digit())
                && positive_index(n) =>
        {
            format!("/{user}/status/{id}/video/{n}")
        }
        _ => return None,
    };
    Some(path)
}

/// Reddit share link: exactly `/r/<sub>/s/<id>` on one of the six canonical
/// Reddit hosts (never `redd.it`). Returns the canonicalized source:
/// `https://www.reddit.com/r/<sub>/s/<id>` with query and fragment dropped.
fn reddit_share(url: &Url) -> Option<Url> {
    let host = url.host_str()?;
    if !REDDIT_HOSTS.contains(&host) {
        return None;
    }
    let segments = path_segments(url)?;
    let [r, sub, s, id] = segments.as_slice() else {
        return None;
    };
    if *r != "r" || *s != "s" || sub.is_empty() || id.is_empty() {
        return None;
    }
    if !sub.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || !id.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return None;
    }
    Url::parse(&format!("https://www.reddit.com/r/{sub}/s/{id}")).ok()
}

/// Mydealz candidate: source minus fragment with query retained (so `clean`
/// can detect tracking-only changes on non-redirecting links).
fn mydealz_candidate(url: &Url) -> Option<Url> {
    let mut c = url.clone();
    c.set_fragment(None);
    if c.set_scheme("https").is_err() {
        return None;
    }
    Some(c)
}

/// One pass over a message's tokens, in order, collecting the first
/// `settings.max_links` enabled, deduplicated candidates.
fn candidates(text: &str, settings: &LinkCorrectionSettings) -> Vec<Candidate> {
    let mut seen: HashSet<Url> = HashSet::new();
    let mut out: Vec<Candidate> = Vec::new();
    for token in text.split_whitespace() {
        if out.len() == settings.max_links {
            break;
        }
        let trimmed = token.trim_start_matches(LEADING).trim_end_matches(TRAILING);
        let lower = trimmed.to_ascii_lowercase();
        let Some(url) = (if lower.starts_with("http://") || lower.starts_with("https://") {
            Url::parse(trimmed).ok()
        } else {
            // Bare links: only when the host itself is a supported source
            // and the token names something below it, otherwise arbitrary
            // bare domains would become URLs.
            let host = lower.split(['/', '?', '#']).next();
            let supported = host.is_some_and(|h| {
                (MYDEALZ_HOSTS.contains(&h) || REDDIT_HOSTS.contains(&h) || X_HOSTS.contains(&h))
                    && lower.contains('/')
            });
            if supported {
                Url::parse(&format!("https://{trimmed}")).ok()
            } else {
                None
            }
        }) else {
            continue;
        };
        let mut url = url;
        if url.scheme() == "http" && url.set_scheme("https").is_err() {
            continue;
        }
        if url.scheme() != "https" {
            continue;
        }
        let Some(host) = url.host_str() else {
            continue;
        };
        // userinfo on a source host is never a viewer link we correct.
        if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
            continue;
        }
        if MYDEALZ_HOSTS.contains(&host) {
            if !settings.mydealz {
                continue;
            }
            let key = mydealz_key(&url);
            if let Some(src) = mydealz_candidate(&url)
                && seen.insert(key)
            {
                out.push(Candidate {
                    kind: CorrectionKind::Mydealz,
                    url: src,
                });
            }
        } else if REDDIT_HOSTS.contains(&host) {
            if !settings.reddit {
                continue;
            }
            if let Some(share) = reddit_share(&url).filter(|s| seen.insert(s.clone())) {
                out.push(Candidate {
                    kind: CorrectionKind::Reddit,
                    url: share,
                });
            }
        } else if X_HOSTS.contains(&host) {
            if !settings.x_twitter {
                continue;
            }
            if let Some(path) = x_path(&url)
                && let Ok(target) = Url::parse(&format!("https://xnotme.com{path}"))
                && seen.insert(target.clone())
            {
                out.push(Candidate {
                    kind: CorrectionKind::XTwitter,
                    url: target,
                });
            }
        }
    }
    out
}

/// Resolve redirects. `None` when the request failed or the response was not
/// a success; the final URL is returned otherwise. Never faults the task.
async fn head(http: &Client, url: &Url) -> Option<Url> {
    match http.head(url.clone()).send_traced_dynamic().await {
        Ok(response) => {
            if response.status().is_success() {
                Some(response.url().clone())
            } else {
                debug!(%url, status = %response.status(), "link did not resolve");
                None
            }
        }
        Err(err) => {
            debug!(%url, ?err, "HEAD request failed");
            None
        }
    }
}

/// Pure Mydealz post-resolution: a recognized Mydealz final host, HTTPS, no
/// fragment, no tracking parameters, and something actually changed versus
/// the normalized source. `None` otherwise.
fn clean_mydealz(source: &Url, resolved: &Url) -> Option<Url> {
    if !is_mydealz(resolved) {
        debug!(%source, %resolved, "mydealz link redirected off-site, not posting");
        return None;
    }
    let mut cleaned = resolved.clone();
    cleaned.set_fragment(None);
    strip_tracking(&mut cleaned);
    (mydealz_key(&cleaned) != mydealz_key(source)).then_some(cleaned)
}

/// Reddit post-resolution: only a canonical post permalink survives. Uses the
/// spoiler URL validator, so login walls, redd.it and non-post paths all
/// return `None`.
fn canonical_reddit(resolved: &Url) -> Option<Url> {
    reddit_post(resolved)?.permalink
}

pub async fn run_link_correction_handler<T, L>(
    mut broadcast_rx: broadcast::Receiver<Inbound>,
    sender: Arc<ChatSender<T, L>>,
    channel: String,
    echo: Arc<EchoGuard>,
    settings: SettingsHandle,
    lifecycle: Lifecycle,
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
        // Optional client construction is an owned task failure, not a reason
        // to park forever under a live supervisor.
        Err(error) => {
            error!(
                ?error,
                "link correction http client build failed, handler exiting"
            );
            return;
        }
    };
    loop {
        let inbound = tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => return,
            result = broadcast_rx.recv() => match result {
                Ok(inbound) => inbound,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    crate::util::observability::metrics::broadcast_lagged("link_correction", skipped);
                    continue;
                }
                Err(_) => return,
            },
        };
        if lifecycle.is_cancelled() {
            return;
        }
        let ServerMessage::Privmsg(p) = &inbound.msg else {
            continue;
        };
        if p.channel_login != channel || echo.is_mouth(p.sender.id.as_str(), &p.sender.login) {
            continue;
        }
        let snapshot = settings.load();
        let found = candidates(&p.message_text, &snapshot.link_correction);
        if found.is_empty() {
            continue;
        }
        handle_message(&http, &sender, p, found, &lifecycle)
            .instrument(inbound.handler_span("link_correction"))
            .await;
    }
}

/// First-occurrence decision for a resolved result. Two different candidates
/// that resolve to the same final URL (two Reddit share links onto one post,
/// equivalent Mydealz redirects) produce exactly one reply. The key matches
/// the candidate-key normalization for Mydealz so a redirect to the source
/// host's canonical form collapses with the source itself.
fn new_result(
    emitted: &mut HashSet<Url>,
    kind: CorrectionKind,
    result: Url,
) -> Option<(&'static str, Url)> {
    let key = match kind {
        CorrectionKind::Mydealz => mydealz_key(&result),
        _ => result.clone(),
    };
    let prefix = match kind {
        CorrectionKind::Mydealz => "🔗",
        CorrectionKind::Reddit => "BRUH 👉",
        CorrectionKind::XTwitter => "🐦",
    };
    emitted.insert(key).then_some((prefix, result))
}

async fn handle_message<T, L>(
    http: &Client,
    sender: &ChatSender<T, L>,
    p: &PrivmsgMessage,
    candidates: Vec<Candidate>,
    lifecycle: &Lifecycle,
) where
    T: Transport,
    L: LoginCredentials,
{
    let mut emitted: HashSet<Url> = HashSet::new();
    for candidate in candidates {
        if lifecycle.is_cancelled() {
            return;
        }
        // The HEAD request is one atomic network operation. If cancellation
        // lands while it is in flight, let it finish and then stop before the
        // externally visible reply or the next link request.
        let result = match candidate.kind {
            CorrectionKind::XTwitter => Some(candidate.url.clone()),
            CorrectionKind::Mydealz => head(http, &candidate.url)
                .await
                .and_then(|resolved| clean_mydealz(&candidate.url, &resolved)),
            CorrectionKind::Reddit => head(http, &candidate.url)
                .await
                .and_then(|resolved| canonical_reddit(&resolved)),
        };
        if lifecycle.is_cancelled() {
            return;
        }
        match result {
            Some(url) => {
                if let Some((prefix, url)) = new_result(&mut emitted, candidate.kind, url) {
                    sender.reply(p, format!("{prefix} {url}")).await;
                }
            }
            None => debug!(url = %candidate.url, "nothing to correct"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> LinkCorrectionSettings {
        LinkCorrectionSettings::default()
    }

    fn kinds(text: &str, s: &LinkCorrectionSettings) -> Vec<(CorrectionKind, String)> {
        candidates(text, s)
            .into_iter()
            .map(|c| (c.kind, c.url.to_string()))
            .collect()
    }

    #[test]
    fn picks_mydealz_bare_and_absolute_links() {
        let s = settings();
        let found = kinds(
            "schau mal https://www.mydealz.de/share-deal-from-app/2834021 und mydealz.de/deals/foo",
            &s,
        );
        assert_eq!(found.len(), 2);
        assert_eq!(
            found[0].1,
            "https://www.mydealz.de/share-deal-from-app/2834021"
        );
        assert_eq!(found[1].1, "https://mydealz.de/deals/foo");
    }

    #[test]
    fn ignores_non_urls_and_lookalike_hosts() {
        let s = settings();
        assert!(kinds("mydealz.de https://evil-mydealz.de/deals/x", &s).is_empty());
        // bare non-source domains are never treated as URLs
        assert!(kinds("example.com/deals/x", &s).is_empty());
    }

    #[test]
    fn trims_punctuation_and_upgrades_http() {
        let s = settings();
        let found = kinds("(https://www.mydealz.de/deals/a).", &s);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, "https://www.mydealz.de/deals/a");

        let found = kinds("http://www.mydealz.de/deals/a", &s);
        assert_eq!(found[0].1, "https://www.mydealz.de/deals/a");
    }

    #[test]
    fn mydealz_fragment_gone_query_kept_in_candidate() {
        let s = settings();
        let found = kinds("https://mydealz.de/deals/a?utm_source=x&page=2#frag", &s);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, "https://mydealz.de/deals/a?utm_source=x&page=2");
    }

    #[test]
    fn reddit_share_links_only() {
        let s = settings();
        let found = kinds("https://old.reddit.com/r/rust/s/AbC1", &s);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, "https://www.reddit.com/r/rust/s/AbC1");

        // redd.it and non-share paths are never candidates
        assert!(kinds("https://redd.it/1abc", &s).is_empty());
        assert!(kinds("https://www.reddit.com/r/rust/comments/1abc/title/", &s).is_empty());
        // trailing slash, extra segment, junk id rejected
        assert!(kinds("https://www.reddit.com/r/rust/s/AbC1/x", &s).is_empty());
        assert!(kinds("https://www.reddit.com/r/rust/s/Ab_C1!", &s).is_empty());
        // empty interior segments are not collapsed into a valid share path
        assert!(kinds("https://www.reddit.com/r//rust/s/AbC1", &s).is_empty());
        assert!(kinds("https://www.reddit.com/r/rust/s//AbC1", &s).is_empty());
        // trailing slash is fine
        assert_eq!(kinds("https://www.reddit.com/r/rust/s/AbC1/", &s).len(), 1);
        // password-only userinfo is credentials too
        assert!(kinds("https://:secret@www.mydealz.de/deals/a", &s).is_empty());
        assert!(kinds("https://user:pw@x.com/Alice", &s).is_empty());
    }

    #[test]
    fn reddit_share_dedupes_across_hosts() {
        let s = settings();
        let found = kinds(
            "https://old.reddit.com/r/rust/s/AbC1 https://np.reddit.com/r/rust/s/AbC1",
            &s,
        );
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn x_accepts_profile_status_photo_video() {
        let s = settings();
        let found = kinds("x.com/Alice", &s);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, "https://xnotme.com/Alice");

        let found = kinds("https://mobile.twitter.com/Bob/status/123/photo/2", &s);
        assert_eq!(found[0].1, "https://xnotme.com/Bob/status/123/photo/2");

        // X raised the photo cap from 4 to 20; any positive index is kept.
        let found = kinds("x.com/Alice/status/123/photo/20", &s);
        assert_eq!(found[0].1, "https://xnotme.com/Alice/status/123/photo/20");

        let found = kinds("twitter.com/Alice/status/123/video/1", &s);
        assert_eq!(found[0].1, "https://xnotme.com/Alice/status/123/video/1");

        let found = kinds("twitter.com/Alice/status/123/video/3", &s);
        assert_eq!(found[0].1, "https://xnotme.com/Alice/status/123/video/3");
    }

    #[test]
    fn x_rejects_reserved_and_invalid() {
        let s = settings();
        for bad in [
            "x.com/home",
            "x.com/search?q=a",
            "x.com/i/flow/login",
            "x.com/Alice/status/123/photo/0",
            "x.com/Alice/status/123/photo/abc",
            "x.com/Alice/status/123/video/",
            "x.com/Alice/status/123/video/0",
            "x.com/Alice/status/abc",
            "x.com//Alice",
            "x.com/Alice//status//123",
            "x.com/thisusernameiswaytoolongforx",
            "fxtwitter.com/Alice",
            "vxtwitter.com/Alice",
            "xnotme.com/Alice",
        ] {
            assert!(kinds(bad, &s).is_empty(), "{bad}");
        }
    }

    #[test]
    fn toggles_disable_each_kind_independently() {
        let mut s = settings();
        s.mydealz = false;
        assert!(kinds("https://www.mydealz.de/deals/a", &s).is_empty());
        assert_eq!(kinds("x.com/Alice", &s).len(), 1);
        s.x_twitter = false;
        assert!(kinds("x.com/Alice", &s).is_empty());
        assert_eq!(kinds("https://www.reddit.com/r/rust/s/AbC1", &s).len(), 1);
        s.reddit = false;
        assert!(kinds("https://www.reddit.com/r/rust/s/AbC1", &s).is_empty());
    }

    #[test]
    fn mixed_input_cap_and_order() {
        let mut s = settings();
        s.max_links = 2;
        let found = kinds(
            "(x.com/Alice/status/123/photo/1), https://twitter.com/Alice/status/123/photo/1 https://www.reddit.com/r/rust/s/AbC,",
            &s,
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, CorrectionKind::XTwitter);
        assert_eq!(found[0].1, "https://xnotme.com/Alice/status/123/photo/1");
        assert_eq!(found[1].0, CorrectionKind::Reddit);
        assert_eq!(found[1].1, "https://www.reddit.com/r/rust/s/AbC");
    }

    #[test]
    fn duplicate_candidates_consume_no_slots() {
        let s = settings();
        let found = kinds("x.com/Alice x.com/Alice x.com/Bob", &s);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].1, "https://xnotme.com/Alice");
        assert_eq!(found[1].1, "https://xnotme.com/Bob");
    }

    #[test]
    fn mydealz_clean_rejects_off_site_and_no_change() {
        let source = Url::parse("https://www.mydealz.de/deals/a?utm_source=x").unwrap();
        // off-site redirect
        let off_site = Url::parse("https://evil.example/steal").unwrap();
        assert!(clean_mydealz(&source, &off_site).is_none());
        // redirect back to the same (tracking-stripped) page = nothing changed
        let same = Url::parse("https://www.mydealz.de/deals/a").unwrap();
        assert!(clean_mydealz(&source, &same).is_none());
        // real change: redirect to a canonical deal page
        let canonical =
            Url::parse("https://mydealz.de/deals/canonical?utm_campaign=z#frag").unwrap();
        let cleaned = clean_mydealz(&source, &canonical).expect("cleaned");
        assert_eq!(cleaned.as_str(), "https://mydealz.de/deals/canonical");

        // bare-host source redirecting to the bare host unchanged: the
        // comparison must be on normalized keys, not raw host forms.
        let bare = Url::parse("https://mydealz.de/deals/a").unwrap();
        assert!(clean_mydealz(&bare, &bare).is_none());
        let www = Url::parse("https://www.mydealz.de/deals/a").unwrap();
        assert!(clean_mydealz(&bare, &www).is_none());
    }

    #[test]
    fn canonical_reddit_validates_via_spoiler() {
        let resolved =
            Url::parse("https://old.reddit.com/r/rust/comments/1abc/title/?utm=1#fragment")
                .unwrap();
        let canonical = canonical_reddit(&resolved).expect("canonical");
        assert_eq!(
            canonical.as_str(),
            "https://www.reddit.com/r/rust/comments/1abc/"
        );

        // login wall and non-post paths are rejected
        assert!(canonical_reddit(&Url::parse("https://www.reddit.com/login").unwrap()).is_none());
        assert!(canonical_reddit(&Url::parse("https://www.reddit.com/r/rust/").unwrap()).is_none());
        // redd.it and no-subreddit targets cannot yield a canonical permalink
        assert!(canonical_reddit(&Url::parse("https://redd.it/1abc").unwrap()).is_none());
        assert!(
            canonical_reddit(&Url::parse("https://www.reddit.com/comments/1abc").unwrap())
                .is_none()
        );
    }

    /// Two distinct share links whose HEADs resolve to the same canonical
    /// post must emit exactly one reply. The wiremock stub pins the shared
    /// redirect target; the `emitted`-set semantics are driven against real
    /// `old.reddit.com` URLs built from the resolved path, so
    /// `canonical_reddit` sees a host it actually validates.
    #[tokio::test]
    async fn two_share_links_resolving_to_one_post_emit_once() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        crate::util::install_crypto_provider();
        let server = MockServer::start().await;
        // The two share endpoints redirect to one final URL; the final URL
        // itself answers 200, so the chain terminates instead of looping.
        let final_url = format!("{}/r/rust/comments/1abc/title/", server.uri());
        for origin in ["/a", "/b"] {
            Mock::given(method("HEAD"))
                .and(path(origin))
                .respond_with(
                    ResponseTemplate::new(302).insert_header("Location", final_url.clone()),
                )
                .mount(&server)
                .await;
        }
        Mock::given(method("HEAD"))
            .and(path("/r/rust/comments/1abc/title/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let http = Client::builder()
            .user_agent(APP_USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap();
        let a = Url::parse(&format!("{}/a", server.uri())).unwrap();
        let b = Url::parse(&format!("{}/b", server.uri())).unwrap();
        let resolved = head(&http, &a).await.expect("redirect chain");
        assert_eq!(head(&http, &b).await.as_ref(), Some(&resolved));
        assert_eq!(resolved.path(), "/r/rust/comments/1abc/title/");

        // Re-home the resolved URL onto a validated Reddit host: the stub
        // cannot answer for reddit.com itself, and `canonical_reddit` is
        // host-strict by design.
        let resolved_reddit =
            Url::parse(&format!("https://old.reddit.com{}", resolved.path())).unwrap();
        let canonical = canonical_reddit(&resolved_reddit).expect("canonical");

        let mut emitted: HashSet<Url> = HashSet::new();
        let mut replies = Vec::new();
        // The same production helper `handle_message` uses, called once per
        // share link: the second call must be suppressed.
        for _ in 0..2 {
            if let Some((prefix, url)) =
                new_result(&mut emitted, CorrectionKind::Reddit, canonical.clone())
            {
                replies.push(format!("{prefix} {url}"));
            }
        }
        assert_eq!(
            replies,
            ["BRUH 👉 https://www.reddit.com/r/rust/comments/1abc/"],
            "one canonical post, one BRUH reply"
        );
    }

    #[tokio::test]
    async fn head_follows_local_redirect() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        crate::util::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/final"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/redirect"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/final", server.uri())),
            )
            .mount(&server)
            .await;

        let http = Client::builder()
            .user_agent(APP_USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap();
        let target = Url::parse(&format!("{}/redirect", server.uri())).unwrap();
        let resolved = head(&http, &target).await.expect("final url");
        assert_eq!(resolved.path(), "/final");
    }

    #[tokio::test]
    async fn head_returns_none_on_404() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        crate::util::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let http = Client::builder()
            .user_agent(APP_USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap();
        let target = Url::parse(&server.uri()).unwrap();
        assert!(head(&http, &target).await.is_none());
    }

    #[tokio::test]
    async fn head_returns_none_on_timeout() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        crate::util::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
            .mount(&server)
            .await;

        let http = Client::builder()
            .user_agent(APP_USER_AGENT)
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let target = Url::parse(&server.uri()).unwrap();
        assert!(head(&http, &target).await.is_none());
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
        let resolved = head(&http, &url).await.expect("resolved");
        let cleaned = clean_mydealz(&url, &resolved).expect("cleaned");
        assert_eq!(cleaned.host_str(), Some("www.mydealz.de"));
        assert!(cleaned.path().starts_with("/deals/"));
        assert!(cleaned.query().is_none());
    }
}
