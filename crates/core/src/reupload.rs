//! `!reupload`: fetch a link some viewers cannot open (imgur and similar) and
//! rehost the file. Images go to imgdrip, everything else to Litterbox. Both
//! hosts are anonymous and keyless.
//!
//! The source is fetched through the same SSRF-guarded client as `read_url`
//! and `!spoiler`, with a byte cap, so a viewer cannot point the bot at the
//! LAN or make it buffer an unbounded body.

use std::time::Duration;

use reqwest::{StatusCode, Url};
use scraper::{Html, Selector};
use serde::Deserialize;

use crate::ai::content::client::{FetchError, RawFetch, fetch_raw};
use crate::settings::ReuploadSettings;
use crate::util::http::{SendTraced as _, read_capped};

pub const IMGDRIP_UPLOAD_URL: &str = "https://imgdrip.com/api/upload";
pub const LITTERBOX_UPLOAD_URL: &str = "https://litterbox.catbox.moe/resources/internals/api.php";

/// imgdrip's own upload limit. An image above it is refused, not sent to
/// Litterbox: images are what imgdrip is for, and a silent host switch would
/// change the link's lifetime from days to 72 hours.
pub const IMGDRIP_MAX_BYTES: u64 = 20 * 1024 * 1024;

/// Litterbox retention. Fixed by decision, not a setting.
const LITTERBOX_TIME: &str = "72h";

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Cap on a host's answer: a JSON object or one URL line.
const HOST_RESPONSE_MAX_BYTES: u64 = 64 * 1024;

/// Image types imgdrip accepts.
const IMGDRIP_MIMES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

