//! Tracing / observability initialisation.

use tracing_error::ErrorLayer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

/// Install the `ring` rustls [`CryptoProvider`] as the process-wide default.
///
/// rustls 0.23 requires an explicit provider when its Cargo features pull in
/// more than one (our dependency tree transitively enables both `ring` and
/// `aws-lc-rs`), otherwise the first TLS handshake panics. Call once at
/// program startup before any TLS client is built. Safe to call again — a
/// second install is reported via the returned `Err` which we ignore.
///
/// [`CryptoProvider`]: rustls::crypto::CryptoProvider
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Install the global tracing subscriber (format + env-filter + error layer).
///
/// Call once at program startup before any spans are created.
pub fn install_tracing() {
    let fmt_layer = fmt::layer().with_target(false);
    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .with(ErrorLayer::default())
        .init();
}
