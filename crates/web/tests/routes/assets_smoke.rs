//! Smoke tests for the `/assets/*` route. Ensures the htmx bundle is real
//! (not the development stub) and that assets ship with the expected
//! cache-control header so deploy reviews catch a regression to placeholders.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_web::build_router;
use twitch_1337_web::helix::HelixClient;

use super::helpers;
use helpers::{FakeHelix, build_state, install_crypto};

fn fake_helix() -> Arc<dyn HelixClient> {
    Arc::new(FakeHelix {
        moderators: vec![],
        users: HashMap::new(),
    })
}

async fn fetch_asset(path: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    install_crypto();
    let state = build_state(fake_helix()).await;
    let app = build_router(state);
    let req = Request::builder().uri(path).body(Body::empty()).unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

#[tokio::test]
async fn htmx_bundle_is_real_not_a_stub() {
    let (status, _headers, bytes) = fetch_asset("/assets/htmx.min.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        bytes.len() > 40_000,
        "htmx bundle suspiciously small: {} bytes",
        bytes.len()
    );
    let s = std::str::from_utf8(&bytes).expect("htmx must be utf-8");
    // The v1 stub printed "htmx placeholder; templates render but interactivity
    // is stubbed" via console.log. The real bundle never carries that phrase.
    assert!(
        !s.contains("htmx placeholder"),
        "htmx still contains the dev stub"
    );
    assert!(
        !s.contains("TODO: replace"),
        "htmx still contains a TODO stub marker"
    );
    assert!(s.contains("htmx"), "htmx bundle must mention itself");
}

#[tokio::test]
async fn self_hosted_fonts_are_served() {
    for path in [
        "/assets/fonts/geist-latin.woff2",
        "/assets/fonts/geist-mono-latin.woff2",
    ] {
        let (status, headers, bytes) = fetch_asset(path).await;
        assert_eq!(status, StatusCode::OK, "{path} must serve");
        assert!(
            bytes.len() > 5_000,
            "{path} suspiciously small: {} bytes",
            bytes.len()
        );
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "font/woff2",
            "{path} must report woff2 mime",
        );
    }
}

#[tokio::test]
async fn settings_dirty_tracker_registers_select_rows() {
    // Regression guard: the settings dirty-tracker collects each
    // `.settings-row`'s primary form control by querying
    // `input[name], select[name], textarea[name]`. A row whose only
    // control is a <select> (the AI · Dreamer/Grok/Media profile pickers)
    // silently drops out of the tracker if `select[name]` isn't in that
    // list — the save bar's visibility is gated on the dirty count, so a
    // profile-only edit becomes unsaveable. There's no JS test harness in
    // this repo, so this checks the shipped selector string directly.
    let (status, _headers, bytes) = fetch_asset("/assets/app.js").await;
    assert_eq!(status, StatusCode::OK);
    let s = std::str::from_utf8(&bytes).expect("app.js must be utf-8");
    assert!(
        s.contains("input[name], select[name], textarea[name]"),
        "settings dirty-tracker must register <select> rows, not just input/textarea",
    );
}

#[tokio::test]
async fn assets_emit_immutable_cache_control() {
    let (status, headers, _bytes) = fetch_asset("/assets/app.css").await;
    assert_eq!(status, StatusCode::OK);
    let cc = headers
        .get(header::CACHE_CONTROL)
        .unwrap()
        .to_str()
        .unwrap();
    // Debug builds emit `no-store` so asset edits show up without a
    // rebuild; release builds emit the production `immutable` policy.
    if cfg!(debug_assertions) {
        assert_eq!(cc, "no-store", "debug build expects no-store");
    } else {
        assert!(
            cc.contains("immutable"),
            "expected immutable cache-control, got `{cc}`"
        );
    }
}

/// Templates carry no `style=""` attributes or `lang="de"` on inputs: inline
/// styles block a strict CSP, and the lang hint never changed Chromium's
/// date/time pickers.
#[test]
fn templates_have_no_inline_styles() {
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for (i, line) in text.lines().enumerate() {
                if line.contains("style=\"") || line.contains("lang=\"de\"") {
                    out.push(format!("{}:{}: {line}", path.display(), i + 1));
                }
            }
        }
    }
    let mut hits = Vec::new();
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates"),
        &mut hits,
    );
    assert!(
        hits.is_empty(),
        "inline style / lang attrs:\n{}",
        hits.join("\n")
    );
}
