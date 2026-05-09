//! Shared application state for axum handlers.
//!
//! Constructed in the bin (or by integration tests) and passed to
//! [`crate::build_router`]. Every handler clones individual `Arc`s out of
//! this struct.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use secrecy::SecretString;

use crate::auth::OAuthCtx;
use crate::auth::session::SessionTable;
use crate::clock::Clock;
use crate::config::WebConfig;
use crate::helix::HelixClient;

#[derive(Clone)]
pub struct WebState {
    pub sessions: Arc<SessionTable>,
    pub helix: Arc<dyn HelixClient>,
    pub irc_connected: Arc<AtomicBool>,
    pub config: Arc<WebConfig>,
    pub clock: Arc<dyn Clock>,
    /// primary channel login (lowercase Twitch login).
    pub channel: Arc<str>,
    /// resolved at startup via helix users by login.
    pub broadcaster_id: Arc<str>,
    pub hidden_admins: Arc<[String]>,
    /// Twitch developer-app client id (used in `Client-Id` headers when the
    /// callback fetches the caller's user record).
    pub client_id: SecretString,
    pub oauth: Arc<OAuthCtx>,
}