/// Why a reupload failed. `Display` is for logs; chat words come from the
/// command's `match`.
#[derive(Debug, thiserror::Error)]
pub enum ReuploadError {
    /// Non-HTTP scheme, invalid URL, or a non-public target.
    #[error("link target refused")]
    Refused(#[source] FetchError),
    #[error("file exceeds the size cap")]
    TooLarge,
    #[error("page has no og:video or og:image")]
    NoDirectLink,
    #[error("source is empty")]
    Empty,
    #[error("source fetch timed out")]
    FetchTimeout(#[source] FetchError),
    #[error("source fetch failed")]
    Fetch(#[source] FetchError),
    #[error("upload failed")]
    Upload(#[source] UploadError),
}

/// Why a file host did not hand back a link.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("upload request failed")]
    Http(#[source] reqwest::Error),
    #[error("upload host answered {status}: {body}")]
    Status { status: StatusCode, body: String },
    #[error("upload host rejected the file: {0}")]
    Rejected(String),
    #[error("upload host answered without a usable link: {0}")]
    BadResponse(String),
}

impl From<FetchError> for ReuploadError {
    fn from(error: FetchError) -> Self {
        match error {
            FetchError::InvalidUrl(_)
            | FetchError::UnsupportedScheme(_)
            | FetchError::Blocked { .. } => Self::Refused(error),
            FetchError::TooLarge => Self::TooLarge,
            FetchError::Timeout(_) => Self::FetchTimeout(error),
            _ => Self::Fetch(error),
        }
    }
}

/// The two upload endpoints and the client that talks to them. The hosts are
/// fixed public services, so this client needs no SSRF guard.
pub struct Hosts {
    http: reqwest::Client,
    imgdrip: Url,
    litterbox: Url,
}

impl Hosts {
    pub fn new() -> eyre::Result<Self> {
        use eyre::WrapErr as _;
        let http = reqwest::Client::builder()
            .user_agent(crate::APP_USER_AGENT)
            .build()
            .wrap_err("build reupload host HTTP client")?;
        let imgdrip = Url::parse(IMGDRIP_UPLOAD_URL).wrap_err("parse imgdrip URL")?;
        let litterbox = Url::parse(LITTERBOX_UPLOAD_URL).wrap_err("parse Litterbox URL")?;
        Ok(Self::with_endpoints(http, imgdrip, litterbox))
    }

    pub fn with_endpoints(http: reqwest::Client, imgdrip: Url, litterbox: Url) -> Self {
        Self {
            http,
            imgdrip,
            litterbox,
        }
    }

    async fn post(&self, url: Url, form: Multipart) -> Result<(StatusCode, String), UploadError> {
        let (content_type, body) = form.finish();
        let response = self
            .http
            .post(url)
            .timeout(UPLOAD_TIMEOUT)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body)
            .send_traced()
            .await
            .map_err(UploadError::Http)?;
        let status = response.status();
        let bytes = read_capped(response, HOST_RESPONSE_MAX_BYTES)
            .await
            .map_err(|error| match error {
                crate::util::http::BodyError::Read(error) => UploadError::Http(error),
                crate::util::http::BodyError::TooLarge { .. } => {
                    UploadError::BadResponse("oversized response".into())
                }
            })?;
        Ok((status, String::from_utf8_lossy_owned(bytes)))
    }

    /// `POST /api/upload` (fields `file`, `expires`); answers
    /// `{"image":{"cdn_url":…}}` or `{"error":{"message":…}}`.
    async fn imgdrip(
        &self,
        file: Vec<u8>,
        name: &str,
        mime: &str,
        expires_days: u32,
    ) -> Result<Url, UploadError> {
        #[derive(Deserialize)]
        struct Answer {
            image: Option<Image>,
            error: Option<Message>,
        }
        #[derive(Deserialize)]
        struct Image {
            cdn_url: String,
        }
        #[derive(Deserialize)]
        struct Message {
            message: String,
        }

        let mut form = Multipart::new();
        form.text("expires", &expires_days.to_string());
        form.file("file", name, mime, &file);
        let (status, body) = self.post(self.imgdrip.clone(), form).await?;
        let answer = serde_json::from_str::<Answer>(&body).ok();
        if let Some(message) = answer.as_ref().and_then(|a| a.error.as_ref()) {
            return Err(UploadError::Rejected(message.message.clone()));
        }
        if !status.is_success() {
            return Err(UploadError::Status {
                status,
                body: clip(&body),
            });
        }
        answer
            .and_then(|a| a.image)
            .and_then(|i| https_url(&i.cdn_url))
            .ok_or_else(|| UploadError::BadResponse(clip(&body)))
    }

    /// `reqtype=fileupload`, `time=72h`, `fileToUpload`; answers the link as
    /// plain text, or an error sentence.
    async fn litterbox(&self, file: Vec<u8>, name: &str, mime: &str) -> Result<Url, UploadError> {
        let mut form = Multipart::new();
        form.text("reqtype", "fileupload");
        form.text("time", LITTERBOX_TIME);
        form.file("fileToUpload", name, mime, &file);
        let (status, body) = self.post(self.litterbox.clone(), form).await?;
        if !status.is_success() {
            return Err(UploadError::Status {
                status,
                body: clip(&body),
            });
        }
        https_url(body.trim()).ok_or_else(|| UploadError::BadResponse(clip(&body)))
    }
}

/// Fetch `url` (one `og:video`/`og:image` hop for an HTML page) and rehost it.
pub async fn reupload(
    fetch_http: &reqwest::Client,
    hosts: &Hosts,
    url: Url,
    settings: &ReuploadSettings,
) -> Result<Url, ReuploadError> {
    let cap = settings.max_bytes.as_u64();
    let first = fetch_raw(fetch_http, url, cap, FETCH_TIMEOUT).await?;
    let media = if is_html(&first) {
        let target = og_media(&first).ok_or(ReuploadError::NoDirectLink)?;
        let second = fetch_raw(fetch_http, target, cap, FETCH_TIMEOUT).await?;
        if is_html(&second) {
            return Err(ReuploadError::NoDirectLink);
        }
        second
    } else {
        first
    };
    if media.bytes.is_empty() {
        return Err(ReuploadError::Empty);
    }

    // Magic bytes first, then the declared Content-Type (stripped of
    // anything that could break the multipart header line).
    let sniffed = infer::get(&media.bytes);
    let mime = sniffed
        .map(|t| t.mime_type().to_owned())
        .unwrap_or_else(|| essence(&media.content_type))
        .replace(['\r', '\n', '"'], "");
    let image = IMGDRIP_MIMES.iter().copied().find(|m| *m == mime);
    let ext = sniffed.map(|t| t.extension()).or(match image {
        Some("image/jpeg") => Some("jpg"),
        Some(m) => m.strip_prefix("image/"),
        None => None,
    });
    let name = file_name(&media.final_url, ext);
    match image {
        Some(mime) => {
            if media.bytes.len() as u64 > IMGDRIP_MAX_BYTES {
                return Err(ReuploadError::TooLarge);
            }
            hosts
                .imgdrip(media.bytes, &name, mime, settings.expires_days)
                .await
        }
        None => {
            let mime = if mime.is_empty() {
                "application/octet-stream"
            } else {
                mime.as_str()
            };
            hosts.litterbox(media.bytes, &name, mime).await
        }
    }
    .map_err(ReuploadError::Upload)
}

/// An HTML page. `infer` recognises common HTML openings itself (doctype,
/// `<html>`, `<p>`, …); any other sniffed signature means a real file even
/// when the server mislabels it, and an unsniffable body falls back to the
/// declared content type.
fn is_html(fetch: &RawFetch) -> bool {
    match infer::get(&fetch.bytes).map(|t| t.mime_type()) {
        Some(mime) => mime == "text/html",
        None => matches!(
            essence(&fetch.content_type).as_str(),
            "text/html" | "application/xhtml+xml"
        ),
    }
}

fn essence(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// The page's video, else its image, resolved against the final URL.
fn og_media(page: &RawFetch) -> Option<Url> {
    let html = String::from_utf8_lossy(&page.bytes);
    let doc = Html::parse_document(&html);
    [
        "video:secure_url",
        "video:url",
        "video",
        "image:secure_url",
        "image:url",
        "image",
    ]
    .iter()
    .find_map(|prop| {
        let sel = Selector::parse(&format!(
            r#"meta[property="og:{prop}"], meta[name="og:{prop}"]"#
        ))
        .ok()?;
        doc.select(&sel)
            .filter_map(|el| el.value().attr("content"))
            .map(str::trim)
            .find(|c| !c.is_empty())
            .and_then(|raw| page.final_url.join(raw).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"))
    })
}

/// Upload file name: the URL's last path segment reduced to
/// `[A-Za-z0-9._-]`, with the sniffed extension when it has none.
fn file_name(url: &Url, sniffed_ext: Option<&str>) -> String {
    let stem: String = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(64)
        .collect();
    let stem = stem.trim_matches('.');
    let base = if stem.is_empty() { "file" } else { stem };
    match sniffed_ext {
        Some(ext) if !base.contains('.') => format!("{base}.{ext}"),
        _ => base.to_owned(),
    }
}

fn https_url(raw: &str) -> Option<Url> {
    Url::parse(raw).ok().filter(|u| u.scheme() == "https")
}

/// Host answers are logged, never sent to chat; keep the log line short.
fn clip(body: &str) -> String {
    body.trim().chars().take(200).collect()
}

/// Minimal `multipart/form-data` body: text fields plus one file part.
struct Multipart {
    boundary: String,
    body: Vec<u8>,
}

impl Multipart {
    fn new() -> Self {
        use rand::RngExt as _;
        let boundary = format!("twitch1337-{:032x}", rand::rng().random::<u128>());
        Self {
            boundary,
            body: Vec::new(),
        }
    }

    fn text(&mut self, name: &str, value: &str) {
        self.body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n",
                self.boundary
            )
            .as_bytes(),
        );
    }

    /// `file_name` and `mime` must be header-safe; [`file_name`] and the
    /// callers guarantee it.
    fn file(&mut self, name: &str, file_name: &str, mime: &str, bytes: &[u8]) {
        self.body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{file_name}\"\r\nContent-Type: {mime}\r\n\r\n",
                self.boundary
            )
            .as_bytes(),
        );
        self.body.extend_from_slice(bytes);
        self.body.extend_from_slice(b"\r\n");
    }

    fn finish(mut self) -> (String, Vec<u8>) {
        self.body
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        (
            format!("multipart/form-data; boundary={}", self.boundary),
            self.body,
        )
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::ai::content::client::{fetch_client, ssrf_bypass_for_tests};

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDRpixels";
    const MP4: &[u8] = b"\0\0\0\x18ftypmp42\0\0\0\0mp42isomframes";

    struct Env {
        source: MockServer,
        hosts: MockServer,
    }

    impl Env {
        async fn new() -> Self {
            crate::install_crypto_provider();
            ssrf_bypass_for_tests(true);
            Self {
                source: MockServer::start().await,
                hosts: MockServer::start().await,
            }
        }

        fn hosts(&self) -> Hosts {
            Hosts::with_endpoints(
                reqwest::Client::new(),
                Url::parse(&format!("{}/imgdrip", self.hosts.uri())).unwrap(),
                Url::parse(&format!("{}/litterbox", self.hosts.uri())).unwrap(),
            )
        }

        async fn serve(&self, at: &str, content_type: &str, body: Vec<u8>) {
            Mock::given(method("GET"))
                .and(path(at))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", content_type)
                        .set_body_bytes(body),
                )
                .mount(&self.source)
                .await;
        }

        async fn run(&self, at: &str, settings: &ReuploadSettings) -> Result<Url, ReuploadError> {
            let url = Url::parse(&format!("{}{at}", self.source.uri())).unwrap();
            reupload(&fetch_client().unwrap(), &self.hosts(), url, settings).await
        }
    }

    fn body_has(needle: &'static [u8]) -> impl Fn(&Request) -> bool {
        move |req: &Request| req.body.windows(needle.len()).any(|w| w == needle)
    }

    fn imgdrip_ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_string(
            r#"{"image":{"cdn_url":"https://cdn.imgdrip.com/x.png","page_url":"https://imgdrip.com/x"}}"#,
        )
    }

