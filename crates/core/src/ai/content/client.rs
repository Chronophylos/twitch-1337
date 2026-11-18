use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use eyre::{Result, WrapErr as _, bail};
use futures_util::StreamExt as _;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use url::Host;

use crate::APP_USER_AGENT;
use crate::ai::content::detect::{Bucket, detect};
use crate::settings::ai::AiMedia;
use crate::util::truncate_response;

static SSRF_BYPASS: AtomicBool = AtomicBool::new(false);

const BLOCKED_TARGET: &str = "blocked target host";

fn ssrf_active() -> bool {
    !SSRF_BYPASS.load(Ordering::Relaxed)
}

#[derive(Default, Clone)]
struct TrustedHost {
    name: Option<String>,
    ip: Option<IpAddr>,
}

impl TrustedHost {
    fn from_url(url: &reqwest::Url) -> Self {
        match url.host() {
            None => Self::default(),
            Some(Host::Domain(name)) => Self {
                name: Some(name.to_ascii_lowercase()),
                ip: None,
            },
            Some(Host::Ipv4(v4)) => Self {
                name: None,
                ip: Some(v4.into()),
            },
            Some(Host::Ipv6(v6)) => Self {
                name: None,
                ip: Some(v6.into()),
            },
        }
    }

    fn matches_name(&self, host: &str) -> bool {
        self.name
            .as_deref()
            .is_some_and(|n| host.eq_ignore_ascii_case(n))
    }

    fn matches_ip(&self, ip: IpAddr) -> bool {
        self.ip.is_some_and(|t| t == ip)
    }
}

struct SsrfSafeResolver {
    trusted: Arc<TrustedHost>,
}

impl Resolve for SsrfSafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        let trusted = Arc::clone(&self.trusted);
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?
                .collect();
            let host_trusted = trusted.matches_name(&host);
            if ssrf_active()
                && !host_trusted
                && let Some(a) = addrs
                    .iter()
                    .find(|a| is_blocked_ip(a.ip()) && !trusted.matches_ip(a.ip()))
            {
                return Err(Box::new(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{BLOCKED_TARGET}: {}", a.ip()),
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            let iter: Addrs = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

fn ssrf_safe_redirect_policy(trusted: Arc<TrustedHost>) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if ssrf_active() && !redirect_target_trusted(attempt.url(), &trusted) {
            attempt.error(BLOCKED_TARGET)
        } else if attempt.previous().len() >= 10 {
            attempt.stop()
        } else {
            attempt.follow()
        }
    })
}

fn redirect_target_trusted(url: &reqwest::Url, trusted: &TrustedHost) -> bool {
    if !is_blocked_host_literal(url) {
        return true;
    }
    match url.host() {
        None => false,
        Some(Host::Domain(name)) => trusted.matches_name(name),
        Some(Host::Ipv4(v4)) => trusted.matches_ip(v4.into()),
        Some(Host::Ipv6(v6)) => trusted.matches_ip(v6.into()),
    }
}

fn build_ssrf_safe_client(trusted: TrustedHost) -> Result<reqwest::Client> {
    let trusted = Arc::new(trusted);
    reqwest::Client::builder()
        .user_agent(APP_USER_AGENT)
        .dns_resolver(Arc::new(SsrfSafeResolver {
            trusted: Arc::clone(&trusted),
        }))
        .redirect(ssrf_safe_redirect_policy(trusted))
        .build()
        .wrap_err("Failed to build web-search HTTP client")
}

/// Enable the SSRF bypass for integration tests pointing at loopback
/// (e.g. wiremock). Always `false` in production.
#[doc(hidden)]
pub fn ssrf_bypass_for_tests(enabled: bool) {
    SSRF_BYPASS.store(enabled, Ordering::Relaxed);
}

/// HTTP client for one-off public fetches (spoiler check). Same SSRF guard
/// as `read_url`, no trusted host.
pub fn fetch_client() -> Result<reqwest::Client> {
    build_ssrf_safe_client(TrustedHost::default())
}

