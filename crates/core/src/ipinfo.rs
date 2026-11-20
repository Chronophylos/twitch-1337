//! Optional ipinfo.io client enriching `lookup_network` IP results with geo,
//! ASN and reverse hostname. Present only when `[ipinfo]` is configured.

use std::net::IpAddr;
use std::time::Duration;

use eyre::{Result, WrapErr, eyre};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::APP_USER_AGENT;
use crate::util::http::read_capped;

pub const DEFAULT_BASE_URL: &str = "https://ipinfo.io";
const TIMEOUT: Duration = Duration::from_secs(10);
/// One IP record is well under 1 KiB.
const MAX_BODY_BYTES: u64 = 64 * 1024;

/// Trimmed ipinfo record. City-level geo is the finest grain forwarded;
/// `loc`, `postal` and `timezone` are dropped.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct IpinfoRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireRecord {
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    city: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    country: Option<String>,
    /// `"AS13335 Cloudflare, Inc."`
    #[serde(default)]
    org: Option<String>,
}

pub struct IpinfoClient {
    http: reqwest::Client,
    base_url: String,
    token: SecretString,
}

impl IpinfoClient {
    pub fn new(token: SecretString) -> Result<Self> {
        Ok(Self {
            http: http_client()?,
            base_url: DEFAULT_BASE_URL.to_string(),
            token,
        })
    }

    /// Test hook: custom base URL (e.g. wiremock).
    pub fn with_base_url(
        http: reqwest::Client,
        base_url: impl Into<String>,
        token: SecretString,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            token,
        }
    }

    pub async fn lookup(&self, ip: IpAddr) -> Result<IpinfoRecord> {
        let url = format!("{}/{ip}/json", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(url)
            .bearer_auth(self.token.expose_secret())
            .send()
            .await
            .wrap_err("ipinfo request")?;
        let status = resp.status();
        if !status.is_success() {
            return Err(eyre!("ipinfo returned {status}"));
        }
        let body = read_capped(resp, MAX_BODY_BYTES)
            .await
            .wrap_err("read ipinfo response body")?;
        let wire: WireRecord = serde_json::from_slice(&body).wrap_err("decode ipinfo response")?;
        Ok(project(wire))
    }
}

/// ipinfo answers the lookup URL directly; redirects are refused so the
/// token never follows one.
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(APP_USER_AGENT)
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .wrap_err("build ipinfo HTTP client")
}

fn project(w: WireRecord) -> IpinfoRecord {
    let (asn, as_name) = match w.org.as_deref().map(str::trim) {
        Some(org) if org.starts_with("AS") => match org.split_once(' ') {
            Some((asn, name)) => (Some(asn.to_string()), non_empty(Some(name.to_string()))),
            None => (Some(org.to_string()), None),
        },
        Some(org) => (None, non_empty(Some(org.to_string()))),
        None => (None, None),
    };
    IpinfoRecord {
        hostname: non_empty(w.hostname),
        city: non_empty(w.city),
        region: non_empty(w.region),
        country: non_empty(w.country),
        asn,
        as_name,
    }
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn sends_bearer_token_and_splits_org() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/1.1.1.1/json"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ip": "1.1.1.1",
                "hostname": "one.one.one.one",
                "city": "Brisbane",
                "region": "Queensland",
                "country": "AU",
                "loc": "-27.4820,153.0136",
                "org": "AS13335 Cloudflare, Inc.",
                "postal": "4101",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = IpinfoClient::with_base_url(
            http_client().expect("client"),
            server.uri(),
            SecretString::new("test-token".into()),
        );
        let got = client
            .lookup("1.1.1.1".parse().expect("ip"))
            .await
            .expect("lookup");
        assert_eq!(got.hostname.as_deref(), Some("one.one.one.one"));
        assert_eq!(got.city.as_deref(), Some("Brisbane"));
        assert_eq!(got.asn.as_deref(), Some("AS13335"));
        assert_eq!(got.as_name.as_deref(), Some("Cloudflare, Inc."));
    }
}
