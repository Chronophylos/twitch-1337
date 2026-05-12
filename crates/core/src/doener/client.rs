use std::time::Duration;

use eyre::{Result, WrapErr};

use crate::APP_USER_AGENT;
use crate::doener::types::{CitiesResponse, CityHit, GlobalStats};

const BASE_URL: &str = "https://xn--dnerindex-07a.com";
const TIMEOUT: Duration = Duration::from_secs(5);

pub struct DoenerClient {
    http: reqwest::Client,
    base_url: String,
}

impl DoenerClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .timeout(TIMEOUT)
            .build()
            .wrap_err("build doener HTTP client")?;
        Ok(Self {
            http,
            base_url: BASE_URL.to_string(),
        })
    }

    /// Test hook: inject an existing `reqwest::Client` (commonly with a short
    /// timeout) and a custom base URL pointing at a wiremock server.
    pub fn with_base_url(http: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into(),
        }
    }

    pub async fn stats(&self) -> Result<GlobalStats> {
        let url = format!("{}/api/stats.php", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .wrap_err("doener stats: request failed")?
            .error_for_status()
            .wrap_err("doener stats: non-2xx")?;
        resp.json::<GlobalStats>()
            .await
            .wrap_err("doener stats: parse JSON")
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn test_client(server: &MockServer) -> DoenerClient {
        crate::install_crypto_provider();
        DoenerClient::with_base_url(reqwest::Client::new(), server.uri())
    }

    #[tokio::test]
    async fn stats_parses_canonical_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/stats.php"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                br#"{"ok":true,"total_locations":6092,"total_cities":2202,"min_price":5.5,"max_price":9,"avg_price":6.1,"locations_no_price":5304,"locations_no_price_pct":87.1}"#.as_slice(),
                "application/json",
            ))
            .mount(&server)
            .await;

        let client = test_client(&server);
        let stats = client.stats().await.expect("stats ok");
        assert_eq!(stats.total_locations, 6092);
        assert_eq!(stats.total_cities, 2202);
    }

    #[tokio::test]
    async fn stats_returns_err_on_500() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/stats.php"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = test_client(&server);
        assert!(client.stats().await.is_err());
    }

    #[tokio::test]
    async fn stats_returns_err_on_malformed_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/stats.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = test_client(&server);
        assert!(client.stats().await.is_err());
    }
}
