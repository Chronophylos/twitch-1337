//! Fetch a linked page and turn it into model input: capped text plus at
//! most one inline image.

use std::future::Future;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use eyre::Result;
use reqwest::Url;
use scraper::{Html, Selector};
use serde::Deserialize;
use tracing::debug;

use crate::ai::content::client::{RawFetch, extract_readable_text, fetch_raw};
use crate::spoiler::url::{RedditPost, Target, reddit_post, rewrite};

pub const PAGE_MAX_BYTES: u64 = 3 * 1024 * 1024;
pub const PAGE_TIMEOUT: Duration = Duration::from_secs(8);
pub const IMAGE_MAX_BYTES: u64 = 4 * 1024 * 1024;
pub const IMAGE_TIMEOUT: Duration = Duration::from_secs(4);
pub const TEXT_CAP_BYTES: usize = 24 * 1024;

/// Community Reddit archive (Arctic Shift): keyless, ingests new posts within
/// seconds, returns Reddit's own post JSON. Reddit itself walls bots.
const ARCTIC_SHIFT_POSTS: &str = "https://arctic-shift.photon-reddit.com/api/posts/ids";
/// Title-only fallback when the archive is down or lacks the post.
const REDDIT_OEMBED: &str = "https://www.reddit.com/oembed";

/// What the decision call sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub text: String,
    /// `data:image/{png,jpeg,webp};base64,…`
    pub image_data_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GatherError {
    /// Fetch failed, blocked host, unsupported type, or nothing readable.
    #[error("link not readable")]
    Unreadable,
}

/// Rewrite, fetch, extract. Reddit posts go to the archive instead; a share
/// link gets one fetch to learn its redirect target first.
pub async fn gather(http: &reqwest::Client, url: &Url) -> Result<Input, GatherError> {
    gather_with(
        |u, max, timeout| async move { Ok(fetch_raw(http, u, max, timeout).await?) },
        url,
    )
    .await
}

async fn gather_with<F, Fut>(fetch: F, url: &Url) -> Result<Input, GatherError>
where
    F: Fn(Url, u64, Duration) -> Fut,
    Fut: Future<Output = Result<RawFetch>>,
{
    let started = Instant::now();
    let (text, image) = if let Some(post) = reddit_post(url) {
        reddit_text(&fetch, &post, PAGE_TIMEOUT).await?
    } else {
        let target = rewrite(url);
        let page = fetch(target.url().clone(), PAGE_MAX_BYTES, PAGE_TIMEOUT)
            .await
            .map_err(unreadable)?;
        match reddit_post(&page.final_url) {
            Some(post) if matches!(target, Target::Page(_)) => {
                let remaining = PAGE_TIMEOUT.saturating_sub(started.elapsed());
                reddit_text(&fetch, &post, remaining).await?
            }
            _ => page_text(page)?,
        }
    };
    let image_data_url = match image {
        ImageSource::Bytes(bytes) => data_url(&bytes),
        ImageSource::Url(Some(image_url)) => {
            match fetch(image_url, IMAGE_MAX_BYTES, IMAGE_TIMEOUT).await {
                Ok(raw) => data_url(&raw.bytes),
                Err(error) => {
                    debug!(?error, "spoiler image fetch failed, continuing with text");
                    None
                }
            }
        }
        ImageSource::Url(None) => None,
    };

    let text = cap_text(text);
    if text.trim().is_empty() && image_data_url.is_none() {
        return Err(unreadable(eyre::eyre!("nothing readable")));
    }

    Ok(Input {
        text,
        image_data_url,
    })
}

fn page_text(page: RawFetch) -> Result<(String, ImageSource), GatherError> {
    let media_type = media_type(&page.content_type);
    if media_type == "text/html" || media_type == "application/xhtml+xml" {
        let ex = extract_html(&String::from_utf8_lossy(&page.bytes), &page.final_url);
        Ok((ex.text, ImageSource::Url(ex.image_url)))
    } else if media_type == "application/json" {
        let ex = extract_tweet(&page.bytes)
            .ok_or_else(|| unreadable(eyre::eyre!("JSON is not an fxtwitter tweet")))?;
        Ok((ex.text, ImageSource::Url(ex.image_url)))
    } else if media_type.starts_with("image/") {
        Ok((String::new(), ImageSource::Bytes(page.bytes)))
    } else {
        Err(unreadable(eyre::eyre!(
            "unsupported content type {media_type:?}"
        )))
    }
}

