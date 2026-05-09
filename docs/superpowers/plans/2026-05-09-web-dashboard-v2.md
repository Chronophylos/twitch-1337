# Web Dashboard v2 (Forward-Debt) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the v1 "forward debt" so the embedded dashboard can ship to production: real htmx/pico bundles, signed sid/csrf cookies, form re-render on validation error, persistent sidebar, post-login `?next=` deep-link, and consolidated helix moderator-check call.

**Architecture:** Six independent tasks landing in their own commits on `feature/web-dashboard-v2`. Tasks 1, 2, 6 are surgical; Tasks 3 and 4 touch every form/page template; Task 5 threads a captured path through the OAuth flow via a short-lived dedicated cookie.

**Tech Stack:** Rust + axum 0.8 + askama 0.14 + HTMX 2.x + Pico 2.x + tower-cookies 0.11 (`signed` + `private` features already enabled in workspace `Cargo.toml`).

---

## File Structure

| File | Responsibility | Tasks that touch it |
|---|---|---|
| `crates/web/assets/htmx.min.js` | vendored htmx 2.x bundle | 1 |
| `crates/web/assets/pico.min.css` | vendored pico 2.x bundle | 1 |
| `crates/web/src/state.rs` | adds `signed_key: tower_cookies::Key` | 2 |
| `crates/web/src/auth/routes.rs` | `cookies.signed(&key)` for sid + csrf; `?next=` plumb-through | 2, 5 |
| `crates/web/src/lib.rs` | router build (no change for v2 — Key lives in state) | — |
| `crates/web/src/error.rs` | `Unauthenticated { next: Option<String> }` variant | 5 |
| `crates/web/src/auth/session.rs` | unchanged | — |
| `crates/web/src/routes/pings.rs` | re-render `FormTpl` on validation/duplicate errors | 3, 4 |
| `crates/web/src/routes/memory.rs` | re-render `EditorTpl` / `NewStateTpl` on validation errors | 3, 4 |
| `crates/web/src/helix.rs` | extract free `helix_moderator_check` helper | 6 |
| `crates/web/src/auth/mod_check.rs` | `is_moderator_with_user_token` calls into helix.rs helper | 6 |
| `crates/web/templates/base.html` | include sidebar, accept layout vars | 4 |
| `crates/web/templates/sidebar.html` | take `current_page` for highlight | 4 |
| All `crates/web/templates/**/*.html` template structs | gain `user_login` + `csrf` + `current_page` fields | 4 |
| `crates/twitch-1337/src/main.rs` | derive `Key` from `session_secret`, store in `WebState` | 2 |
| `crates/web/tests/...` | new tests per task (see each task) | 1, 2, 3, 5, 6 |

---

## Branch Setup

- [ ] **Step 0: Create v2 feature branch from current `spec/web-dashboard` HEAD**

```bash
git checkout spec/web-dashboard
git pull --ff-only
git checkout -b feature/web-dashboard-v2
```

Expected: clean working tree, branch tracks nothing yet.

---

## Task 1: Real htmx + pico bundles

**Why:** v1 ships placeholder stubs (`htmx.min.js` is 190 bytes of `console.log`). HTMX-driven delete + form submit don't actually work without the real library. `Cache-Control: immutable` is already set on the asset route, so no header changes needed.

**Files:**
- Replace: `crates/web/assets/htmx.min.js`
- Replace: `crates/web/assets/pico.min.css`
- Test: `crates/web/tests/assets_smoke.rs` (new)

**Pinned versions** (used in download commands below):
- htmx **2.0.4** — `bigskysoftware/htmx` tag `v2.0.4`
- pico **2.0.6** — `picocss/pico` tag `v2.0.6`

These are hand-picked because they're the latest stable at plan time (2026-05-09); bumping to whatever is current at execution time is fine, but record the new version in the commit message.

- [ ] **Step 1.1: Download bundles from raw.githubusercontent.com**

```bash
curl -fsSL -o crates/web/assets/htmx.min.js \
  https://raw.githubusercontent.com/bigskysoftware/htmx/v2.0.4/dist/htmx.min.js
curl -fsSL -o crates/web/assets/pico.min.css \
  https://raw.githubusercontent.com/picocss/pico/v2.0.6/css/pico.min.css
```

Expected: `htmx.min.js` ~50 KiB, `pico.min.css` ~80 KiB.

- [ ] **Step 1.2: Sanity-check the downloads**

```bash
wc -c crates/web/assets/htmx.min.js crates/web/assets/pico.min.css
file crates/web/assets/htmx.min.js crates/web/assets/pico.min.css
head -c 100 crates/web/assets/htmx.min.js
```

Expected: file sizes >40 KiB each; `file` reports text/JS-ish for htmx and CSS for pico; first bytes of htmx contain identifiers like `htmx` and a license banner. If either is HTML (404 page), retry with the `mirror` host or pick a different tag.

- [ ] **Step 1.3: Write smoke test asserting non-stub bundle**

Create `crates/web/tests/assets_smoke.rs`:

