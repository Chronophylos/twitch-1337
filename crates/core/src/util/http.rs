//! Shared HTTP client helpers: the traced send and capped body reads.

use futures_util::StreamExt as _;
use reqwest::header::HeaderMap;
use reqwest::{RequestBuilder, Response, StatusCode, Url};
use serde::de::DeserializeOwned;
use tracing::{Instrument as _, Span, field::Empty, info_span};

/// `server.address` for requests whose target a viewer or the model chose.
const DYNAMIC_SERVER_ADDRESS: &str = "dynamic";

/// Send a request inside one `http.client` span.
///
/// Every outbound request goes through this trait instead of
/// `RequestBuilder::send`, so each one is a single client span carrying
/// `http.request.method`, `server.address` (host only: paths and query
/// strings can hold API keys and user input), and
/// `http.response.status_code` once headers arrive. The span stays open in
/// the returned [`TracedResponse`] until its body is read or it is dropped,
/// so body failures land on the same span.
///
/// Failure marking: a transport error, a 5xx, a failed body read, or a
/// status the caller rejects via [`TracedResponse::error_for_status`] sets
/// `error.type` and span status Error. A 4xx alone does not: several
/// upstreams answer 404 for "no such thing" (adsbdb, pronouns, RDAP, DHL),
/// which is a result, not a failure.
pub trait SendTraced {
    /// Send to a configured upstream; `server.address` is its host.
    fn send_traced(self) -> impl Future<Output = reqwest::Result<TracedResponse>> + Send;

    /// Send to a URL a viewer or the model supplied (`read_url`, link
    /// resolution); `server.address` is `dynamic` so the label stays a
    /// bounded set.
    fn send_traced_dynamic(self) -> impl Future<Output = reqwest::Result<TracedResponse>> + Send;
}

impl SendTraced for RequestBuilder {
    fn send_traced(self) -> impl Future<Output = reqwest::Result<TracedResponse>> + Send {
        send(self, Target::Configured)
    }

    fn send_traced_dynamic(self) -> impl Future<Output = reqwest::Result<TracedResponse>> + Send {
        send(self, Target::Dynamic)
    }
}

#[derive(Clone, Copy)]
enum Target {
    Configured,
    Dynamic,
}

async fn send(builder: RequestBuilder, target: Target) -> reqwest::Result<TracedResponse> {
    let (client, request) = builder.build_split();
    let request = request?;
    let span = {
        let server_address = match target {
            Target::Configured => request.url().host_str().unwrap_or_default(),
            Target::Dynamic => DYNAMIC_SERVER_ADDRESS,
        };
        info_span!(
            "http.client",
            otel.kind = "client",
            http.request.method = %request.method(),
            server.address = server_address,
            http.response.status_code = Empty,
            error.type = Empty,
            otel.status_code = Empty,
        )
    };
    match client.execute(request).instrument(span.clone()).await {
        Ok(inner) => {
            let status = inner.status();
            span.record("http.response.status_code", status.as_u16());
            if status.is_server_error() {
                mark_failed(&span, status.as_str());
            }
            Ok(TracedResponse { inner, span })
        }
        Err(error) => {
            mark_failed(&span, reqwest_error_type(&error));
            Err(error)
        }
    }
}

/// A response whose `http.client` span is still open. Reading the body
/// ([`json`](Self::json), [`text`](Self::text), [`read_capped`]) runs inside
/// the span and marks it failed on error; the span closes once the body is
/// consumed or the response is dropped.
pub struct TracedResponse {
    inner: Response,
    span: Span,
}

impl TracedResponse {
    pub fn status(&self) -> StatusCode {
        self.inner.status()
    }

    pub fn headers(&self) -> &HeaderMap {
        self.inner.headers()
    }

    /// Final URL after redirects.
    pub fn url(&self) -> &Url {
        self.inner.url()
    }

    /// [`Response::error_for_status`]: the caller rejects every 4xx/5xx, so
    /// a rejected status marks the span failed.
    pub fn error_for_status(self) -> reqwest::Result<Self> {
        let Self { inner, span } = self;
        let status = inner.status();
        match inner.error_for_status() {
            Ok(inner) => Ok(Self { inner, span }),
            Err(error) => {
                mark_failed(&span, status.as_str());
                Err(error)
            }
        }
    }

    pub async fn json<T: DeserializeOwned>(self) -> reqwest::Result<T> {
        let Self { inner, span } = self;
        read_body(span, inner.json()).await
    }

    pub async fn text(self) -> reqwest::Result<String> {
        let Self { inner, span } = self;
        read_body(span, inner.text()).await
    }
}

async fn read_body<T>(
    span: Span,
    read: impl Future<Output = reqwest::Result<T>>,
) -> reqwest::Result<T> {
    let result = read.instrument(span.clone()).await;
    if let Err(error) = &result {
        mark_failed(&span, reqwest_error_type(error));
    }
    result
}

/// Set `error.type` and span status Error (`otel.status_code`, which
/// tracing-opentelemetry maps onto the span status).
fn mark_failed(span: &Span, error_type: &str) {
    span.record("error.type", error_type);
    span.record("otel.status_code", "ERROR");
}

/// Bounded failure class of a reqwest error, for `error.type`.
fn reqwest_error_type(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_redirect() {
        "redirect"
    } else if error.is_decode() {
        "decode"
    } else if error.is_body() {
        "body"
    } else if error.is_request() {
        "request"
    } else {
        "_OTHER"
    }
}

