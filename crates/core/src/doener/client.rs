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
}