    #[tokio::test]
    async fn image_goes_to_imgdrip_with_expiry_and_returns_cdn_url() {
        let env = Env::new().await;
        env.serve("/a/cat.png", "image/png", PNG.to_vec()).await;
        Mock::given(method("POST"))
            .and(path("/imgdrip"))
            .and(body_has(b"name=\"expires\"\r\n\r\n30\r\n"))
            .and(body_has(
                b"name=\"file\"; filename=\"cat.png\"\r\nContent-Type: image/png",
            ))
            .and(body_has(PNG))
            .respond_with(imgdrip_ok())
            .expect(1)
            .mount(&env.hosts)
            .await;

        let url = env
            .run("/a/cat.png", &ReuploadSettings::default())
            .await
            .unwrap();
        assert_eq!(url.as_str(), "https://cdn.imgdrip.com/x.png");
    }

    #[tokio::test]
    async fn video_goes_to_litterbox_for_72_hours() {
        let env = Env::new().await;
        env.serve("/clip", "application/octet-stream", MP4.to_vec())
            .await;
        Mock::given(method("POST"))
            .and(path("/litterbox"))
            .and(body_has(b"name=\"reqtype\"\r\n\r\nfileupload\r\n"))
            .and(body_has(b"name=\"time\"\r\n\r\n72h\r\n"))
            .and(body_has(
                b"filename=\"clip.mp4\"\r\nContent-Type: video/mp4",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("https://litter.catbox.moe/ab12.mp4\n"),
            )
            .expect(1)
            .mount(&env.hosts)
            .await;

        let url = env
            .run("/clip", &ReuploadSettings::default())
            .await
            .unwrap();
        assert_eq!(url.as_str(), "https://litter.catbox.moe/ab12.mp4");
    }

    #[tokio::test]
    async fn html_page_resolves_og_video_before_og_image() {
        let env = Env::new().await;
        env.serve(
            "/page",
            "text/html; charset=utf-8",
            br#"<html><head><meta property="og:image" content="/thumb.png">
            <meta property="og:video" content="/clip.mp4"></head></html>"#
                .to_vec(),
        )
        .await;
        env.serve("/clip.mp4", "video/mp4", MP4.to_vec()).await;
        Mock::given(method("POST"))
            .and(path("/litterbox"))
            .and(body_has(b"filename=\"clip.mp4\""))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("https://litter.catbox.moe/v.mp4"),
            )
            .expect(1)
            .mount(&env.hosts)
            .await;

        let url = env
            .run("/page", &ReuploadSettings::default())
            .await
            .unwrap();
        assert_eq!(url.as_str(), "https://litter.catbox.moe/v.mp4");
    }