/// The post from the archive, else its oEmbed title (needs the permalink).
async fn reddit_text<F, Fut>(
    fetch: &F,
    post: &RedditPost,
    budget: Duration,
) -> Result<(String, ImageSource), GatherError>
where
    F: Fn(Url, u64, Duration) -> Fut,
    Fut: Future<Output = Result<RawFetch>>,
{
    let started = Instant::now();
    let archive = Url::parse_with_params(ARCTIC_SHIFT_POSTS, [("ids", post.id.as_str())])
        .expect("valid archive URL");
    match fetch(archive, PAGE_MAX_BYTES, budget).await {
        Ok(raw) => match extract_reddit_post(&raw.bytes) {
            Some(ex) => return Ok((ex.text, ImageSource::Url(ex.image_url))),
            None => debug!(id = %post.id, "reddit post not in archive"),
        },
        Err(error) => debug!(?error, id = %post.id, "reddit archive fetch failed"),
    }

    let Some(permalink) = &post.permalink else {
        return Err(unreadable(eyre::eyre!(
            "no archive copy and no permalink for oEmbed"
        )));
    };
    let remaining = budget.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(unreadable(eyre::eyre!("page budget spent before oEmbed")));
    }
    let oembed = Url::parse_with_params(REDDIT_OEMBED, [("url", permalink.as_str())])
        .expect("valid oEmbed URL");
    let raw = fetch(oembed, PAGE_MAX_BYTES, remaining)
        .await
        .map_err(unreadable)?;
    let title = serde_json::from_slice::<OEmbed>(&raw.bytes)
        .map_err(|e| unreadable(e.into()))?
        .title;
    Ok((title, ImageSource::Url(None)))
}

#[derive(Deserialize)]
struct OEmbed {
    #[serde(default)]
    title: String,
}

#[derive(Deserialize)]
struct ArchiveResponse {
    #[serde(default)]
    data: Vec<ArchivePost>,
}

#[derive(Deserialize)]
struct ArchivePost {
    #[serde(default)]
    subreddit: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    selftext: String,
    link_flair_text: Option<String>,
    #[serde(default)]
    spoiler: bool,
    #[serde(default)]
    is_self: bool,
    url: Option<String>,
    preview: Option<ArchivePreview>,
}

#[derive(Deserialize)]
struct ArchivePreview {
    #[serde(default)]
    images: Vec<ArchivePreviewImage>,
}

#[derive(Deserialize)]
struct ArchivePreviewImage {
    source: ArchiveImageSource,
}

#[derive(Deserialize)]
struct ArchiveImageSource {
    url: String,
}