/// Body bytes of a capped GET, untouched, plus the post-redirect URL.
#[derive(Debug, Clone)]
pub struct RawFetch {
    pub final_url: reqwest::Url,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

pub async fn fetch_raw(
    http: &reqwest::Client,
    url: reqwest::Url,
    max_bytes: u64,
    timeout: Duration,
) -> Result<RawFetch> {
    match url.scheme() {
        "http" | "https" => {}
        other => bail!("Unsupported URL scheme: {other}"),
    }

    if ssrf_active() && is_blocked_host_literal(&url) {
        bail!(BLOCKED_TARGET)
    }

    let response = http
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .wrap_err("Failed to fetch URL")?
        .error_for_status()
        .wrap_err("URL returned error status")?;

    let final_url = response.url().clone();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = read_capped(response, max_bytes).await?;

    Ok(RawFetch {
        final_url,
        content_type,
        bytes,
    })
}

/// Stream a body, bailing as soon as it would exceed `max_bytes`.
async fn read_capped(response: reqwest::Response, max_bytes: u64) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length()
        && length > max_bytes
    {
        bail!("Response too large")
    }

    let initial_capacity = response
        .content_length()
        .map(|len| len.min(max_bytes) as usize)
        .unwrap_or(0);

    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(initial_capacity);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.wrap_err("Failed to read URL response body")?;
        if (buf.len() as u64).saturating_add(chunk.len() as u64) > max_bytes {
            bail!("Response too large")
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

const SEARX_RESPONSE_LIMIT: usize = 10;

#[derive(Debug, Clone)]
pub enum Payload {
    Text(String),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct FetchedContent {
    pub bucket: Bucket,
    pub content_type: String,
    pub payload: Payload,
}

fn overall_cap(caps: &AiMedia) -> u64 {
    [
        caps.max_image_size,
        caps.max_pdf_size,
        caps.max_audio_size,
        caps.max_video_size,
        caps.max_text_size,
    ]
    .iter()
    .max()
    .expect("non-empty")
    .as_u64()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub published_at: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SearchClient {
    search_http: reqwest::Client,
    fetch_http: reqwest::Client,
    base_url: String,
    timeout: Duration,
}

impl SearchClient {
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self> {
        let parsed = reqwest::Url::parse(base_url).wrap_err("Invalid SearXNG base_url")?;
        let trusted = TrustedHost::from_url(&parsed);
        let search_http = build_ssrf_safe_client(trusted)?;
        let fetch_http = build_ssrf_safe_client(TrustedHost::default())?;
        Ok(Self {
            search_http,
            fetch_http,
            base_url: base_url.to_string(),
            timeout,
        })
    }

    pub fn new_with_client(base_url: String, timeout: Duration, http: reqwest::Client) -> Self {
        Self {
            search_http: http.clone(),
            fetch_http: http,
            base_url,
            timeout,
        }
    }

    pub async fn web_search(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>> {
        let effective_max = max_results.min(SEARX_RESPONSE_LIMIT);

        let response: SearxSearchResponse = self
            .search_http
            .get(&self.base_url)
            .query(&[("q", query), ("format", "json")])
            .timeout(self.timeout)
            .send()
            .await
            .wrap_err("Failed to call SearXNG search endpoint")?
            .error_for_status()
            .wrap_err("SearXNG returned error status")?
            .json()
            .await
            .wrap_err("Failed to parse SearXNG search response")?;

        let results = response
            .results
            .into_iter()
            .take(effective_max)
            .map(|r| SearchResult {
                title: truncate_response(&r.title, 200),
                url: r.url,
                snippet: truncate_response(&r.content.unwrap_or_default(), 500),
                published_at: r.published_date,
                source: r.engine,
            })
            .collect();

        Ok(results)
    }

    pub async fn fetch_for_read(&self, raw_url: &str, caps: &AiMedia) -> Result<FetchedContent> {
        let url = reqwest::Url::parse(raw_url).wrap_err("Invalid URL")?;

        match url.scheme() {
            "http" | "https" => {}
            other => bail!("Unsupported URL scheme: {other}"),
        }

        if ssrf_active() && is_blocked_host_literal(&url) {
            bail!(BLOCKED_TARGET)
        }

        let response = self
            .fetch_http
            .get(url.clone())
            .timeout(self.timeout)
            .send()
            .await
            .wrap_err("Failed to fetch URL")?
            .error_for_status()
            .wrap_err("URL returned error status")?;

        let header_ct = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let buf = read_capped(response, overall_cap(caps)).await?;

        let head = &buf[..buf.len().min(16)];
        let Some(bucket) = detect(&header_ct, head) else {
            bail!("Unsupported content type")
        };

        let bucket_cap = caps.cap_for(bucket).as_u64();
        if buf.len() as u64 > bucket_cap {
            bail!("Response too large")
        }

        let content_type = if header_ct.is_empty() {
            infer::get(head)
                .map(|k| k.mime_type().to_string())
                .unwrap_or_else(|| "application/octet-stream".to_string())
        } else {
            header_ct
        };

        let payload = if bucket == Bucket::Text {
            let media_type = content_type
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            let body = String::from_utf8_lossy(&buf).to_string();
            let text = if matches!(
                media_type.as_str(),
                "text/html" | "application/xhtml+xml" | "application/xml" | "text/xml"
            ) {
                extract_readable_text(&body)
            } else {
                collapse_ws(&body)
            };
            if text.is_empty() {
                bail!("No readable content extracted")
            }
            Payload::Text(text)
        } else {
            Payload::Bytes(buf)
        };

        Ok(FetchedContent {
            bucket,
            content_type,
            payload,
        })
    }
}

#[derive(Debug, Deserialize)]
struct SearxSearchResponse {
    #[serde(default)]
    results: Vec<SearxResult>,
}

#[derive(Debug, Deserialize)]
struct SearxResult {
    #[serde(default)]
    title: String,
    url: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default, rename = "publishedDate")]
    published_date: Option<String>,
    #[serde(default)]
    engine: Option<String>,
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || v4.is_multicast()
                || is_cgnat(v4)
                // 0.0.0.0/8 "this network"
                || a == 0
                // 198.18.0.0/15 benchmarking
                || (a == 198 && (b & 0xFE) == 18)
                // 240.0.0.0/4 reserved (incl. broadcast)
                || a >= 240
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            let embedded = |hi: u16, lo: u16| {
                let [h1, h2] = hi.to_be_bytes();
                let [l1, l2] = lo.to_be_bytes();
                is_blocked_ip(IpAddr::V4(Ipv4Addr::new(h1, h2, l1, l2)))
            };
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_blocked_ip(IpAddr::V4(v4)))
                // NAT64 well-known prefix 64:ff9b::/96, v4 in the low 32 bits
                || (seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] && embedded(seg[6], seg[7]))
                // 6to4 2002::/16, v4 in bits 16..48
                || (seg[0] == 0x2002 && embedded(seg[1], seg[2]))
        }
    }
}