    /// A body `infer` can't sniff still counts as an image when the server
    /// declares one of imgdrip's types: it goes to imgdrip, not Litterbox.
    #[tokio::test]
    async fn declared_image_type_routes_to_imgdrip_when_unsniffable() {
        let env = Env::new().await;
        env.serve(
            "/odd",
            "image/webp; charset=binary",
            b"not-a-real-riff".to_vec(),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/imgdrip"))
            .and(body_has(
                b"filename=\"odd.webp\"\r\nContent-Type: image/webp",
            ))
            .respond_with(imgdrip_ok())
            .expect(1)
            .mount(&env.hosts)
            .await;

        env.run("/odd", &ReuploadSettings::default()).await.unwrap();
    }

    #[tokio::test]
    async fn html_page_with_og_image_reuploads_the_image() {
        let env = Env::new().await;
        env.serve(
            "/gallery",
            "text/html",
            br#"<meta property="og:image" content="/i/pic.png"><p>imgur</p>"#.to_vec(),
        )
        .await;
        env.serve("/i/pic.png", "image/png", PNG.to_vec()).await;
        Mock::given(method("POST"))
            .and(path("/imgdrip"))
            .respond_with(imgdrip_ok())
            .expect(1)
            .mount(&env.hosts)
            .await;

        env.run("/gallery", &ReuploadSettings::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn page_without_og_media_is_no_direct_link() {
        let env = Env::new().await;
        env.serve("/text", "text/html", b"<p>just words</p>".to_vec())
            .await;
        let error = env
            .run("/text", &ReuploadSettings::default())
            .await
            .unwrap_err();
        assert!(matches!(error, ReuploadError::NoDirectLink), "{error:?}");
    }

    #[tokio::test]
    async fn source_over_max_bytes_is_too_large_and_nothing_uploads() {
        let env = Env::new().await;
        env.serve("/big.mp4", "video/mp4", [MP4, &[0u8; 2048]].concat())
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&env.hosts)
            .await;
        let settings = ReuploadSettings {
            max_bytes: bytesize::ByteSize::b(1024),
            ..ReuploadSettings::default()
        };
        let error = env.run("/big.mp4", &settings).await.unwrap_err();
        assert!(matches!(error, ReuploadError::TooLarge), "{error:?}");
    }

    /// An image above imgdrip's limit is refused, not sent to Litterbox.
    #[tokio::test]
    async fn image_over_imgdrip_limit_is_too_large() {
        let env = Env::new().await;
        let mut big = PNG.to_vec();
        big.resize(usize::try_from(IMGDRIP_MAX_BYTES).unwrap() + 1, 0);
        env.serve("/huge.png", "image/png", big).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&env.hosts)
            .await;
        let error = env
            .run("/huge.png", &ReuploadSettings::default())
            .await
            .unwrap_err();
        assert!(matches!(error, ReuploadError::TooLarge), "{error:?}");
    }

    #[tokio::test]
    async fn empty_source_is_rejected() {
        let env = Env::new().await;
        env.serve("/empty", "image/png", Vec::new()).await;
        let error = env
            .run("/empty", &ReuploadSettings::default())
            .await
            .unwrap_err();
        assert!(matches!(error, ReuploadError::Empty), "{error:?}");
    }

    #[tokio::test]
    async fn non_public_target_is_refused_before_any_request() {
        crate::install_crypto_provider();
        ssrf_bypass_for_tests(false);
        let hosts = Hosts::with_endpoints(
            reqwest::Client::new(),
            Url::parse("http://127.0.0.1:9/imgdrip").unwrap(),
            Url::parse("http://127.0.0.1:9/litterbox").unwrap(),
        );
        for target in [
            "http://127.0.0.1/x.png",
            "http://localhost./x.png",
            "ftp://example.com/x",
        ] {
            let error = reupload(
                &fetch_client().unwrap(),
                &hosts,
                Url::parse(target).unwrap(),
                &ReuploadSettings::default(),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, ReuploadError::Refused(_)),
                "{target}: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn host_failures_map_to_upload_errors() {
        let env = Env::new().await;
        env.serve("/a.png", "image/png", PNG.to_vec()).await;
        env.serve("/b.mp4", "video/mp4", MP4.to_vec()).await;
        Mock::given(method("POST"))
            .and(path("/imgdrip"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"error":{"message":"unsupported file"}}"#),
            )
            .mount(&env.hosts)
            .await;
        Mock::given(method("POST"))
            .and(path("/litterbox"))
            .respond_with(ResponseTemplate::new(200).set_body_string("No file uploaded"))
            .mount(&env.hosts)
            .await;

        let image = env.run("/a.png", &ReuploadSettings::default()).await;
        assert!(
            matches!(&image, Err(ReuploadError::Upload(UploadError::Rejected(m))) if m == "unsupported file"),
            "{image:?}"
        );
        let video = env.run("/b.mp4", &ReuploadSettings::default()).await;
        assert!(
            matches!(
                video,
                Err(ReuploadError::Upload(UploadError::BadResponse(_)))
            ),
            "{video:?}"
        );
    }

    #[test]
    fn file_name_keeps_safe_chars_and_adds_the_sniffed_extension() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            file_name(&u("https://i.imgur.com/AbC.png"), Some("png")),
            "AbC.png"
        );
        assert_eq!(
            file_name(&u("https://x.example/watch"), Some("mp4")),
            "watch.mp4"
        );
        assert_eq!(file_name(&u("https://x.example/"), Some("gif")), "file.gif");
        assert_eq!(
            file_name(&u("https://x.example/a%22b%0D.png"), None),
            "a22b0D.png"
        );
    }
}
