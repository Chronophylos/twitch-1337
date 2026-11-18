# ADR-0007: Ring-only rustls (no aws-lc)

**Status:** Accepted (2026-07-18)
**Relates to:** musl / `FROM scratch` deploy ([AGENTS.md](../../AGENTS.md) § Binary / Docker);
[`install_crypto_provider`](../../crates/core/src/util/mod.rs)

## Context

rustls 0.23 supports two first-party crypto providers: `ring` and `aws-lc-rs`.
When both appear in the Cargo feature graph, rustls refuses to auto-pick and
panics on the first TLS handshake unless the process installs a default
provider. The production image is a fully static musl binary (`FROM scratch`);
`aws-lc-sys` pulls a CMake/C build that is hostile to that constraint and is the
ecosystem default many crates enable via `hyper-rustls` / reqwest defaults.

We briefly depended on a half-finished “ring TLS” setup: the app pinned ring
and called `install_crypto_provider()`, but workspace test/dev deps (notably
fantoccini → hyper-rustls defaults) and oauth2’s bundled reqwest 0.12 path
reintroduced `aws-lc-rs` via feature unification.

## Decision

1. **Crypto provider:** rustls with **`ring` only**. Workspace pin:
   `rustls = { default-features = false, features = ["ring"] }`.
2. **HTTP:** `reqwest` with `rustls-no-provider` (no aws-lc). Process start
   must call `install_crypto_provider()` before any TLS client is built.
3. **Ban aws-lc:** `aws-lc-sys` / `aws-lc-rs` must not appear in
   `cargo tree --workspace --all-targets`. Enforced in CI and via
   `just check-no-aws-lc`.
4. **oauth2:** compiled without bundled `reqwest` / `rustls-tls` features;
   the dashboard supplies a workspace reqwest 0.13 adapter.
5. **e2e:** fantoccini is HTTP-only (no `rustls-tls` / `native-tls`) so it
   cannot pull hyper-rustls defaults.
6. **IRC:** `twitch-irc` from the Chronophylos fork rev that wires
   `tokio-rustls` + `ring` and token refresh via `rustls-no-provider`
   (pin by commit; upstream PR is a follow-up).

## Consequences

- **+** No dual-provider panics; one clear provider for prod and tests.
- **+** Musl / `FROM scratch` stays free of cmake / aws-lc-sys.
- **−** Every new dependency that enables rustls aws-lc defaults must be
  feature-gated or avoided; the CI tree check is the safety net.
- **−** Still need an explicit `install_crypto_provider()` under
  `rustls-no-provider` (intentional, not a dual-provider workaround).