```rust
//! Smoke tests for the `/assets/*` route. Ensures htmx and pico bundles
//! are real (not the development stubs) and served with the expected
//! cache-control header so deploy reviews catch a regression to placeholders.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use twitch_1337_web::build_router;

mod helpers;
use helpers::{build_state, fake_helix, install_crypto};

async fn fetch_asset(path: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    install_crypto();
    let state = build_state(fake_helix()).await;
    let app = build_router(state);
    let req = Request::builder().uri(path).body(Body::empty()).unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, headers, bytes)
}

#[tokio::test]
async fn htmx_bundle_is_real_not_a_stub() {
    let (status, _headers, bytes) = fetch_asset("/assets/htmx.min.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(bytes.len() > 40_000, "htmx bundle suspiciously small: {} bytes", bytes.len());
    let s = std::str::from_utf8(&bytes).expect("htmx must be utf-8");
    assert!(!s.contains("placeholder"), "htmx still contains the dev stub");
    assert!(s.contains("htmx"), "htmx bundle must mention itself");
}

#[tokio::test]
async fn pico_bundle_is_real_not_a_stub() {
    let (status, _headers, bytes) = fetch_asset("/assets/pico.min.css").await;
    assert_eq!(status, StatusCode::OK);
    assert!(bytes.len() > 40_000, "pico bundle suspiciously small: {} bytes", bytes.len());
    let s = std::str::from_utf8(&bytes).expect("pico must be utf-8");
    assert!(!s.contains("placeholder"));
    assert!(s.contains("pico") || s.contains("--pico"));
}

#[tokio::test]
async fn assets_emit_immutable_cache_control() {
    let (status, headers, _bytes) = fetch_asset("/assets/app.css").await;
    assert_eq!(status, StatusCode::OK);
    let cc = headers.get(header::CACHE_CONTROL).unwrap().to_str().unwrap();
    assert!(cc.contains("immutable"), "expected immutable cache-control, got `{cc}`");
}
```

Note: `http-body-util` is a dev-dep already pulled by axum 0.8. If clippy complains about its missing import, add `http-body-util = "0.1"` to `crates/web/Cargo.toml` `[dev-dependencies]` and re-run.

- [ ] **Step 1.4: Run the new tests**

```bash
cargo nextest run -p twitch-1337-web --test assets_smoke --show-progress=none --cargo-quiet --status-level=fail
```

Expected: `3 tests run: 3 passed`.

- [ ] **Step 1.5: Run the full web-crate suite**

```bash
cargo nextest run -p twitch-1337-web --show-progress=none --cargo-quiet --status-level=fail
```

Expected: all tests still passing (no regressions from the bundle swap).

- [ ] **Step 1.6: Commit**

```bash
git add crates/web/assets/htmx.min.js crates/web/assets/pico.min.css crates/web/tests/assets_smoke.rs
# Add Cargo.toml only if Step 1.3 forced you to add http-body-util to dev-deps.
git commit -m "feat(web): vendor real htmx 2.0.4 + pico 2.0.6 bundles

Replaces the v1 placeholder stubs (190-byte htmx, 274-byte pico) with
the actual minified releases pulled from upstream. Smoke tests assert
the bundles are real (size + content sniff) so a future drop back to
placeholders fails CI."
```

---

## Task 2: Signed sid + csrf cookies

**Why:** `[web].session_secret` is parsed from config but never consumed. Sid + csrf cookies are currently unsigned, so a forged sid that happens to collide with a live HashMap key passes auth (low-probability, but the secret exists explicitly to harden this). The flash cookie stays unsigned because it carries no auth value.

**Files:**
- Modify: `crates/web/src/state.rs`
- Modify: `crates/web/src/auth/routes.rs`
- Modify: `crates/twitch-1337/src/main.rs` (build `Key` from `session_secret`)
- Modify: `crates/web/tests/helpers/mod.rs` (test `Key` factory)
- Test: `crates/web/tests/auth_signed_cookies.rs` (new)

- [ ] **Step 2.1: Add `signed_key` to `WebState`**

Edit `crates/web/src/state.rs`:

```rust
//! Application state injected into every handler.
//!
//! Constructed by the bin (see `build_web_spawner` in
//! `crates/twitch-1337/src/main.rs`) and cloned cheaply per request via
//! axum's `State` extractor (every field is `Arc`-backed or `Copy`).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use secrecy::SecretString;
use tokio::sync::RwLock;
use tower_cookies::Key;
use twitch_1337_core::ai::memory::store::MemoryStore;
use twitch_1337_core::ping::PingManager;

use crate::auth::routes::OAuthCtx;
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
    pub channel: Arc<str>,
    pub broadcaster_id: Arc<str>,
    pub hidden_admins: Arc<[String]>,
    /// Twitch developer-app client id, used in `Client-Id` headers.
    pub client_id: SecretString,
    pub oauth: Arc<OAuthCtx>,
    pub ping_manager: Arc<RwLock<PingManager>>,
    pub memory_store: MemoryStore,
    /// HMAC key for signed cookies (sid + csrf). Derived from
    /// `[web].session_secret` in the bin so tampering with sid is detected
    /// on the next request rather than handled by HashMap miss alone.
    pub signed_key: Key,
}
```

`Key` is `Clone` and cheap (just two byte arrays); cloning `WebState` per request is unchanged.

- [ ] **Step 2.2: Build the `Key` in the bin from `session_secret`**

Edit `crates/twitch-1337/src/main.rs` inside `build_web_spawner`, just before `let state = WebState { ... }`:

```rust
let signed_key = {
    let secret = config.web.session_secret.expose_secret().as_bytes();
    if secret.len() < 32 {
        return Err(eyre!(
            "web.session_secret must be at least 32 bytes (got {})",
            secret.len()
        ));
    }
    tower_cookies::Key::derive_from(secret)
};
```

Add `signed_key,` to the `WebState { ... }` literal a few lines later.

- [ ] **Step 2.3: Switch sid + csrf cookie writes to `cookies.signed(&key)`**

Edit `crates/web/src/auth/routes.rs` `callback` handler — replace the `cookies.add(Cookie::build((SID_COOKIE, sid))...)` and `cookies.add(Cookie::build((CSRF_COOKIE, csrf_value_hex))...)` blocks with:

```rust
    let signed = cookies.signed(&state.signed_key);
    signed.add(
        Cookie::build((SID_COOKIE, sid))
            .http_only(true)
            .secure(true)
            .same_site(SameSite::Lax)
            .path("/")
            .build(),
    );
    signed.add(
        Cookie::build((CSRF_COOKIE, csrf_value_hex))
            .secure(true)
            .same_site(SameSite::Lax)
            .path("/")
            .build(),
    );
```

`tower_cookies::SignedCookies::add` consumes the cookie and stores its value with a prepended HMAC tag; the extractor's `.get()` later strips and verifies it.

- [ ] **Step 2.4: Read sid via the signed accessor in `logout` + `require_mod`**

Same file. `logout`:

```rust
    let sid = cookies
        .signed(&state.signed_key)
        .get(SID_COOKIE)
        .map(|c| c.value().to_owned())
        .ok_or(WebError::CsrfMismatch)?;
```

`require_mod`:

```rust
    let sid_cookie = cookies
        .signed(&state.signed_key)
        .get(SID_COOKIE)
        .ok_or(WebError::Unauthenticated)?;
    let session = state
        .sessions
        .get_and_touch(sid_cookie.value())
        .ok_or(WebError::Unauthenticated)?;
```

(The `oauth_state` cookie stays unsigned — it's already a CSRF nonce that's compared for equality, signing it adds nothing.)

The csrf cookie value lives client-side as a JS-readable double-submit token; `cookies.signed(...)` is *only* used to round-trip it through Set-Cookie / verify-on-read paths in tests and the conflict resubmit flow. The form-field check via `csrf::verify` is unchanged because the form field carries the bare hex (no signature) — and that's still constant-time-compared against `session.csrf_value`.

- [ ] **Step 2.5: Update test helpers to mint a `Key` + supply it through `WebState`**

Edit `crates/web/tests/helpers/mod.rs`. In `build_state()`, just before constructing `WebState`:

```rust
    // Tests don't need a real production secret; a fixed 32-byte key keeps
    // signed-cookie round-trips deterministic across reruns.
    let signed_key = tower_cookies::Key::from(&[0x42u8; 64]);
```

Add `signed_key,` to the WebState literal. Update the `cookie_header` helper to also reflect the new signed shape — the value posted in the `Cookie:` header must be the **signed** value (HMAC tag + bare value), which `tower_cookies::SignedCookies::add` produces. Helper:

```rust
/// `Cookie:` header value combining sid + csrf, matching what the browser
/// would send after a successful login. Tests inject the *signed* value,
/// because that's what hits the server in production.
pub fn cookie_header(signed_sid: &str, signed_csrf: &str) -> String {
    format!("tw1337_sid={signed_sid}; tw1337_csrf={signed_csrf}")
}

/// Sign a cookie pair against the test signed_key so handlers see a
/// SID_COOKIE the signed extractor will accept.
pub fn sign_for_tests(state: &WebState, name: &str, value: &str) -> String {
    use tower_cookies::Cookies;
    let cookies = Cookies::default();
    let signed = cookies.signed(&state.signed_key);
    signed.add(
        tower_cookies::Cookie::build((name.to_owned(), value.to_owned()))
            .path("/")
            .build(),
    );
    cookies
        .get(name)
        .expect("cookie present after signed add")
        .value()
        .to_owned()
}
```

`insert_session` then becomes:

```rust
pub fn insert_session(state: &WebState, user_id: &str, user_login: &str) -> (String, String) {
    let (sid, csrf) = state
        .sessions
        .insert(user_id.to_owned(), user_login.to_owned())
        .expect("insert session");
    let signed_sid = sign_for_tests(state, "tw1337_sid", &sid);
    let signed_csrf = sign_for_tests(state, "tw1337_csrf", &hex::encode(csrf));
    (signed_sid, signed_csrf)
}
```

The form-field csrf carries the **bare** hex (not the signed cookie value) — handlers compare it against `session.csrf_value` directly. So tests should keep one helper that returns the bare csrf for filling the form field, and another that returns the signed value for the cookie header. Simplest pattern:

```rust
/// Returns (signed_sid_for_cookie, signed_csrf_for_cookie, bare_csrf_for_form_field).
pub fn insert_session(state: &WebState, user_id: &str, user_login: &str) -> (String, String, String) {
    let (sid, csrf) = state
        .sessions
        .insert(user_id.to_owned(), user_login.to_owned())
        .expect("insert session");
    let bare_csrf = hex::encode(csrf);
    let signed_sid = sign_for_tests(state, "tw1337_sid", &sid);
    let signed_csrf = sign_for_tests(state, "tw1337_csrf", &bare_csrf);
    (signed_sid, signed_csrf, bare_csrf)
}
```

Update every existing test that destructured `(sid, csrf) = insert_session(...)` to `(sid, csrf, bare_csrf) = insert_session(...)` — keep `csrf` (signed) for the cookie header and use `bare_csrf` for the `_csrf=` form field. This is mechanical; the compiler points out each call site.

- [ ] **Step 2.6: Write a signed-cookie tampering test**

Create `crates/web/tests/auth_signed_cookies.rs`:

```rust
//! Smoke test: a tampered sid cookie is rejected by the signed extractor.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_web::build_router;

mod helpers;
use helpers::{FakeHelix, build_state, install_crypto, insert_session};

#[tokio::test]
async fn tampered_sid_redirects_to_login() {
    install_crypto();
    let state = build_state(std::sync::Arc::new(FakeHelix {
        moderators: vec!["12345".into()],
        users: std::collections::HashMap::new(),
    }))
    .await;
    let (signed_sid, _signed_csrf, _bare_csrf) =
        insert_session(&state, "12345", "alice");

    // Flip a single hex char in the signed sid; the HMAC must fail.
    let mut tampered = signed_sid.clone();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'a' { 'b' } else { 'a' });

    let app = build_router(state);
    let req = Request::builder()
        .uri("/pings")
        .header(header::COOKIE, format!("tw1337_sid={tampered}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "tampered sid must redirect to /login (Unauthenticated)"
    );
    let location = res.headers().get(header::LOCATION).unwrap().to_str().unwrap();
    // ?next= will be added in Task 5; for now, must at least begin with /login.
    assert!(location.starts_with("/login"), "got {location}");
}

#[tokio::test]
async fn untampered_sid_passes_through() {
    install_crypto();
    let state = build_state(std::sync::Arc::new(FakeHelix {
        moderators: vec!["12345".into()],
        users: std::collections::HashMap::new(),
    }))
    .await;
    let (signed_sid, _signed_csrf, _bare_csrf) =
        insert_session(&state, "12345", "alice");

    let app = build_router(state);
    let req = Request::builder()
        .uri("/pings")
        .header(header::COOKIE, format!("tw1337_sid={signed_sid}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}
```

- [ ] **Step 2.7: Run tests**

```bash
cargo nextest run -p twitch-1337-web --show-progress=none --cargo-quiet --status-level=fail
```

Expected: every existing test still passes (the signed/bare destructure refactor is mechanical), plus the two new tampering tests pass.

- [ ] **Step 2.8: Commit**

```bash
git add crates/web/src/state.rs crates/web/src/auth/routes.rs \
        crates/twitch-1337/src/main.rs crates/web/tests/helpers/mod.rs \
        crates/web/tests/auth_signed_cookies.rs \
        crates/web/tests/auth_session.rs crates/web/tests/auth_routes.rs \
        crates/web/tests/auth_csrf.rs crates/web/tests/auth_mod_check.rs \
        crates/web/tests/healthz.rs crates/web/tests/memory_read.rs \
        crates/web/tests/memory_write.rs crates/web/tests/pings_routes.rs
git commit -m "feat(web): sign sid + csrf cookies with session_secret-derived key

Wires \`[web].session_secret\` through to a tower_cookies::Key
(\`Key::derive_from\`, requires >=32 bytes). The OAuth callback writes sid
and csrf via \`cookies.signed(&key)\`; \`require_mod\` and \`logout\` read
them back through the same signed extractor so a tampered cookie is
treated as no cookie at all.

Flash and oauth_state cookies stay unsigned (no auth value).

Test helpers grow a \`sign_for_tests\` factory and \`insert_session\` now
returns (signed_sid, signed_csrf, bare_csrf_for_form_field) — call sites
updated mechanically."
```

---

## Task 3: Form re-render on validation error

**Why:** Today, posting a duplicate ping name returns `400 ping \`foo\` already exists` as plain text — the user loses their typed-in template. Same for memory state writes hitting the byte cap. v2 re-renders the originating form with the user's draft + an inline error instead.

**Approach:** *No new `WebError` variants.* Instead, each handler converts its existing `WebError::Validation` / `WebError::DuplicateName` branches to a direct `render(&FormTpl { ... error: Some(msg) ... })` return with status `400`. CsrfMismatch + Internal still bubble — those don't have a meaningful form to re-render.

**Files:**
- Modify: `crates/web/src/routes/pings.rs`
- Modify: `crates/web/src/routes/memory.rs`
- Test: `crates/web/tests/pings_routes.rs` (extend existing)
- Test: `crates/web/tests/memory_write.rs` (extend existing)

- [ ] **Step 3.1: Helper for "render with status"**

In `crates/web/src/routes/pings.rs`, replace the existing `fn render` with:

```rust
fn render<T: Template>(tpl: &T) -> Result<Response, WebError> {
    render_with(StatusCode::OK, tpl)
}

fn render_with<T: Template>(status: StatusCode, tpl: &T) -> Result<Response, WebError> {
    let body = tpl
        .render()
        .map_err(|e| WebError::Internal(eyre::eyre!("render: {e}")))?;
    Ok((status, Html(body)).into_response())
}
```

Same change in `crates/web/src/routes/memory.rs`. Imports: `use axum::http::StatusCode;` (already imported in `pings.rs` via `axum::http::{HeaderMap, StatusCode}`; add to `memory.rs` if missing).

- [ ] **Step 3.2: Re-render `FormTpl` on `pings::create` errors**

Replace `pings::create` body's error branches:

```rust
async fn create(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    axum::Form(form): axum::Form<CreateForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let name = form.name.trim().to_owned();
    let template = form.template;
    let csrf_hex = csrf::encode(&session.csrf_value);

    let mut mgr = state.ping_manager.write().await;
    if mgr.ping_exists_ignore_case(&name) {
        tracing::info!(
            target: "twitch_1337_web",
            user_id = %session.user_id,
            action = "ping_create",
            target_name = %name,
            result = "duplicate",
        );
        return render_with(
            StatusCode::BAD_REQUEST,
            &FormTpl {
                is_new: true,
                name: &name,
                template_text: &template,
                csrf: &csrf_hex,
                error: Some(format!("ping `{name}` already exists")),
            },
        );
    }
    if let Err(e) = mgr.create_ping(name.clone(), template.clone(), session.user_login.clone(), None) {
        tracing::warn!(
            target: "twitch_1337_web",
            user_id = %session.user_id,
            action = "ping_create",
            target_name = %name,
            result = "validation",
            error = ?e,
        );
        return render_with(
            StatusCode::BAD_REQUEST,
            &FormTpl {
                is_new: true,
                name: &name,
                template_text: &template,
                csrf: &csrf_hex,
                error: Some(e.to_string()),
            },
        );
    }
    drop(mgr);

    tracing::info!(
        target: "twitch_1337_web",
        user_id = %session.user_id,
        action = "ping_create",
        target_name = %name,
        result = "ok",
    );
    flash::set(&cookies, &format!("Created ping `{name}`."));
    Ok(Redirect::to("/pings").into_response())
}
```

Note `template.clone()` so the error branch can still reference it.

- [ ] **Step 3.3: Re-render `FormTpl` on `pings::update` errors**

```rust
async fn update(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    Path(name): Path<String>,
    axum::Form(form): axum::Form<UpdateForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let csrf_hex = csrf::encode(&session.csrf_value);
    let template = form.template;

    let mut mgr = state.ping_manager.write().await;
    if let Err(e) = mgr.edit_template(&name, template.clone()) {
        tracing::warn!(
            target: "twitch_1337_web",
            user_id = %session.user_id,
            action = "ping_update",
            target_name = %name,
            result = "validation",
            error = ?e,
        );
        return render_with(
            StatusCode::BAD_REQUEST,
            &FormTpl {
                is_new: false,
                name: &name,
                template_text: &template,
                csrf: &csrf_hex,
                error: Some(e.to_string()),
            },
        );
    }
    drop(mgr);

    tracing::info!(
        target: "twitch_1337_web",
        user_id = %session.user_id,
        action = "ping_update",
        target_name = %name,
        result = "ok",
    );
    flash::set(&cookies, &format!("Updated ping `{name}`."));
    Ok(Redirect::to("/pings").into_response())
}
```

- [ ] **Step 3.4: Re-render `EditorTpl` in `memory::save_kind` on Full / StateFull / InvalidSlug**

Edit `save_kind` in `crates/web/src/routes/memory.rs`. Add new arg `cap: usize` (the byte cap for the editor) and `delete_url: Option<String>` so the re-render carries them. Replace the `Err(err) => Err(map_write_error(err))` branch with explicit per-variant rendering:

```rust
#[allow(clippy::too_many_arguments)]
async fn save_kind(
    state: &WebState,
    session: &Session,
    cookies: &Cookies,
    kind: FileKind,
    label: String,
    id: String,
    form: SaveForm,
    redirect_to: String,
    cap: usize,
    delete_url: Option<String>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let outcome = state
        .memory_store
        .write_with_guard(kind.clone(), &id, &form.body, Some(form.mtime))
        .await;
    let csrf_hex = csrf::encode(&session.csrf_value);
    match outcome {
        Ok(WriteOutcome::Written { .. }) => {
            tracing::info!(
                target: "twitch_1337_web",
                user_id = %session.user_id,
                action = "memory_write",
                target_label = %label,
                target_id = %id,
                result = "ok",
            );
            flash::set(cookies, &format!("{label} saved"));
            Ok(Redirect::to(&redirect_to).into_response())
        }
        Ok(WriteOutcome::Conflict { current_body, current_mtime }) => {
            tracing::info!(
                target: "twitch_1337_web",
                user_id = %session.user_id,
                action = "memory_write",
                target_label = %label,
                target_id = %id,
                result = "conflict",
            );
            Err(WebError::Conflict(Box::new(ConflictPayload {
                kind: label,
                id,
                current_body,
                current_mtime,
                draft: form.body,
                csrf: csrf_hex,
            })))
        }
        Err(err) => {
            tracing::warn!(
                target: "twitch_1337_web",
                user_id = %session.user_id,
                action = "memory_write",
                target_label = %label,
                target_id = %id,
                result = "error",
                error = ?err,
            );
            // Render the editor with the user's draft preserved. `Io` is the
            // only variant that lacks a meaningful form context — bubble it.
            let msg = match err {
                WriteError::Full => "exceeds byte cap".to_owned(),
                WriteError::StateFull => "state collection full".to_owned(),
                WriteError::InvalidSlug => "reserved or invalid slug".to_owned(),
                WriteError::Io(e) => return Err(WebError::Internal(e)),
            };
            render_with(
                StatusCode::BAD_REQUEST,
                &EditorTpl {
                    title: &label,
                    body: &form.body,
                    csrf: &csrf_hex,
                    mtime: form.mtime,
                    byte_cap: cap,
                    save_url: &redirect_to,
                    delete_url: delete_url.as_deref(),
                    error: Some(msg),
                },
            )
        }
    }
}
```

Update the four callers of `save_kind` (`save_soul`, `save_lore`, `save_user`, `save_state`) to pass the cap from `state.memory_store.caps()` and the appropriate `delete_url`. Caps mapping (already exposed via `state.memory_store.caps()` returning `Caps { soul_bytes, lore_bytes, user_bytes, state_bytes }`):

| Handler | cap field | delete_url |
|---|---|---|
| `save_soul` | `caps.soul_bytes` | `None` |
| `save_lore` | `caps.lore_bytes` | `None` |
| `save_user` | `caps.user_bytes` | `None` |
| `save_state` | `caps.state_bytes` | `Some(format!("/memory/state/{slug}/delete"))` |

`save_kind` no longer needs to call `map_write_error` — delete that helper if no other call site remains; otherwise leave it.

- [ ] **Step 3.5: Re-render the new-state form on `create_state` errors**

The new-state form (rendered by `new_state_form`) currently uses `EditorTpl`. The `create_state` handler bails to plain `WebError::Validation` on bad slugs / byte-cap overruns. Re-render the same `EditorTpl` instead:

```rust
async fn create_state(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    axum::Form(form): axum::Form<CreateStateForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let csrf_hex = csrf::encode(&session.csrf_value);
    let cap = state.memory_store.caps().state_bytes;

    if let Err(e) = validate_slug(&form.slug) {
        return render_state_create_error(&form, &csrf_hex, cap, slug_error_msg(&e));
    }
    let slug = form.slug.clone();
    match state
        .memory_store
        .write_state(
            &FileKind::State { slug: slug.clone() },
            &form.body,
            Some(&session.user_id),
        )
        .await
    {
        Ok(()) => {
            tracing::info!(
                target: "twitch_1337_web",
                user_id = %session.user_id,
                action = "memory_create",
                target_label = "state",
                target_id = %slug,
                result = "ok",
            );
            flash::set(&cookies, &format!("state `{slug}` created"));
            Ok(Redirect::to(&format!("/memory/state/{slug}")).into_response())
        }
        Err(err) => {
            tracing::warn!(
                target: "twitch_1337_web",
                user_id = %session.user_id,
                action = "memory_create",
                target_label = "state",
                target_id = %slug,
                result = "error",
                error = ?err,
            );
            let msg = match err {
                WriteError::Full => "exceeds byte cap".to_owned(),
                WriteError::StateFull => "state collection full".to_owned(),
                WriteError::InvalidSlug => "reserved or invalid slug".to_owned(),
                WriteError::Io(e) => return Err(WebError::Internal(e)),
            };
            render_state_create_error(&form, &csrf_hex, cap, msg)
        }
    }
}

fn slug_error_msg(_e: &WebError) -> String {
    "must be 1-64 chars, [a-zA-Z0-9._-], not `new`/`delete`, no `..`".to_owned()
}

fn render_state_create_error(
    form: &CreateStateForm,
    csrf_hex: &str,
    cap: usize,
    msg: String,
) -> Result<Response, WebError> {
    render_with(
        StatusCode::BAD_REQUEST,
        &EditorTpl {
            title: "new state note",
            body: &form.body,
            csrf: csrf_hex,
            mtime: 0,
            byte_cap: cap,
            save_url: "/memory/state",
            delete_url: None,
            error: Some(msg),
        },
    )
}
```

The `validate_slug` helper signature stays `Result<(), WebError>` so a thin `slug_error_msg` adapter pulls a fresh user-facing string. (You can inline this if you prefer; isolating it keeps the create handler readable.)

- [ ] **Step 3.6: Add tests for re-render behavior**

Append to `crates/web/tests/pings_routes.rs`:

```rust
#[tokio::test]
async fn create_duplicate_renders_form_with_error_and_user_draft() {
    install_crypto();
    let state = build_state(fake_helix_with_mod()).await;
    state
        .ping_manager
        .write()
        .await
        .create_ping("foo".into(), "@user".into(), "alice".into(), None)
        .unwrap();
    let (sid, csrf, bare_csrf) = insert_session(&state, "12345", "alice");

    let app = build_router(state);
    let body = format!(
        "_csrf={bare_csrf}&name=foo&template=%40new-template-text"
    );
    let req = Request::builder()
        .method("POST")
        .uri("/pings")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = String::from_utf8(
        res.into_body().collect().await.unwrap().to_bytes().to_vec(),
    ).unwrap();
    assert!(body.contains("@new-template-text"), "user draft must round-trip into the form");
    assert!(body.contains("already exists"), "error message must render");
}
```

Same shape for `update_invalid_template_renders_form_with_error`. Pick a template that `PingManager::edit_template` will reject (control char `\u{1}` works).

Append to `crates/web/tests/memory_write.rs`:

```rust
#[tokio::test]
async fn save_state_oversized_renders_editor_with_draft() {
    install_crypto();
    let state = build_state(fake_helix_with_mod()).await;
    let (sid, csrf, bare_csrf) = insert_session(&state, "12345", "alice");
    // First create the state note so save (POST /memory/state/<slug>) is valid.
    state
        .memory_store
        .write_state(
            &twitch_1337_core::ai::memory::types::FileKind::State { slug: "notes".into() },
            "hello",
            Some("12345"),
        )
        .await
        .unwrap();

    // 4 KiB+ payload exceeds the 2 KiB state cap.
    let huge = "x".repeat(4096);
    let app = build_router(state);
    let body = format!("_csrf={bare_csrf}&mtime=0&body={}", urlencoding::encode(&huge));
    let req = Request::builder()
        .method("POST")
        .uri("/memory/state/notes")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = String::from_utf8(
        res.into_body().collect().await.unwrap().to_bytes().to_vec(),
    ).unwrap();
    assert!(body.contains("exceeds byte cap"));
    // The user's draft must round-trip (rendered HTML-escaped, but still recognizable).
    assert!(body.contains(&"x".repeat(64)), "draft must survive the re-render");
}
```

`urlencoding` is already a workspace dep used by other tests.

- [ ] **Step 3.7: Run tests**

```bash
cargo nextest run -p twitch-1337-web --show-progress=none --cargo-quiet --status-level=fail
```

Expected: all existing tests still passing + the new re-render tests passing.

- [ ] **Step 3.8: Commit**

```bash
git add crates/web/src/routes/pings.rs crates/web/src/routes/memory.rs \
        crates/web/tests/pings_routes.rs crates/web/tests/memory_write.rs
git commit -m "feat(web): re-render forms with user draft on validation error

Replaces the plain-text 400 response with a re-render of the originating
form template, carrying the user's submitted values plus an inline error
banner. Affects ping create/edit, memory soul/lore/user/state save, and
memory state create. CsrfMismatch and Internal still bubble through
WebError because they don't have a meaningful form to re-render."
```

---

## Task 4: Persistent sidebar in base layout

**Why:** `crates/web/templates/sidebar.html` exists but isn't included anywhere. Pages have no persistent nav.

**Approach:** Every authed template gains three layout fields (`user_login`, `csrf`, `current_page`); `base.html` includes the sidebar inside the body wrapper. The login/denied templates override `{% block layout %}` and don't get a sidebar (no session).

**Files:**
- Modify: `crates/web/templates/base.html`
- Modify: `crates/web/templates/sidebar.html`
- Modify: every `crates/web/templates/{pings,memory}/*.html`
- Modify: every `Tpl` struct in `routes/{pings,memory}.rs`

- [ ] **Step 4.1: Update `base.html` to wrap content with the sidebar**

```html
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>{% block title %}twitch-1337{% endblock %}</title>
  <link rel="stylesheet" href="/assets/pico.min.css">
  <link rel="stylesheet" href="/assets/app.css">
  <script src="/assets/htmx.min.js" defer></script>
  <script src="/assets/app.js" defer></script>
</head>
<body>
  {% block layout %}
  <div class="dashboard">
    {% include "sidebar.html" %}
    <main class="content">
      {% block content %}{% endblock %}
    </main>
  </div>
  {% endblock %}
</body>
</html>
```

- [ ] **Step 4.2: Update `sidebar.html` for current-page highlight**

```html
<aside class="sidebar">
  <header><strong>twitch-1337</strong></header>
  <nav>
    <ul>
      <li{% if current_page == "pings" %} class="active"{% endif %}>
        <a href="/pings">Pings</a>
      </li>
      <li>Memory
        <ul>
          <li{% if current_page == "memory_soul" %} class="active"{% endif %}>
            <a href="/memory/soul">SOUL</a>
          </li>
          <li{% if current_page == "memory_lore" %} class="active"{% endif %}>
            <a href="/memory/lore">LORE</a>
          </li>
          <li{% if current_page == "memory_users" %} class="active"{% endif %}>
            <a href="/memory/users">Users</a>
          </li>
          <li{% if current_page == "memory_state" %} class="active"{% endif %}>
            <a href="/memory/state">State</a>
          </li>
        </ul>
      </li>
    </ul>
  </nav>
  <footer>
    <small>{{ user_login }}</small>
    <form method="post" action="/logout">
      <input type="hidden" name="_csrf" value="{{ csrf }}">
      <button type="submit" class="secondary">Logout</button>
    </form>
  </footer>
</aside>
```

- [ ] **Step 4.3: Add layout fields to every authed template struct**

In `crates/web/src/routes/pings.rs`:

```rust
#[derive(Template)]
#[template(path = "pings/list.html")]
struct ListTpl {
    rows: Vec<RowView>,
    flash: Option<String>,
    csrf: String,
    user_login: String,
    current_page: &'static str,
}

#[derive(Template)]
#[template(path = "pings/form.html")]
struct FormTpl<'a> {
    is_new: bool,
    name: &'a str,
    template_text: &'a str,
    csrf: &'a str,
    error: Option<String>,
    user_login: &'a str,
    current_page: &'static str,
}
```

In `crates/web/src/routes/memory.rs`:

```rust
#[derive(Template)]
#[template(path = "memory/tree.html")]
struct TreeTpl {
    user_count: usize,
    state_count: usize,
    csrf: String,
    user_login: String,
    current_page: &'static str,
}

#[derive(Template)]
#[template(path = "memory/editor.html")]
struct EditorTpl<'a> {
    title: &'a str,
    body: &'a str,
    csrf: &'a str,
    mtime: u64,
    byte_cap: usize,
    save_url: &'a str,
    delete_url: Option<&'a str>,
    error: Option<String>,
    user_login: &'a str,
    current_page: &'static str,
}

#[derive(Template)]
#[template(path = "memory/state_list.html")]
struct StateListTpl {
    items: Vec<StateRow>,
    csrf: String,
    user_login: String,
    current_page: &'static str,
}

#[derive(Template)]
#[template(path = "memory/users_list.html")]
struct UsersListTpl {
    items: Vec<UserRow>,
    csrf: String,
    user_login: String,
    current_page: &'static str,
}
```

The conflict template gets the same treatment.

- [ ] **Step 4.4: Pass session info into every render**

Each handler that already takes `Extension(session)` adds the layout fields when constructing its `Tpl`:

```rust
ListTpl {
    rows,
    flash: flash::take(&cookies),
    csrf: csrf::encode(&session.csrf_value),
    user_login: session.user_login.clone(),
    current_page: "pings",
}
```

`FormTpl`:

```rust
FormTpl {
    is_new: true,
    name: "",
    template_text: "",
    csrf: &csrf_hex,
    error: None,
    user_login: &session.user_login,
    current_page: "pings",
}
```

`view_kind` accepts an extra `current_page: &'static str` parameter and threads it into `EditorTpl`. Per-caller mapping:

| caller | current_page |
|---|---|
| `view_soul` | `"memory_soul"` |
| `view_lore` | `"memory_lore"` |
| `view_user` | `"memory_users"` |
| `view_state` / `new_state_form` | `"memory_state"` |
| `tree` | `"memory"` |
| `list_users` | `"memory_users"` |
| `list_state` | `"memory_state"` |

- [ ] **Step 4.5: Bare-minimum CSS for the dashboard layout**

`crates/web/assets/app.css`:

```css
.dashboard { display: grid; grid-template-columns: 16rem 1fr; min-height: 100vh; }
.sidebar { padding: 1rem; border-right: 1px solid var(--pico-muted-border-color, #ddd); }
.sidebar nav ul { list-style: none; padding-left: 0; }
.sidebar nav ul ul { padding-left: 1rem; }
.sidebar li.active > a { font-weight: bold; }
.content { padding: 1rem 2rem; }
.flash { padding: 0.5rem 1rem; background: var(--pico-color-azure-100, #e8f4fd); border-radius: 0.25rem; margin-bottom: 1rem; }
.error { padding: 0.5rem 1rem; background: var(--pico-color-pumpkin-100, #fde8e0); border-radius: 0.25rem; margin-bottom: 1rem; }
```

(Append this to whatever's already in `app.css`. The pico-color variables come from pico v2's design tokens — degrade gracefully via the fallbacks if a variable is missing.)

- [ ] **Step 4.6: Sidebar smoke test**

Create `crates/web/tests/sidebar_smoke.rs`:

```rust
//! Sidebar always renders on authed pages and highlights the current page.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use twitch_1337_web::build_router;

mod helpers;
use helpers::{build_state, cookie_header, fake_helix_with_mod, insert_session, install_crypto};

async fn fetch_authed(uri: &str) -> String {
    install_crypto();
    let state = build_state(fake_helix_with_mod()).await;
    let (sid, csrf, _bare) = insert_session(&state, "12345", "alice");
    let app = build_router(state);
    let req = Request::builder()
        .uri(uri)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "{uri}: status");
    String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

#[tokio::test]
async fn pings_page_renders_sidebar_with_active_pings() {
    let body = fetch_authed("/pings").await;
    assert!(body.contains("twitch-1337"));
    assert!(body.contains("alice"), "sidebar must show user_login");
    assert!(body.contains("class=\"active\""), "current-page highlight missing");
    // Crude: the active class must be on a li that links to /pings.
    let snippet = body
        .split("class=\"active\"")
        .nth(1)
        .expect("active class present");
    assert!(snippet.contains("/pings"), "active li should be the pings entry");
}

#[tokio::test]
async fn memory_state_page_highlights_state() {
    let body = fetch_authed("/memory/state").await;
    let snippet = body
        .split("class=\"active\"")
        .nth(1)
        .expect("active class present");
    assert!(snippet.contains("/memory/state"));
}
```

- [ ] **Step 4.7: Run tests**

```bash
cargo nextest run -p twitch-1337-web --show-progress=none --cargo-quiet --status-level=fail
```

Expected: existing tests pass (the assertion bodies don't care about the new layout HTML), new sidebar tests pass.

- [ ] **Step 4.8: Commit**

```bash
git add crates/web/templates/base.html crates/web/templates/sidebar.html \
        crates/web/templates/pings/*.html crates/web/templates/memory/*.html \
        crates/web/src/routes/pings.rs crates/web/src/routes/memory.rs \
        crates/web/assets/app.css crates/web/tests/sidebar_smoke.rs
git commit -m "feat(web): persistent sidebar across authed pages

base.html now wraps {% block content %} with a sidebar include; every
authed template struct gains user_login + csrf + current_page so the
sidebar can render the user identity, logout form, and active-page
highlight without a per-route partial. Login + denied keep their
{% block layout %} override (no session, no sidebar)."
```

---

## Task 5: Post-login `?next=` deep-link

**Why:** Currently every successful login lands on `/`, so a user who clicked `/memory/state/notes/edit` and got bounced to login lands on the wrong page. v2 captures the original path and redirects after callback.

**Approach:**
1. `WebError::Unauthenticated` carries `Option<String>` next-path; the middleware fills it from `req.uri()` before bailing.
2. The `IntoResponse` impl emits `/login?next=<encoded>` instead of bare `/login`.
3. The `login` handler reads `?next=`, validates it (must start with `/`, must not contain `://`, no `\r\n`, max 256 bytes), and stashes it in a short-lived `tw1337_next` cookie alongside `tw1337_oauth_state`.
4. The `callback` handler reads the `tw1337_next` cookie post-mod-check and redirects to it (validated again defensively); falls back to `/` when missing.

**Files:**
- Modify: `crates/web/src/error.rs`
- Modify: `crates/web/src/auth/routes.rs`
- Modify: `crates/web/src/auth/mod.rs` (re-export of `require_mod` is fine; no change here)
- Test: `crates/web/tests/auth_next.rs` (new)

- [ ] **Step 5.1: Add the next path to `WebError::Unauthenticated`**

Edit `crates/web/src/error.rs`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    /// User has no valid session. Redirects to `/login`. The optional `next`
    /// captures the requested path so the callback can return there after
    /// successful login.
    #[error("unauthenticated; redirect to login")]
    Unauthenticated { next: Option<String> },
    // ... rest unchanged
}
```

Update the `IntoResponse` impl arm:

```rust
WebError::Unauthenticated { next } => {
    if let Some(path) = next.filter(|p| is_safe_redirect(p)) {
        Redirect::to(&format!(
            "/login?next={}",
            urlencoding::encode(&path)
        ))
        .into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}
```

And add the validator (used both here and in the `login` handler):

```rust
/// Allow only same-origin absolute paths. Anything that smells like a
/// scheme, host, or CRLF is rejected so the redirect can't be turned into
/// an open-redirect or header-splitting vector.
pub(crate) fn is_safe_redirect(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 256
        && !path.starts_with("//")
        && !path.contains("://")
        && !path.contains(['\r', '\n'])
}
```

Make `is_safe_redirect` `pub(crate)` so `auth/routes.rs` can reuse it.

- [ ] **Step 5.2: Update `require_mod` to capture the next path**

In `crates/web/src/auth/routes.rs::require_mod`:

```rust
pub async fn require_mod(
    State(state): State<WebState>,
    cookies: Cookies,
    mut req: Request,
    next: Next,
) -> Result<Response, WebError> {
    let captured_next = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned());
    let unauth = || WebError::Unauthenticated { next: captured_next.clone() };

    let sid_cookie = cookies
        .signed(&state.signed_key)
        .get(SID_COOKIE)
        .ok_or_else(unauth)?;
    let session = state
        .sessions
        .get_and_touch(sid_cookie.value())
        .ok_or_else(unauth)?;
    // ... rest unchanged (mod refresh logic)
```

Two usages of `unauth()` — both via `ok_or_else` so the closure is re-callable.

- [ ] **Step 5.3: Update `login` to accept `?next=` + stash it in a cookie**

```rust
#[derive(Deserialize)]
struct LoginParams {
    next: Option<String>,
}

const NEXT_COOKIE: &str = "tw1337_next";

async fn login(
    State(state): State<WebState>,
    Query(params): Query<LoginParams>,
    cookies: Cookies,
) -> Response {
    let csrf = CsrfToken::new_random();
    cookies.add(
        Cookie::build((OAUTH_STATE_COOKIE, csrf.secret().to_owned()))
            .http_only(true)
            .secure(true)
            .same_site(SameSite::Lax)
            .path("/")
            .max_age(time::Duration::minutes(10))
            .build(),
    );
    if let Some(path) = params.next.as_deref() {
        if crate::error::is_safe_redirect(path) {
            cookies.add(
                Cookie::build((NEXT_COOKIE, path.to_owned()))
                    .http_only(true)
                    .secure(true)
                    .same_site(SameSite::Lax)
                    .path("/")
                    .max_age(time::Duration::minutes(10))
                    .build(),
            );
        }
    }
    let csrf_for_url = csrf.clone();
    // ... rest of the function unchanged
```

- [ ] **Step 5.4: Update `callback` to consume the next cookie**

Replace the final `Ok(Redirect::to("/").into_response())` with:

```rust
    let next_path = cookies
        .get(NEXT_COOKIE)
        .map(|c| c.value().to_owned())
        .filter(|p| crate::error::is_safe_redirect(p))
        .unwrap_or_else(|| "/".to_owned());
    cookies.remove(Cookie::build(NEXT_COOKIE).path("/").build());

    tracing::info!(target: "twitch_1337_web", user_id=%me.id, user_login=%me.login, action="login", result="ok");
    Ok(Redirect::to(&next_path).into_response())
```

- [ ] **Step 5.5: Tests**

Create `crates/web/tests/auth_next.rs`:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_web::build_router;
use twitch_1337_web::error::is_safe_redirect;

mod helpers;
use helpers::{build_state, fake_helix, install_crypto};

#[test]
fn safe_redirect_rejects_scheme_and_host() {
    assert!(is_safe_redirect("/pings"));
    assert!(is_safe_redirect("/memory/state/notes"));
    assert!(!is_safe_redirect("//evil.example/x"));
    assert!(!is_safe_redirect("https://evil.example/"));
    assert!(!is_safe_redirect("javascript:alert(1)"));
    assert!(!is_safe_redirect("/path\r\nSet-Cookie: x=1"));
    assert!(!is_safe_redirect(&"/".repeat(257)));
}

#[tokio::test]
async fn unauth_request_redirects_to_login_with_next() {
    install_crypto();
    let state = build_state(fake_helix()).await;
    let app = build_router(state);

    let req = Request::builder()
        .uri("/memory/state/notes")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let location = res.headers().get(header::LOCATION).unwrap().to_str().unwrap();
    assert!(
        location.starts_with("/login?next=") && location.contains("memory"),
        "expected next=memory deep-link, got {location}"
    );
}

#[tokio::test]
async fn login_with_unsafe_next_drops_it_silently() {
    install_crypto();
    let state = build_state(fake_helix()).await;
    let app = build_router(state);

    let req = Request::builder()
        .uri("/login?next=https://evil.example/x")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    // Login still redirects to twitch authorize; the unsafe next must NOT
    // appear as a Set-Cookie value.
    let set_cookie = res
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !set_cookie.contains("tw1337_next="),
        "unsafe next must not be stashed; saw: {set_cookie}"
    );
}
```

The callback round-trip (next consumed → redirect to that path) needs a wiremock + cookie injection setup that the existing `auth_routes.rs` test doesn't have. Defer that test path until live smoke testing — the unit tests above cover validation + the middleware capture path, which is the security-critical part.

- [ ] **Step 5.6: Run tests**

```bash
cargo nextest run -p twitch-1337-web --show-progress=none --cargo-quiet --status-level=fail
```

Expected: all green, including the three new tests.

- [ ] **Step 5.7: Commit**

```bash
git add crates/web/src/error.rs crates/web/src/auth/routes.rs \
        crates/web/tests/auth_next.rs
git commit -m "feat(web): post-login ?next= deep-link redirect

require_mod captures the requested path on Unauthenticated; IntoResponse
encodes it into /login?next=<path>. /login validates the param via
is_safe_redirect (only same-origin absolute paths, length <=256, no
scheme/host/CRLF) and stashes the safe value in a short-lived
tw1337_next cookie. The OAuth callback consumes + clears the cookie and
redirects there instead of the hardcoded /."
```

---

## Task 6: Helix paginator consolidation

**Why:** After the simplify pass, `helix::is_moderator` and `mod_check::is_moderator_with_user_token` are both single-call (no pagination), but still have two near-identical 25-line implementations differing only in which token is bearer.

**Approach:** Free function `helix_moderator_check` in `helix.rs` does the actual HTTP call with an explicit bearer token. Both call sites delegate.

**Files:**
- Modify: `crates/web/src/helix.rs`
- Modify: `crates/web/src/auth/mod_check.rs`

- [ ] **Step 6.1: Extract the shared helper**

Append to `crates/web/src/helix.rs`:

```rust
/// Single helix moderator-list call filtered by `user_id`. Returns true iff
/// `user_id` is a moderator of `broadcaster_id`. Used by both
/// `ReqwestHelixClient::is_moderator` (bot token) and the OAuth callback's
/// `is_moderator_with_user_token` (user token); the only difference is the
/// bearer.
pub async fn helix_moderator_check(
    http: &reqwest::Client,
    helix_base: &str,
    client_id: &str,
    bearer_token: &str,
    broadcaster_id: &str,
    user_id: &str,
) -> Result<bool> {
    #[derive(Deserialize)]
    struct ModEntry {}
    #[derive(Deserialize)]
    struct ModResp {
        data: Vec<ModEntry>,
    }
    let mut url = url::Url::parse(&format!("{helix_base}/helix/moderation/moderators"))?;
    url.query_pairs_mut()
        .append_pair("broadcaster_id", broadcaster_id)
        .append_pair("user_id", user_id);
    let resp: ModResp = http
        .get(url)
        .bearer_auth(bearer_token)
        .header("Client-Id", client_id)
        .send()
        .await?
        .error_for_status()
        .wrap_err("helix moderators")?
        .json()
        .await?;
    Ok(!resp.data.is_empty())
}
```

- [ ] **Step 6.2: Replace the impl in `ReqwestHelixClient::is_moderator`**

```rust
    async fn is_moderator(&self, broadcaster_id: &str, user_id: &str) -> Result<bool> {
        let token = self.access_token_provider.current_access_token().await?;
        helix_moderator_check(
            &self.http,
            &self.helix_base,
            self.client_id.expose_secret(),
            &token,
            broadcaster_id,
            user_id,
        )
        .await
    }
```

- [ ] **Step 6.3: Replace `is_moderator_with_user_token` in `mod_check.rs`**

```rust
async fn is_moderator_with_user_token(
    user_id: &str,
    access_token: &str,
    broadcaster_id: &str,
    state: &WebState,
) -> eyre::Result<bool> {
    crate::helix::helix_moderator_check(
        &state.oauth.http,
        "https://api.twitch.tv",
        state.client_id.expose_secret(),
        access_token,
        broadcaster_id,
        user_id,
    )
    .await
}
```

Drop the now-unused `serde::Deserialize` and `eyre::WrapErr` imports if they're no longer referenced.

- [ ] **Step 6.4: Run tests**

```bash
cargo nextest run -p twitch-1337-web --show-progress=none --cargo-quiet --status-level=fail
```

Expected: `helix_pagination.rs` (now misnamed but still valid) still passes — the wiremock matchers were already updated in the simplify pass to expect `?user_id=` filter.

- [ ] **Step 6.5: Rename the test file for accuracy**

```bash
git mv crates/web/tests/helix_pagination.rs crates/web/tests/helix_moderator_check.rs
```

- [ ] **Step 6.6: Commit**

```bash
git add crates/web/src/helix.rs crates/web/src/auth/mod_check.rs \
        crates/web/tests/helix_moderator_check.rs
git commit -m "refactor(web): collapse helix moderator-check into one helper

Both is_moderator paths (bot token via ReqwestHelixClient, user token
via the OAuth callback) shared the same ~25 lines of query
construction + reqwest builder + Resp deser, differing only in which
bearer token went into the Authorization header. Lift the body into a
free helix_moderator_check helper; both call sites become 8-line
wrappers."
```

---

## Wrap-up

- [ ] **Step W.1: Final fmt + clippy + test**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo nextest run --workspace --show-progress=none --cargo-quiet --status-level=fail
```

Expected: all clean.

- [ ] **Step W.2: Push branch + open PR**

```bash
git push -u origin feature/web-dashboard-v2
gh pr create --title "feat(web): v2 dashboard — signed cookies, real bundles, sidebar, ?next=" \
  --body "$(cat <<'EOF'
## Summary

Closes the v1 forward-debt list:

- Real htmx 2.0.4 + pico 2.0.6 bundles (replaces 190-byte placeholders)
- Signed sid + csrf cookies via `[web].session_secret`-derived `tower_cookies::Key`
- Form re-render on validation/duplicate errors (preserves user draft)
- Persistent sidebar in base.html with current-page highlight
- Post-login `?next=` deep-link with same-origin path validation
- Helix moderator-check consolidated to a single helper

Plan: `docs/superpowers/plans/2026-05-09-web-dashboard-v2.md`

## Test plan

- [ ] `cargo nextest run --workspace`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] CI 7 required checks
- [ ] Manual: log in, deep-link to `/memory/state/foo` while logged out, verify post-login redirect lands there
- [ ] Manual: tamper with sid cookie value, verify silent redirect to /login (no panic)
- [ ] Manual: submit duplicate ping name, verify form re-renders with the typed template intact

## Deferred to v3

- Memory edit audit log
- Production-grade form-error type system (this PR localises re-render at the handler level rather than restructuring `WebError`)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-Review Notes

- **Spec coverage:** All six locked scope items have a dedicated task. Out-of-scope items (audit log, production-grade error type) explicitly deferred in the wrap-up.
- **Type consistency:** `validate_state_slug` (Task 3 reference) matches the post-simplify symbol exported from `core::ai::memory::store`. `Key::derive_from` (Task 2) is the documented `tower_cookies::Key` constructor accepting any byte slice ≥32 bytes. `is_safe_redirect` (Task 5) is referenced consistently from `error.rs` + `auth/routes.rs`. `helix_moderator_check` (Task 6) signature is identical at definition and both call sites.
- **Task ordering:** Task 1 (bundles) is independent. Task 2 (signed cookies) must precede Tasks 3–5 because their tests use the new helper signature. Task 4 (sidebar) and Task 5 (`?next=`) are independent of each other but both depend on Task 2's helper changes. Task 6 is independent.
- **No placeholders:** No "TBD" / "implement later" / "similar to Task N" strings in the plan body.