/// `r/<sub> · <flair>`, title, spoiler mark, body, link; first preview image.
fn extract_reddit_post(bytes: &[u8]) -> Option<Extracted> {
    let post = serde_json::from_slice::<ArchiveResponse>(bytes)
        .ok()?
        .data
        .into_iter()
        .next()?;
    let mut head = format!("r/{}", post.subreddit);
    if let Some(flair) = post.link_flair_text.filter(|f| !f.trim().is_empty()) {
        head.push_str(" · ");
        head.push_str(flair.trim());
    }
    let mut parts = vec![head, post.title];
    if post.spoiler {
        parts.push("(Reddit: the poster marked this post as a spoiler)".into());
    }
    parts.push(post.selftext);
    if !post.is_self
        && let Some(link) = post.url
    {
        parts.push(format!("Link: {link}"));
    }
    // Older archive rows keep Reddit's HTML-escaped `&amp;` in preview URLs.
    let image_url = post
        .preview
        .and_then(|p| p.images.into_iter().next())
        .and_then(|i| Url::parse(&i.source.url.replace("&amp;", "&")).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"));
    let text = parts
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Some(Extracted { text, image_url })
}

enum ImageSource {
    Url(Option<Url>),
    Bytes(Vec<u8>),
}

fn unreadable(error: eyre::Report) -> GatherError {
    debug!(?error, "spoiler link unreadable");
    GatherError::Unreadable
}

fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

struct Extracted {
    text: String,
    image_url: Option<Url>,
}

/// `og:title\nog:description\n\nreadable text`, empty parts skipped.
fn extract_html(html: &str, final_url: &Url) -> Extracted {
    let doc = Html::parse_document(html);
    let og = |prop: &str| -> Option<String> {
        let sel = Selector::parse(&format!(
            r#"meta[property="og:{prop}"], meta[name="og:{prop}"]"#
        ))
        .ok()?;
        doc.select(&sel)
            .filter_map(|el| el.value().attr("content"))
            .map(str::trim)
            .find(|c| !c.is_empty())
            .map(str::to_string)
    };

    let head = [og("title"), og("description")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
    let image_url = og("image")
        .and_then(|raw| final_url.join(&raw).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"));
    let body = extract_readable_text(html);

    let text = match (head.is_empty(), body.is_empty()) {
        (false, false) => format!("{head}\n\n{body}"),
        (false, true) => head,
        (true, _) => body,
    };
    Extracted { text, image_url }
}

#[derive(Deserialize)]
struct FxResponse {
    tweet: Option<FxTweet>,
}

#[derive(Deserialize)]
struct FxTweet {
    #[serde(default)]
    text: String,
    media: Option<FxMedia>,
}

#[derive(Deserialize)]
struct FxMedia {
    #[serde(default)]
    all: Vec<FxMediaItem>,
}

#[derive(Deserialize)]
struct FxMediaItem {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    url: String,
}

/// `tweet.text` plus the first `photo` in `tweet.media.all`.
fn extract_tweet(bytes: &[u8]) -> Option<Extracted> {
    let tweet = serde_json::from_slice::<FxResponse>(bytes).ok()?.tweet?;
    let image_url = tweet
        .media
        .into_iter()
        .flat_map(|m| m.all)
        .find(|item| item.kind == "photo")
        .and_then(|item| Url::parse(&item.url).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"));
    Some(Extracted {
        text: tweet.text,
        image_url,
    })
}

fn cap_text(mut text: String) -> String {
    if text.len() > TEXT_CAP_BYTES {
        let cut = text.floor_char_boundary(TEXT_CAP_BYTES);
        text.truncate(cut);
    }
    text
}

/// PNG, JPEG or WebP by magic bytes; the server's header is not trusted.
fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn data_url(bytes: &[u8]) -> Option<String> {
    let Some(mime) = image_mime(bytes) else {
        debug!("spoiler image dropped: not PNG/JPEG/WebP");
        return None;
    };
    Some(format!("data:{mime};base64,{}", BASE64.encode(bytes)))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::ai::content::client::{fetch_client, ssrf_bypass_for_tests};

    const PNG: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F'];
    const WEBP: &[u8] = b"RIFF\x00\x00\x00\x00WEBPVP8 ";
    const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00";

    fn u(s: &str) -> Url {
        Url::parse(s).expect("url")
    }

    #[test]
    fn html_og_tags_and_readable_text() {
        let html = r#"<html><head>
            <meta property="og:title" content="GTA 6 Trailer 3">
            <meta property="og:description" content="Rockstar drops it">
            <meta property="og:image" content="/i.jpg">
            </head><body><article><p>Body text here.</p></article></body></html>"#;
        let ex = extract_html(html, &u("https://example.com/news/post"));
        assert_eq!(
            ex.text,
            "GTA 6 Trailer 3\nRockstar drops it\n\nBody text here."
        );
        assert_eq!(ex.image_url, Some(u("https://example.com/i.jpg")));
    }

    #[test]
    fn html_absolute_og_image_kept_and_non_http_dropped() {
        let abs = r#"<meta property="og:image" content="https://cdn.example/a.png"><p>x</p>"#;
        assert_eq!(
            extract_html(abs, &u("https://example.com/")).image_url,
            Some(u("https://cdn.example/a.png"))
        );
        let data = r#"<meta property="og:image" content="data:image/png;base64,AA=="><p>x</p>"#;
        assert_eq!(
            extract_html(data, &u("https://example.com/")).image_url,
            None
        );
    }

    #[test]
    fn html_without_og_is_just_readable_text() {
        let ex = extract_html(
            "<html><body><p>Only body.</p></body></html>",
            &u("https://example.com/"),
        );
        assert_eq!(ex.text, "Only body.");
        assert_eq!(ex.image_url, None);
    }

    #[test]
    fn tweet_json_text_and_first_photo() {
        let json = serde_json::json!({
            "code": 200,
            "tweet": {
                "text": "GTA 6 map leak",
                "media": {"all": [
                    {"type": "video", "url": "https://video.example/v.mp4"},
                    {"type": "photo", "url": "https://pbs.example/1.jpg"},
                    {"type": "photo", "url": "https://pbs.example/2.jpg"}
                ]}
            }
        });
        let ex = extract_tweet(json.to_string().as_bytes()).expect("tweet");
        assert_eq!(ex.text, "GTA 6 map leak");
        assert_eq!(ex.image_url, Some(u("https://pbs.example/1.jpg")));
    }

    #[test]
    fn tweet_json_without_media_or_tweet() {
        let no_media = serde_json::json!({"code": 200, "tweet": {"text": "hi"}});
        let ex = extract_tweet(no_media.to_string().as_bytes()).expect("tweet");
        assert_eq!(ex.text, "hi");
        assert_eq!(ex.image_url, None);
        let not_found = serde_json::json!({"code": 404, "message": "NOT_FOUND"});
        assert!(extract_tweet(not_found.to_string().as_bytes()).is_none());
        assert!(extract_tweet(b"not json").is_none());
    }

    #[test]
    fn text_cap_cuts_at_char_boundary() {
        let s = "ä".repeat(15_000); // 30 000 bytes
        let capped = cap_text(s);
        assert!(capped.len() <= TEXT_CAP_BYTES);
        assert!(capped.len() > TEXT_CAP_BYTES - 4);
        assert!(capped.chars().all(|c| c == 'ä'));

        let emoji = format!("a{}", "🎮".repeat(10_000));
        let capped = cap_text(emoji);
        assert!(capped.len() <= TEXT_CAP_BYTES);

        let cjk = "漢".repeat(10_000);
        assert!(cap_text(cjk).len() <= TEXT_CAP_BYTES);

        assert_eq!(cap_text("short".to_string()), "short");
    }

    #[test]
    fn image_magic_bytes() {
        assert_eq!(image_mime(PNG), Some("image/png"));
        assert_eq!(image_mime(JPEG), Some("image/jpeg"));
        assert_eq!(image_mime(WEBP), Some("image/webp"));
        assert_eq!(image_mime(GIF), None);
        assert_eq!(image_mime(b""), None);
        assert_eq!(image_mime(b"RIFF\x00\x00\x00\x00WAVE"), None);
    }

    fn raw(final_url: &str, content_type: &str, body: &[u8]) -> RawFetch {
        RawFetch {
            final_url: u(final_url),
            content_type: content_type.to_string(),
            bytes: body.to_vec(),
        }
    }

    fn archive_json() -> Vec<u8> {
        serde_json::json!({"data": [{
            "subreddit": "GamingLeaksAndRumours",
            "title": "GTA VI rating drama",
            "selftext": "Regulators postponed twice.",
            "link_flair_text": "Rumour",
            "spoiler": true,
            "is_self": false,
            "url": "https://insider-gaming.example/gta",
            "preview": {"images": [{"source": {"url": "https://external-preview.redd.it/a.jpeg?width=1&amp;s=x"}}]}
        }]})
        .to_string()
        .into_bytes()
    }

    #[test]
    fn reddit_archive_post_text_and_preview() {
        let ex = extract_reddit_post(&archive_json()).expect("post");
        assert_eq!(
            ex.text,
            "r/GamingLeaksAndRumours · Rumour\nGTA VI rating drama\n\
             (Reddit: the poster marked this post as a spoiler)\n\
             Regulators postponed twice.\nLink: https://insider-gaming.example/gta"
        );
        assert_eq!(
            ex.image_url,
            Some(u("https://external-preview.redd.it/a.jpeg?width=1&s=x"))
        );
        assert!(extract_reddit_post(br#"{"data":[]}"#).is_none());
        assert!(extract_reddit_post(b"<html>").is_none());
    }

    type Seen = Arc<Mutex<Vec<String>>>;

    /// A fake fetch answering via `respond` and logging every requested URL.
    fn recorder(
        respond: impl Fn(&Url) -> Result<RawFetch> + Sync,
    ) -> (
        Seen,
        impl Fn(Url, u64, Duration) -> std::future::Ready<Result<RawFetch>>,
    ) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let fetch = move |url: Url, _max: u64, _t: Duration| {
            log.lock().expect("lock").push(url.to_string());
            std::future::ready(respond(&url))
        };
        (seen, fetch)
    }

    #[tokio::test]
    async fn reddit_post_link_reads_archive_never_reddit() {
        let (seen, fetch) = recorder(|url| {
            Ok(raw(url.as_str(), "application/json", &archive_json())).map(|mut r| {
                if url.host_str() == Some("external-preview.redd.it") {
                    r.content_type = "image/png".into();
                    r.bytes = PNG.to_vec();
                }
                r
            })
        });
        let input = gather_with(
            fetch,
            &u("https://old.reddit.com/r/GamingLeaksAndRumours/comments/1xugesq/slug/"),
        )
        .await
        .expect("readable");
        assert!(input.text.contains("Regulators postponed twice."));
        assert!(input.image_data_url.is_some());
        assert_eq!(
            *seen.lock().expect("lock"),
            [
                "https://arctic-shift.photon-reddit.com/api/posts/ids?ids=1xugesq",
                "https://external-preview.redd.it/a.jpeg?width=1&s=x",
            ]
        );
    }

    #[tokio::test]
    async fn reddit_falls_back_to_oembed_title() {
        let (seen, fetch) = recorder(|url| {
            if url.host_str() == Some("www.reddit.com") {
                Ok(raw(
                    url.as_str(),
                    "application/json",
                    br#"{"title":"GTA VI ending leaked","type":"rich"}"#,
                ))
            } else {
                Ok(raw(url.as_str(), "application/json", br#"{"data":[]}"#))
            }
        });
        let input = gather_with(fetch, &u("https://www.reddit.com/r/GTA6/comments/abc/x"))
            .await
            .expect("readable");
        assert_eq!(input.text, "GTA VI ending leaked");
        assert_eq!(
            seen.lock().expect("lock")[1],
            "https://www.reddit.com/oembed?url=https%3A%2F%2Fwww.reddit.com%2Fr%2FGTA6%2Fcomments%2Fabc%2F"
        );
    }

    #[tokio::test]
    async fn redd_it_without_archive_copy_is_unreadable() {
        let (seen, fetch) = recorder(|_| Err(eyre::eyre!("archive down")));
        assert_eq!(
            gather_with(fetch, &u("https://redd.it/abc")).await,
            Err(GatherError::Unreadable)
        );
        assert_eq!(seen.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn share_link_resolved_by_redirect_then_archive() {
        let (seen, fetch) = recorder(|url| {
            if url.path().contains("/s/") {
                // Reddit redirects the share link, then walls the post page.
                Ok(raw(
                    "https://www.reddit.com/r/x/comments/abc/t/",
                    "text/html",
                    b"<p>proof of work</p>",
                ))
            } else {
                Ok(raw(url.as_str(), "application/json", &archive_json()))
            }
        });
        let input = gather_with(fetch, &u("https://reddit.com/r/x/s/AbC"))
            .await
            .expect("readable");
        assert!(input.text.contains("GTA VI rating drama"));
        let seen = seen.lock().expect("lock");
        assert_eq!(seen[0], "https://reddit.com/r/x/s/AbC");
        assert_eq!(
            seen[1],
            "https://arctic-shift.photon-reddit.com/api/posts/ids?ids=abc"
        );
    }

    #[tokio::test]
    async fn tweet_target_fetches_fxtwitter_api() {
        let seen = std::sync::Mutex::new(Vec::new());
        let fetch = |url: Url, _max: u64, _t: Duration| {
            seen.lock().expect("lock").push(url.to_string());
            async {
                Ok(raw(
                    "https://api.fxtwitter.com/u/status/9",
                    "application/json",
                    br#"{"code":200,"tweet":{"text":"leak"}}"#,
                ))
            }
        };
        let input = gather_with(fetch, &u("https://x.com/u/status/9?s=20"))
            .await
            .expect("readable");
        assert_eq!(input.text, "leak");
        assert_eq!(
            *seen.lock().expect("lock"),
            vec!["https://api.fxtwitter.com/u/status/9".to_string()]
        );
    }

    async fn server() -> MockServer {
        crate::install_crypto_provider();
        ssrf_bypass_for_tests(true);
        MockServer::start().await
    }

    async fn serve(server: &MockServer, at: &str, content_type: &str, body: Vec<u8>) {
        Mock::given(method("GET"))
            .and(path(at))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", content_type)
                    .set_body_bytes(body),
            )
            .mount(server)
            .await;
    }

    async fn run(server: &MockServer, at: &str) -> Result<Input, GatherError> {
        let http = fetch_client().expect("client");
        gather(&http, &u(&format!("{}{at}", server.uri()))).await
    }

    #[tokio::test]
    async fn gather_html_with_og_image() {
        let server = server().await;
        serve(
            &server,
            "/page",
            "text/html; charset=utf-8",
            br#"<html><head><meta property="og:title" content="Trailer 3">
            <meta property="og:image" content="/thumb.png"></head>
            <body><p>words</p></body></html>"#
                .to_vec(),
        )
        .await;
        serve(&server, "/thumb.png", "image/png", PNG.to_vec()).await;

        let input = run(&server, "/page").await.expect("readable");
        assert!(input.text.contains("Trailer 3"), "{}", input.text);
        let img = input.image_data_url.expect("image");
        assert!(img.starts_with("data:image/png;base64,"), "{img}");
    }

    #[tokio::test]
    async fn gather_og_image_404_keeps_text() {
        let server = server().await;
        serve(
            &server,
            "/page",
            "text/html",
            br#"<meta property="og:title" content="Trailer 3">
            <meta property="og:image" content="/missing.png"><p>words</p>"#
                .to_vec(),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/missing.png"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let input = run(&server, "/page").await.expect("readable");
        assert!(input.text.contains("Trailer 3"));
        assert_eq!(input.image_data_url, None);
    }

    #[tokio::test]
    async fn gather_og_image_gif_dropped() {
        let server = server().await;
        serve(
            &server,
            "/page",
            "text/html",
            br#"<meta property="og:image" content="/a.gif"><p>words</p>"#.to_vec(),
        )
        .await;
        serve(&server, "/a.gif", "image/gif", GIF.to_vec()).await;

        let input = run(&server, "/page").await.expect("readable");
        assert_eq!(input.image_data_url, None);
        assert_eq!(input.text, "words");
    }

    #[tokio::test]
    async fn gather_fxtwitter_json() {
        let server = server().await;
        let json = serde_json::json!({
            "code": 200,
            "tweet": {
                "text": "GTA 6 leak thread",
                "media": {"all": [{"type": "photo", "url": format!("{}/p.jpg", server.uri())}]}
            }
        });
        serve(
            &server,
            "/u/status/9",
            "application/json",
            json.to_string().into_bytes(),
        )
        .await;
        serve(&server, "/p.jpg", "image/jpeg", JPEG.to_vec()).await;

        let input = run(&server, "/u/status/9").await.expect("readable");
        assert_eq!(input.text, "GTA 6 leak thread");
        assert!(
            input
                .image_data_url
                .expect("image")
                .starts_with("data:image/jpeg;base64,")
        );
    }

    #[tokio::test]
    async fn gather_direct_image() {
        let server = server().await;
        serve(&server, "/pic", "image/jpeg", JPEG.to_vec()).await;

        let input = run(&server, "/pic").await.expect("readable");
        assert_eq!(input.text, "");
        assert!(
            input
                .image_data_url
                .expect("image")
                .starts_with("data:image/jpeg;base64,")
        );
    }

    #[tokio::test]
    async fn gather_unreadable_cases() {
        let server = server().await;
        serve(&server, "/zip", "application/zip", vec![0x50, 0x4B, 3, 4]).await;
        serve(
            &server,
            "/empty",
            "text/html",
            b"<html><head></head><body></body></html>".to_vec(),
        )
        .await;
        serve(&server, "/gif", "image/gif", GIF.to_vec()).await;
        Mock::given(method("GET"))
            .and(path("/500"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        for at in ["/zip", "/empty", "/gif", "/500"] {
            assert_eq!(run(&server, at).await, Err(GatherError::Unreadable), "{at}");
        }
    }
}