/// 100.64.0.0/10: carrier-grade NAT, also Tailscale's tailnet range.
fn is_cgnat(v4: Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    a == 100 && (b & 0xC0) == 64
}

/// Matches on the parsed `Host`, not `host_str()`: the latter brackets IPv6
/// (`"[::1]"`), which never parses as an `IpAddr`, and hyper skips the custom
/// resolver for IP literals, so this is the only guard they get.
fn is_blocked_host_literal(url: &reqwest::Url) -> bool {
    match url.host() {
        None => true,
        Some(Host::Ipv4(v4)) => is_blocked_ip(v4.into()),
        Some(Host::Ipv6(v6)) => is_blocked_ip(v6.into()),
        Some(Host::Domain(host)) => {
            let host = host.to_ascii_lowercase();
            host == "localhost" || host.ends_with(".localhost")
        }
    }
}

pub(crate) fn extract_readable_text(html: &str) -> String {
    let doc = Html::parse_document(html);
    let article_sel = Selector::parse("article, main").expect("valid selector");
    let para_sel = Selector::parse("p, h1, h2, h3, li, blockquote").expect("valid selector");
    let body_sel = Selector::parse("body").expect("valid selector");

    let mut chunks: Vec<String> = doc
        .select(&article_sel)
        .flat_map(|node| node.select(&para_sel))
        .map(|n| collapse_ws(&n.text().collect::<Vec<_>>().join(" ")))
        .filter(|line| !line.is_empty())
        .collect();

    if chunks.is_empty()
        && let Some(body) = doc.select(&body_sel).next()
    {
        chunks = body
            .select(&para_sel)
            .map(|n| collapse_ws(&n.text().collect::<Vec<_>>().join(" ")))
            .filter(|line| !line.is_empty())
            .collect();
    }

    if chunks.is_empty() {
        // Skip script/style/noscript so a JS shell reads as empty instead of
        // handing minified code (or "enable JavaScript") on as page text.
        let text: Vec<&str> = doc
            .root_element()
            .descendants()
            .filter_map(|node| {
                let text = node.value().as_text()?;
                let hidden = node.ancestors().any(|a| {
                    a.value()
                        .as_element()
                        .is_some_and(|e| matches!(e.name(), "script" | "style" | "noscript"))
                });
                (!hidden).then_some(&**text)
            })
            .collect();
        return collapse_ws(&text.join(" "));
    }

    chunks.join("\n")
}

