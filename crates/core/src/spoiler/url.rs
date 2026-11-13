//! URL detection in chat text and the redirect-blind host rewrites that pick
//! a fetchable form (fxtwitter API), plus Reddit post ids for the archive lookup before the first fetch.

use reqwest::Url;

/// Trailing punctuation chat glues onto a link.
const TRAILING: &[char] = &[')', ']', '.', ',', '!', '?', '"', '\''];

/// Opening wrappers around a link (`(https://…)`, `"https://…"`).
const LEADING: &[char] = &['(', '[', '"', '\''];

/// Reddit hosts. All of them wall logged-out bots (JS proof-of-work page,
/// login redirect, or a TLS-fingerprint 403), so posts are never fetched
/// from Reddit itself; see [`reddit_post`].
const REDDIT_HOSTS: &[&str] = &[
    "reddit.com",
    "www.reddit.com",
    "new.reddit.com",
    "np.reddit.com",
    "m.reddit.com",
    "old.reddit.com",
];

const TWITTER_HOSTS: &[&str] = &[
    "x.com",
    "www.x.com",
    "mobile.x.com",
    "twitter.com",
    "www.twitter.com",
    "mobile.twitter.com",
    "fxtwitter.com",
    "www.fxtwitter.com",
    "vxtwitter.com",
    "www.vxtwitter.com",
    "fixupx.com",
    "www.fixupx.com",
];

const FXTWITTER_API: &str = "https://api.fxtwitter.com";

/// What to fetch first for a user-supplied URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Fetch as-is.
    Page(Url),
    /// `api.fxtwitter.com/<user>/status/<id>` JSON.
    Tweet(Url),
}

impl Target {
    pub fn url(&self) -> &Url {
        match self {
            Target::Page(url) | Target::Tweet(url) => url,
        }
    }
}

/// First whitespace-separated token that is an http(s) URL, or a bare link
/// on a known rewrite host (Twitch auto-links `reddit.com/…`), with trailing
/// chat punctuation trimmed.
pub fn find_url(text: &str) -> Option<Url> {
    text.split_whitespace().find_map(token_url)
}

fn token_url(token: &str) -> Option<Url> {
    let token = token.trim_start_matches(LEADING).trim_end_matches(TRAILING);
    let lower = token.to_ascii_lowercase();
    let url = if lower.starts_with("http://") || lower.starts_with("https://") {
        Url::parse(token).ok()?
    } else {
        let host = lower.split(['/', '?', '#']).next()?;
        if !is_known_host(host) {
            return None;
        }
        Url::parse(&format!("https://{token}")).ok()?
    };
    matches!(url.scheme(), "http" | "https").then_some(url)
}

fn is_known_host(host: &str) -> bool {
    REDDIT_HOSTS.contains(&host) || host == "redd.it" || TWITTER_HOSTS.contains(&host)
}

/// String-level rewrite, applied once before the first fetch.
pub fn rewrite(url: &Url) -> Target {
    let host = url.host_str().unwrap_or_default();
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|seg| !seg.is_empty()).collect())
        .unwrap_or_default();

    if TWITTER_HOSTS.contains(&host)
        && let [user, "status", id, ..] = segments.as_slice()
        && let Ok(api) = Url::parse(&format!("{FXTWITTER_API}/{user}/status/{id}"))
    {
        return Target::Tweet(api);
    }

    Target::Page(url.clone())
}

/// A Reddit post named by a link: its base36 id, plus the canonical
/// permalink when the link carried the subreddit (oEmbed needs it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedditPost {
    pub id: String,
    pub permalink: Option<Url>,
}

