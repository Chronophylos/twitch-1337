//! Shared fixtures for route-level integration tests in `crates/web/tests/`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use secrecy::SecretString;
use twitch_1337_web::WebState;
use twitch_1337_web::auth::OAuthCtx;
use twitch_1337_web::auth::session::SessionTable;
use twitch_1337_web::clock::Clock;
use twitch_1337_web::config::WebConfig;
use twitch_1337_web::helix::{HelixClient, HelixUser};

pub struct FixedClock(pub DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

pub struct FakeHelix {
    pub moderators: Vec<String>,
    pub users: HashMap<String, HelixUser>,
}

#[async_trait]
impl HelixClient for FakeHelix {
    async fn fetch_user_by_id(&self, id: &str) -> eyre::Result<Option<HelixUser>> {
        Ok(self.users.get(id).cloned())
    }
    async fn fetch_user_by_login(&self, login: &str) -> eyre::Result<Option<HelixUser>> {
        Ok(self.users.values().find(|u| u.login == login).cloned())
    }
    async fn is_moderator(&self, _broadcaster: &str, user_id: &str) -> eyre::Result<bool> {
        Ok(self.moderators.iter().any(|m| m == user_id))
    }
}

pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn build_state(helix: Arc<dyn HelixClient>) -> WebState {
    let clock = Arc::new(FixedClock(
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 1, 1, 0, 0, 0).unwrap(),
    ));
    let sessions = Arc::new(SessionTable::new(Duration::from_secs(7200), clock.clone()));
    let oauth = Arc::new(
        OAuthCtx::new(
            "test-client-id",
            &SecretString::new("test-secret".to_owned().into()),
            "https://test.invalid",
        )
        .expect("test oauth"),
    );
    let config = Arc::new(WebConfig {
        bind_addr: "127.0.0.1:0".into(),
        public_url: "https://test.invalid".into(),
        session_secret: SecretString::new("0".repeat(64).into()),
        session_ttl: Duration::from_secs(7200),
        mod_check_refresh: Duration::from_secs(300),
    });
    WebState {
        sessions,
        helix,
        irc_connected: Arc::new(AtomicBool::new(true)),
        config,
        clock,
        channel: Arc::from("testchannel"),
        broadcaster_id: Arc::from("100"),
        hidden_admins: Arc::from(Vec::<String>::new().into_boxed_slice()),
        client_id: SecretString::new("test-client-id".to_owned().into()),
        oauth,
    }
}