/// Why [`read_capped`] gave up on a response body.
#[derive(Debug, thiserror::Error)]
pub enum BodyError {
    #[error("response body exceeds {max_bytes} bytes")]
    TooLarge { max_bytes: u64 },
    #[error("response body could not be read")]
    Read(#[source] reqwest::Error),
}

impl BodyError {
    /// Strip the request URL from a transport error, like
    /// [`reqwest::Error::without_url`], for call sites that keep URLs
    /// (query strings, instance ids) out of logs.
    pub fn without_url(self) -> Self {
        match self {
            Self::Read(error) => Self::Read(error.without_url()),
            too_large @ Self::TooLarge { .. } => too_large,
        }
    }

    /// `error.type` for the request's span.
    fn error_type(&self) -> &'static str {
        match self {
            Self::TooLarge { .. } => "TooLarge",
            Self::Read(error) => reqwest_error_type(error),
        }
    }
}

/// Stream a body, bailing as soon as it would exceed `max_bytes`.
///
/// Every external response body goes through this instead of
/// [`TracedResponse::text`]/[`TracedResponse::json`], so a misbehaving
/// upstream can make the bot buffer at most `max_bytes`, not an unbounded
/// body. Runs inside the request's span and marks it failed on error.
pub async fn read_capped(response: TracedResponse, max_bytes: u64) -> Result<Vec<u8>, BodyError> {
    let TracedResponse { inner, span } = response;
    let result = read_capped_untraced(inner, max_bytes)
        .instrument(span.clone())
        .await;
    if let Err(error) = &result {
        mark_failed(&span, error.error_type());
    }
    result
}

async fn read_capped_untraced(response: Response, max_bytes: u64) -> Result<Vec<u8>, BodyError> {
    if let Some(length) = response.content_length()
        && length > max_bytes
    {
        return Err(BodyError::TooLarge { max_bytes });
    }

    let initial_capacity = response
        .content_length()
        .map(|len| len.min(max_bytes) as usize)
        .unwrap_or(0);

    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(initial_capacity);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(BodyError::Read)?;
        if (buf.len() as u64).saturating_add(chunk.len() as u64) > max_bytes {
            return Err(BodyError::TooLarge { max_bytes });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Arc;

    use parking_lot::Mutex;
    use tracing::subscriber::DefaultGuard;
    use tracing_subscriber::filter::Targets;
    use tracing_subscriber::fmt::{self, format::FmtSpan};
    use tracing_subscriber::prelude::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{BodyError, SendTraced as _, read_capped};

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SharedBuf {
        fn output(&self) -> String {
            String::from_utf8(self.0.lock().clone()).unwrap()
        }
    }

    /// Capture closed spans of this module as text.
    fn capture() -> (SharedBuf, DefaultGuard) {
        crate::install_crypto_provider();
        let buf = SharedBuf::default();
        let sink = buf.clone();
        let layer = fmt::layer()
            .with_ansi(false)
            .with_span_events(FmtSpan::CLOSE)
            .with_writer(move || sink.clone())
            .with_filter(
                Targets::new().with_target("twitch_1337_core::util::http", tracing::Level::INFO),
            );
        let guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));
        (buf, guard)
    }

    async fn server(status: u16, body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn server_error_marks_the_one_client_span_failed_without_the_url() {
        let server = server(503, "down").await;
        let (buf, _guard) = capture();

        let body = reqwest::Client::new()
            .get(format!("{}/secret/path", server.uri()))
            .query(&[("api_key", "SECRET")])
            .send_traced()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "down");

        let out = buf.output();
        assert_eq!(out.matches("http.client{").count(), 1, "{out}");
        for field in [
            "http.request.method=GET",
            "server.address=\"127.0.0.1\"",
            "http.response.status_code=503",
            "error.type=\"503\"",
            "otel.status_code=\"ERROR\"",
        ] {
            assert!(out.contains(field), "missing {field}: {out}");
        }
        assert!(!out.contains("SECRET"), "query leaked: {out}");
        assert!(!out.contains("/secret/path"), "path leaked: {out}");
    }

    #[tokio::test]
    async fn dynamic_target_hides_the_host_and_4xx_is_not_a_failure() {
        let server = server(404, "").await;
        let (buf, _guard) = capture();

        let response = reqwest::Client::new()
            .get(server.uri())
            .send_traced_dynamic()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        drop(response);

        let out = buf.output();
        assert!(out.contains("server.address=\"dynamic\""), "{out}");
        assert!(!out.contains("127.0.0.1"), "host leaked: {out}");
        assert!(out.contains("http.response.status_code=404"), "{out}");
        assert!(!out.contains("error.type"), "{out}");
        assert!(!out.contains("otel.status_code"), "{out}");
    }

    #[tokio::test]
    async fn oversized_body_marks_the_span_failed() {
        let server = server(200, "0123456789").await;
        let (buf, _guard) = capture();

        let response = reqwest::Client::new()
            .get(server.uri())
            .send_traced()
            .await
            .unwrap();
        let error = read_capped(response, 4).await.unwrap_err();
        assert!(matches!(error, BodyError::TooLarge { .. }), "{error:?}");

        let out = buf.output();
        assert_eq!(out.matches("http.client{").count(), 1, "{out}");
        assert!(out.contains("error.type=\"TooLarge\""), "{out}");
        assert!(out.contains("otel.status_code=\"ERROR\""), "{out}");
    }
}