/// `/r/<sub>/comments/<id>/…` or `/comments/<id>` on a Reddit host, or
/// `redd.it/<id>`. Share links (`/r/<sub>/s/<id>`) are not posts; they are
/// fetched and their redirect target is checked again.
pub fn reddit_post(url: &Url) -> Option<RedditPost> {
    let host = url.host_str()?;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let (id, sub) = match segments.as_slice() {
        ["r", sub, "comments", id, ..] if REDDIT_HOSTS.contains(&host) => (*id, Some(*sub)),
        ["comments", id, ..] if REDDIT_HOSTS.contains(&host) => (*id, None),
        [id] if host == "redd.it" => (*id, None),
        _ => return None,
    };
    let id = id.to_ascii_lowercase();
    if id.is_empty() || id.len() > 13 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let permalink = sub
        .filter(|sub| sub.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        .and_then(|sub| Url::parse(&format!("https://www.reddit.com/r/{sub}/comments/{id}/")).ok());
    Some(RedditPost { id, permalink })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).expect("url")
    }

    #[test]
    fn find_url_trims_trailing_punctuation() {
        assert_eq!(
            find_url("check this: https://x.com/a/status/1)."),
            Some(u("https://x.com/a/status/1"))
        );
        assert_eq!(
            find_url("\"https://example.com/a?b=1\"!?"),
            Some(u("https://example.com/a?b=1"))
        );
    }

    #[test]
    fn find_url_prefixes_bare_known_hosts() {
        assert_eq!(
            find_url("reddit.com/r/gta6/comments/abc/x"),
            Some(u("https://reddit.com/r/gta6/comments/abc/x"))
        );
        assert_eq!(
            find_url("lol www.reddit.com/r/gta6/s/AbC,"),
            Some(u("https://www.reddit.com/r/gta6/s/AbC"))
        );
        assert_eq!(find_url("redd.it/abc"), Some(u("https://redd.it/abc")));
        assert_eq!(
            find_url("x.com/u/status/9"),
            Some(u("https://x.com/u/status/9"))
        );
    }

    #[test]
    fn find_url_rejects_non_urls() {
        assert_eq!(find_url("hi KEKW"), None);
        assert_eq!(find_url("ftp://a.b"), None);
        assert_eq!(find_url("example.com/page"), None);
        assert_eq!(find_url(""), None);
    }

    #[test]
    fn find_url_takes_first_match() {
        assert_eq!(
            find_url("GTA 6 https://a.example/1 https://b.example/2"),
            Some(u("https://a.example/1"))
        );
    }

    #[test]
    fn reddit_post_from_every_host_and_redd_it() {
        for host in REDDIT_HOSTS {
            assert_eq!(
                reddit_post(&u(&format!(
                    "https://{host}/r/GTA6/comments/1XuG/t?share_id=9"
                ))),
                Some(RedditPost {
                    id: "1xug".into(),
                    permalink: Some(u("https://www.reddit.com/r/GTA6/comments/1xug/")),
                }),
                "{host}"
            );
        }
        let bare = Some(RedditPost {
            id: "abc".into(),
            permalink: None,
        });
        assert_eq!(reddit_post(&u("https://redd.it/abc")), bare);
        assert_eq!(reddit_post(&u("https://www.reddit.com/comments/abc")), bare);
    }

    #[test]
    fn reddit_post_rejects_non_posts_and_odd_ids() {
        for link in [
            "https://reddit.com/r/x/s/AbC",
            "https://www.reddit.com/r/x/",
            "https://redd.it/a/b",
            "https://www.reddit.com/r/x/comments/a&b=1",
            "https://www.reddit.com/r/x/comments/aaaaaaaaaaaaaaaaaaaa",
            "https://example.com/r/x/comments/abc",
        ] {
            assert_eq!(reddit_post(&u(link)), None, "{link}");
        }
    }

    #[test]
    fn rewrite_twitter_family_to_fxtwitter_api() {
        assert_eq!(
            rewrite(&u("https://mobile.twitter.com/u/status/9?s=20")),
            Target::Tweet(u("https://api.fxtwitter.com/u/status/9"))
        );
        for host in [
            "x.com",
            "www.x.com",
            "twitter.com",
            "www.twitter.com",
            "fxtwitter.com",
            "vxtwitter.com",
            "fixupx.com",
        ] {
            assert_eq!(
                rewrite(&u(&format!("https://{host}/u/status/9"))),
                Target::Tweet(u("https://api.fxtwitter.com/u/status/9")),
                "{host}"
            );
        }
        let profile = u("https://x.com/u");
        assert_eq!(rewrite(&profile), Target::Page(profile.clone()));
    }

    #[test]
    fn rewrite_leaves_other_hosts_alone() {
        for link in [
            "https://youtube.com/watch?v=1",
            "https://reddit.com/r/x/s/AbC",
        ] {
            assert_eq!(rewrite(&u(link)), Target::Page(u(link)));
        }
    }
}
