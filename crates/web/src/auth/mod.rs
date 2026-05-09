//! Web auth: OAuth + session + CSRF + mod-gate plumbing.
//!
//! Module map:
//! - [`session`]: in-memory session table (TTL + sliding refresh)
//! - [`csrf`]: hex-encoded double-submit token helpers
//! - [`mod_check`]: hidden_admins → broadcaster → helix moderators
//! - [`routes`]: login / callback / logout handlers + middleware

pub mod csrf;
pub mod mod_check;
pub mod session;

mod routes;

pub use routes::{OAuthCtx, auth_router, require_csrf, require_mod};
