//! Finnhub stock quote client backing `!aktien`. Only `/quote` is used: the
//! free tier has no top-movers endpoint, so the command quotes a baked pool
//! and ranks it itself. Free tier: US listings only, 60 calls/min.

use std::time::Duration;

use eyre::{Result, WrapErr};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::APP_USER_AGENT;
use crate::util::http::{BodyError, read_capped};

pub const DEFAULT_BASE_URL: &str = "https://finnhub.io/api/v1";
const TIMEOUT: Duration = Duration::from_secs(10);
/// Body cap. A quote is ~150 bytes of JSON.
const MAX_BODY_BYTES: u64 = 64 * 1024;

/// Why a quote request failed. Every variant is an upstream failure; the
/// command answers all of them with the same chat line, so the cause is for
/// the log only.
#[derive(Debug, thiserror::Error)]
pub enum FinnhubError {
    #[error("finnhub request failed")]
    Request(#[source] reqwest::Error),
    #[error("finnhub returned {0}")]
    Status(reqwest::StatusCode),
    #[error("finnhub response body could not be read")]
    Body(#[source] BodyError),
    #[error("finnhub response did not parse")]
    Parse(#[source] serde_json::Error),
}

/// `/quote` body. Finnhub answers an unknown symbol with 200 and all zeros
/// (`dp` null), so every field is optional.
#[derive(Debug, Deserialize)]
struct WireQuote {
    /// Current price.
    #[serde(default)]
    c: Option<f64>,
    /// Percent change against the previous close.
    #[serde(default)]
    dp: Option<f64>,
}

impl WireQuote {
    /// The day's percent change, if the quote carries a live price.
    fn change_percent(self) -> Option<f64> {
        self.c.filter(|c| c.is_finite() && *c > 0.0)?;
        self.dp.filter(|dp| dp.is_finite())
    }
}

pub struct FinnhubClient {
    http: reqwest::Client,
    base_url: String,
    api_key: SecretString,
}

impl FinnhubClient {
    pub fn new(api_key: SecretString) -> Result<Self> {
        Ok(Self {
            http: http_client()?,
            base_url: DEFAULT_BASE_URL.to_owned(),
            api_key,
        })
    }

    /// Test hook: custom base URL (e.g. wiremock). Caller supplies the HTTP
    /// client; production [`Self::new`] uses [`http_client`].
    pub fn with_base_url(
        http: reqwest::Client,
        base_url: impl Into<String>,
        api_key: SecretString,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            api_key,
        }
    }

    /// The day's percent change (`dp`) for `symbol`. `Ok(None)` when Finnhub
    /// has no usable quote: an unknown symbol or one without data (all
    /// zeros), or a symbol the key's plan may not read (403, e.g. a non-US
    /// listing on the free tier).
    pub async fn change_percent(&self, symbol: &str) -> Result<Option<f64>, FinnhubError> {
        let url = format!("{}/quote", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(url)
            .header("X-Finnhub-Token", self.api_key.expose_secret())
            .query(&[("symbol", symbol)])
            .send()
            .await
            .map_err(FinnhubError::Request)?;

        let status = resp.status();
        if status == reqwest::StatusCode::FORBIDDEN {
            tracing::debug!(symbol, "finnhub denied the symbol for this plan");
            return Ok(None);
        }
        if !status.is_success() {
            return Err(FinnhubError::Status(status));
        }

        let body = read_capped(resp, MAX_BODY_BYTES)
            .await
            .map_err(FinnhubError::Body)?;
        let wire: WireQuote = serde_json::from_slice(&body).map_err(FinnhubError::Parse)?;
        Ok(wire.change_percent())
    }
}

/// The key rides in the custom `X-Finnhub-Token` header, which reqwest does
/// not strip on a cross-host redirect. Finnhub answers `/quote` directly, so
/// redirects are refused.
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(APP_USER_AGENT)
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .wrap_err("failed to build finnhub HTTP client")
}

#[cfg(test)]
mod tests {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path, query_param},
    };

    use super::*;

    fn client(server: &MockServer) -> FinnhubClient {
        crate::install_crypto_provider();
        FinnhubClient::with_base_url(
            reqwest::Client::new(),
            server.uri(),
            SecretString::new("key".into()),
        )
    }

    #[tokio::test]
    async fn quote_sends_token_header_and_parses() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/quote"))
            .and(query_param("symbol", "NVDA"))
            .and(header("X-Finnhub-Token", "key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"c":181.5,"d":7.3,"dp":4.19,"h":182,"l":175,"o":176,"pc":174.2,"t":1763650000}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;
        let change = client(&server).change_percent("NVDA").await.expect("ok");
        assert_eq!(change, Some(4.19));
    }

    #[tokio::test]
    async fn zero_quote_and_forbidden_symbol_are_none() {
        let server = MockServer::start().await;
        Mock::given(path("/quote"))
            .and(query_param("symbol", "NOPE"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"{"c":0,"d":null,"dp":null,"h":0,"l":0,"o":0,"pc":0,"t":0}"#,
                ),
            )
            .mount(&server)
            .await;
        Mock::given(path("/quote"))
            .and(query_param("symbol", "SAP.DE"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string(r#"{"error":"You don't have access to this resource."}"#),
            )
            .mount(&server)
            .await;
        let c = client(&server);
        assert_eq!(c.change_percent("NOPE").await.expect("ok"), None);
        assert_eq!(c.change_percent("SAP.DE").await.expect("ok"), None);
    }

    #[tokio::test]
    async fn rate_limit_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(path("/quote"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let err = client(&server)
            .change_percent("GME")
            .await
            .expect_err("429");
        assert!(
            matches!(err, FinnhubError::Status(s) if s == reqwest::StatusCode::TOO_MANY_REQUESTS),
            "{err:?}"
        );
    }
}