#[cfg(test)]
mod tests {

    use std::time::Duration;

    use wiremock::matchers::{method as wm_method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::ai::content::detect::Bucket;

    #[test]
    fn readable_text_fallback_skips_script_style_noscript() {
        let html = "<html><head><script>var x = 1;</script><style>p{color:red}</style></head>\
<body><div>hello world</div><noscript>Please enable JavaScript</noscript>\
<script>window.__STATE__={}</script></body></html>";
        assert_eq!(extract_readable_text(html), "hello world");

        let shell = "<html><head><script>boot()</script></head><body><div id=\"app\"></div>\
<script src=\"a.js\"></script></body></html>";
        assert_eq!(extract_readable_text(shell), "");
    }

    #[test]
    fn trusted_host_from_url_extracts_name_or_ip() {
        let by_name = TrustedHost::from_url(
            &reqwest::Url::parse("http://searxng-core:8080/search").expect("url"),
        );
        assert_eq!(by_name.name.as_deref(), Some("searxng-core"));
        assert!(by_name.ip.is_none());
        assert!(by_name.matches_name("searxng-core"));
        assert!(by_name.matches_name("SearXNG-Core"));
        assert!(!by_name.matches_name("evil.example"));

        let by_ip = TrustedHost::from_url(
            &reqwest::Url::parse("http://172.22.0.3:8080/search").expect("url"),
        );
        assert!(by_ip.name.is_none());
        assert!(by_ip.matches_ip("172.22.0.3".parse().expect("ip")));
        assert!(!by_ip.matches_ip("172.22.0.4".parse().expect("ip")));
    }

    #[test]
    fn redirect_trusts_configured_host() {
        let trusted = TrustedHost::from_url(
            &reqwest::Url::parse("http://searxng-core:8080/search").expect("url"),
        );
        assert!(redirect_target_trusted(
            &reqwest::Url::parse("http://searxng-core:8080/results").expect("url"),
            &trusted
        ));
        assert!(!redirect_target_trusted(
            &reqwest::Url::parse("http://10.0.0.1/results").expect("url"),
            &trusted
        ));
        assert!(redirect_target_trusted(
            &reqwest::Url::parse("https://example.com/results").expect("url"),
            &trusted
        ));
    }

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).expect("url")
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("ip")
    }

    #[test]
    fn blocks_ip_literal_hosts_incl_bracketed_ipv6() {
        for blocked in [
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::ffff:10.0.0.1]/",
            "http://[fd00::1]/",
            "http://[::ffff:100.64.0.1]/",
            "http://0.0.0.0/",
            "http://127.0.0.1/",
            "http://10.0.0.2/",
            "http://[::]:8080/",
            "http://localhost/",
        ] {
            assert!(is_blocked_host_literal(&url(blocked)), "{blocked}");
        }
        for allowed in [
            "http://[2606:4700::1111]/",
            "http://[::ffff:8.8.8.8]/",
            "https://example.com/",
        ] {
            assert!(!is_blocked_host_literal(&url(allowed)), "{allowed}");
        }
    }

    #[test]
    fn redirect_rejects_ipv6_loopback_target() {
        let trusted = TrustedHost::default();
        assert!(!redirect_target_trusted(&url("http://[::1]/"), &trusted));
        assert!(!redirect_target_trusted(
            &url("http://[::ffff:100.64.0.1]/"),
            &trusted
        ));
        assert!(redirect_target_trusted(
            &url("http://[2606:4700::1111]/"),
            &trusted
        ));
    }

    #[test]
    fn trusted_host_from_bracketed_ipv6_url() {
        let trusted = TrustedHost::from_url(&url("http://[fd00::5]:8080/search"));
        assert!(trusted.name.is_none());
        assert!(trusted.matches_ip(ip("fd00::5")));
        assert!(redirect_target_trusted(
            &url("http://[fd00::5]:8080/results"),
            &trusted
        ));
    }

    #[test]
    fn blocks_extra_special_v4_ranges() {
        for (blocked, outside) in [
            ("0.1.2.3", "1.0.0.1"),
            ("224.0.0.1", "223.255.255.255"),
            ("239.255.255.255", "223.255.255.254"),
            ("198.18.0.1", "198.17.255.255"),
            ("198.19.255.255", "198.20.0.0"),
            ("240.0.0.1", "8.8.8.8"),
            ("255.255.255.255", "9.9.9.9"),
        ] {
            assert!(is_blocked_ip(ip(blocked)), "{blocked}");
            assert!(!is_blocked_ip(ip(outside)), "{outside}");
        }
    }

    #[test]
    fn blocks_extra_special_v6_ranges() {
        assert!(is_blocked_ip(ip("ff02::1")));
        assert!(!is_blocked_ip(ip("fe00::1")));
        // NAT64 well-known prefix with embedded v4.
        assert!(is_blocked_ip(ip("64:ff9b::7f00:1")));
        assert!(is_blocked_ip(ip("64:ff9b::a00:1")));
        assert!(!is_blocked_ip(ip("64:ff9b::808:808")));
        assert!(!is_blocked_ip(ip("64:ff9a::7f00:1")));
        // 6to4 with embedded v4 in bits 16..48.
        assert!(is_blocked_ip(ip("2002:7f00:1::1")));
        assert!(is_blocked_ip(ip("2002:6440:1::1")));
        assert!(!is_blocked_ip(ip("2002:808:808::1")));
        assert!(!is_blocked_ip(ip("2003:7f00:1::1")));
    }

    #[test]
    fn blocks_cgnat_range() {
        assert!(is_blocked_ip("100.64.0.1".parse().expect("ip")));
        assert!(is_blocked_ip("100.127.255.254".parse().expect("ip")));
        assert!(!is_blocked_ip("100.128.0.1".parse().expect("ip")));
        assert!(!is_blocked_ip("100.63.255.255".parse().expect("ip")));
        assert!(is_blocked_ip("::ffff:100.64.0.1".parse().expect("ip")));
    }

    #[tokio::test]
    async fn fetch_raw_returns_bytes_and_final_url_after_redirect() {
        crate::install_crypto_provider();
        ssrf_bypass_for_tests(true);
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/from"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/to"))
            .mount(&server)
            .await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/to"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/zip")
                    .set_body_bytes(vec![1u8, 2, 3]),
            )
            .mount(&server)
            .await;

        let http = fetch_client().expect("client");
        let url = reqwest::Url::parse(&format!("{}/from", server.uri())).expect("url");
        let raw = fetch_raw(&http, url, 1024, Duration::from_secs(2))
            .await
            .expect("fetch");
        assert_eq!(raw.final_url.path(), "/to");
        assert_eq!(raw.content_type, "application/zip");
        assert_eq!(raw.bytes, vec![1u8, 2, 3]);
    }

    #[tokio::test]
    async fn fetch_raw_rejects_oversize_and_error_status() {
        crate::install_crypto_provider();
        ssrf_bypass_for_tests(true);
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 64]))
            .mount(&server)
            .await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let http = fetch_client().expect("client");
        let big = reqwest::Url::parse(&format!("{}/big", server.uri())).expect("url");
        let err = fetch_raw(&http, big, 16, Duration::from_secs(2))
            .await
            .expect_err("too large");
        assert!(err.to_string().contains("too large"), "{err}");
        let gone = reqwest::Url::parse(&format!("{}/gone", server.uri())).expect("url");
        assert!(
            fetch_raw(&http, gone, 16, Duration::from_secs(2))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn fetch_raw_rejects_non_http_scheme() {
        crate::install_crypto_provider();
        let http = fetch_client().expect("client");
        let url = reqwest::Url::parse("ftp://example.com/x").expect("url");
        assert!(
            fetch_raw(&http, url, 16, Duration::from_secs(2))
                .await
                .is_err()
        );
    }

    #[test]
    fn extracts_readable_text_from_html() {
        let html = r#"
            <html><body>
                <nav>menu</nav>
                <article>
                    <h1>Title</h1>
                    <p>First paragraph.</p>
                    <p>Second paragraph.</p>
                </article>
                <script>ignore me</script>
            </body></html>
        "#;

        let out = extract_readable_text(html);
        assert!(out.contains("Title"), "got: {out}");
        assert!(out.contains("First paragraph."), "got: {out}");
        assert!(!out.contains("ignore me"), "got: {out}");
    }

    fn caps() -> AiMedia {
        AiMedia::default()
    }

    fn make_test_client(base_url: &str) -> SearchClient {
        ssrf_bypass_for_tests(true);
        SearchClient::new(base_url, Duration::from_secs(2)).expect("client")
    }

    #[tokio::test]
    async fn fetch_for_read_returns_text_bucket_for_html() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/page"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html; charset=utf-8")
                    .set_body_string("<html><body><p>Hello</p></body></html>"),
            )
            .mount(&server)
            .await;

        let client = make_test_client(&format!("{}/search", server.uri()));
        let url = format!("{}/page", server.uri());
        let fetched = client
            .fetch_for_read(&url, &caps())
            .await
            .expect("fetch ok");
        assert_eq!(fetched.bucket, Bucket::Text);
        match fetched.payload {
            Payload::Text(t) => assert!(t.contains("Hello"), "got: {t}"),
            Payload::Bytes(_) => panic!("expected Text payload"),
        }
    }

    #[tokio::test]
    async fn fetch_for_read_returns_image_bucket_for_png() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        let png = vec![0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
        Mock::given(wm_method("GET"))
            .and(wm_path("/p.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(png.clone()),
            )
            .mount(&server)
            .await;

        let client = make_test_client(&format!("{}/search", server.uri()));
        let url = format!("{}/p.png", server.uri());
        let fetched = client.fetch_for_read(&url, &caps()).await.expect("fetch");
        assert_eq!(fetched.bucket, Bucket::Image);
        match fetched.payload {
            Payload::Bytes(b) => assert_eq!(b, png),
            Payload::Text(_) => panic!("expected Bytes payload"),
        }
    }

    #[tokio::test]
    async fn fetch_for_read_rejects_oversize_via_content_length() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        let png_magic = vec![0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
        Mock::given(wm_method("GET"))
            .and(wm_path("/big.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(png_magic.clone()),
            )
            .mount(&server)
            .await;

        let tiny_caps = AiMedia {
            max_image_size: bytesize::ByteSize::b(1),
            max_pdf_size: bytesize::ByteSize::b(1),
            max_audio_size: bytesize::ByteSize::b(1),
            max_video_size: bytesize::ByteSize::b(1),
            max_text_size: bytesize::ByteSize::b(1),
            ..AiMedia::default()
        };

        let client = make_test_client(&format!("{}/search", server.uri()));
        let err = client
            .fetch_for_read(&format!("{}/big.png", server.uri()), &tiny_caps)
            .await
            .expect_err("should reject");
        assert!(
            err.to_string().to_lowercase().contains("too large"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn fetch_for_read_rejects_unsupported_content_type() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/x"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/x-mystery")
                    .set_body_bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            )
            .mount(&server)
            .await;

        let client = make_test_client(&format!("{}/search", server.uri()));
        let err = client
            .fetch_for_read(&format!("{}/x", server.uri()), &caps())
            .await
            .expect_err("reject");
        assert!(
            err.to_string().to_lowercase().contains("unsupported"),
            "{err}"
        );
    }
}
