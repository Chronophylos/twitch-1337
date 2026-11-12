//! URL detection in chat text and the redirect-blind host rewrites that pick
//! a fetchable form (old.reddit, fxtwitter API) before the first fetch.

use reqwest::Url;

/// Trailing punctuation chat glues onto a link.
const TRAILING: &[char] = &[')', ']', '.', ',', '!', '?', '"', '\''];

/// Opening wrappers around a link (`(https://…)`, `"https://…"`).
const LEADING: &[char] = &['(', '[', '"', '\''];

/// Reddit hosts that serve the 8 KB JS shell instead of the post.
const REDDIT_SHELL_HOSTS: &[&str] = &[
    "reddit.com",
    "www.reddit.com",
    "new.reddit.com",
    "np.reddit.com",
    "m.reddit.com",
];

const OLD_REDDIT: &str = "old.reddit.com";

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
    /// Fetch as-is (possibly rewritten to old.reddit).
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
    REDDIT_SHELL_HOSTS.contains(&host)
        || host == OLD_REDDIT
        || host == "redd.it"
        || TWITTER_HOSTS.contains(&host)
}

/// String-level rewrite, applied once before the first fetch.
pub fn rewrite(url: &Url) -> Target {
    let host = url.host_str().unwrap_or_default();
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|seg| !seg.is_empty()).collect())
        .unwrap_or_default();

    if REDDIT_SHELL_HOSTS.contains(&host)
        && let ["r", _, "comments", ..] = segments.as_slice()
        && let Some(old) = old_reddit(url.path())
    {
        return Target::Page(old);
    }

    if host == "redd.it"
        && let [id] = segments.as_slice()
        && let Some(old) = old_reddit(&format!("/comments/{id}"))
    {
        return Target::Page(old);
    }

    if TWITTER_HOSTS.contains(&host)
        && let [user, "status", id, ..] = segments.as_slice()
        && let Ok(api) = Url::parse(&format!("{FXTWITTER_API}/{user}/status/{id}"))
    {
        return Target::Tweet(api);
    }

    Target::Page(url.clone())
}

fn old_reddit(path: &str) -> Option<Url> {
    Url::parse(&format!("https://{OLD_REDDIT}{path}")).ok()
}

/// After the page fetch: a share link that redirected to a shell host gets
/// one refetch on old.reddit. `None` when no refetch is needed.
pub fn needs_old_reddit_refetch(final_url: &Url) -> Option<Url> {
    let host = final_url.host_str()?;
    if !REDDIT_SHELL_HOSTS.contains(&host) {
        return None;
    }
    old_reddit(final_url.path())
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
    fn rewrite_reddit_comment_hosts_to_old() {
        for host in [
            "reddit.com",
            "www.reddit.com",
            "new.reddit.com",
            "np.reddit.com",
            "m.reddit.com",
        ] {
            assert_eq!(
                rewrite(&u(&format!("https://{host}/r/x/comments/1/t"))),
                Target::Page(u("https://old.reddit.com/r/x/comments/1/t")),
                "{host}"
            );
        }
    }

    #[test]
    fn rewrite_leaves_reddit_share_link_alone() {
        let share = u("https://reddit.com/r/x/s/AbC");
        assert_eq!(rewrite(&share), Target::Page(share.clone()));
    }

    #[test]
    fn rewrite_redd_it_to_old_comments() {
        assert_eq!(
            rewrite(&u("https://redd.it/abc")),
            Target::Page(u("https://old.reddit.com/comments/abc"))
        );
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
        let yt = u("https://youtube.com/watch?v=1");
        assert_eq!(rewrite(&yt), Target::Page(yt.clone()));
        let old = u("https://old.reddit.com/r/x/comments/1");
        assert_eq!(rewrite(&old), Target::Page(old.clone()));
    }

    #[test]
    fn share_link_redirect_target_gets_old_reddit_refetch() {
        assert_eq!(
            needs_old_reddit_refetch(&u("https://www.reddit.com/r/x/comments/1")),
            Some(u("https://old.reddit.com/r/x/comments/1"))
        );
        assert_eq!(
            needs_old_reddit_refetch(&u("https://old.reddit.com/r/x/comments/1")),
            None
        );
        assert_eq!(
            needs_old_reddit_refetch(&u("https://youtube.com/watch?v=1")),
            None
        );
    }
}
