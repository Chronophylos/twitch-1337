#![allow(clippy::unwrap_used, reason = "test crate: a panic is a test failure")]
//! Browser-level e2e tests for the dashboard. Compiled only under
//! `--features e2e` (see Cargo.toml `[[test]]`). Needs a running
//! chromedriver at `$WEBDRIVER_URL` (default http://localhost:9515).

#[path = "e2e_support/harness.rs"]
mod harness;

use harness::E2eSession;

#[tokio::test]
async fn harness_boots_and_authenticates() {
    let s = E2eSession::start().await;
    s.goto("/pings").await;
    // Authenticated mod session lands on the pings page with the sidebar.
    s.wait_for("aside.sidebar").await;
    let html = s.source().await;
    assert!(
        html.contains("Pings"),
        "sidebar/page did not render; first 200 chars: {html:.200}"
    );
    s.close().await;
}

/// Every sidebar nav link loads its page without bouncing to login.
#[tokio::test]
async fn nav_links_all_load_authenticated() {
    let s = E2eSession::start().await;
    s.goto("/pings").await;
    s.wait_for("aside.sidebar").await;

    // Hrefs from templates/sidebar.html.
    // NOTE: /settings is owner-gated (require_owner middleware) and the
    // harness session is Role::Mod with no owner configured in the stub state
    // (state.owner = None). A Mod session returns 403 on GET /settings —
    // confirmed by settings_route.rs::non_owner_get_settings_returns_403.
    // Excluded here; it is exercised in settings_route.rs integration tests.
    let paths = [
        "/pings",
        "/leaderboard",
        "/flights",
        "/schedules",
        "/memory/soul",
        "/memory/lore",
        "/memory/users",
        "/memory/scratch",
    ];
    for path in paths {
        s.goto(path).await;
        // Authenticated: stays on the path, not redirected to /auth/login.
        let url = s.current_url().await;
        assert!(url.ends_with(path), "expected to land on {path}, got {url}");
        // Sidebar present => layout rendered, not an error page.
        s.wait_for("aside.sidebar").await;
    }
    s.close().await;
}

/// Create a ping (form POST), add a member (form POST), then delete it
/// (htmx outerHTML swap). Exercises both the form-nav and htmx paths.
#[tokio::test]
async fn pings_create_add_member_delete() {
    let s = E2eSession::start().await;

    // Create.
    s.goto("/pings/new").await;
    s.wait_and_fill("#ping-name", "squad").await;
    s.fill("#ping-template", "yo {mentions}").await;
    s.click("button[type=\"submit\"].btn.primary").await;

    // Back on the list, the row exists.
    s.wait_for("a.ping-name").await;
    let mut names = Vec::new();
    for el in s.find_all("a.ping-name").await {
        names.push(el.text().await.unwrap_or_default());
    }
    assert!(
        names.iter().any(|n| n == "squad"),
        "ping row missing: {names:?}"
    );

    // Add a member from the edit page.
    s.goto("/pings/squad").await;
    s.wait_and_fill("#ping-add-member", "alice").await;
    s.click("form[action=\"/pings/squad/members\"] button[type=\"submit\"]")
        .await;
    s.wait_for(".ping-name").await;
    assert!(
        s.source().await.contains("alice"),
        "member not shown after add"
    );

    // Delete from the list via htmx outerHTML swap (confirm auto-accepted).
    s.goto("/pings").await;
    let del_btn = "button.row-act.danger[hx-post=\"/pings/squad/delete\"]";
    s.wait_and_click(del_btn).await;
    s.wait_gone(del_btn).await;

    s.close().await;
}

/// A failing htmx delete (forged CSRF token) shows the server's plain-text
/// error in the toast and leaves the row in place.
#[tokio::test]
async fn failing_htmx_delete_shows_toast() {
    let s = E2eSession::start().await;

    s.goto("/pings/new").await;
    s.wait_and_fill("#ping-name", "doomed").await;
    s.fill("#ping-template", "yo {mentions}").await;
    s.click("button[type=\"submit\"].btn.primary").await;
    s.wait_for("a.ping-name").await;

    let del_btn = "button.row-act.danger[hx-post=\"/pings/doomed/delete\"]";
    s.wait_for(del_btn).await;
    s.exec(&format!(
        "document.querySelector('{del_btn}').setAttribute('hx-headers', '{{\"X-Csrf-Token\": \"forged\"}}');"
    ))
    .await;
    s.click(del_btn).await;

    let toast = s.wait_text("#toast .toast-msg").await;
    assert_eq!(toast, "Session expired, reload and try again");
    s.find(del_btn).await;

    s.click("#toast .toast-close").await;
    s.wait_gone("#toast .toast").await;

    s.close().await;
}

/// Create an interval schedule (enabled by default), toggle it off, delete it.
#[tokio::test]
async fn schedules_create_toggle_delete() {
    let s = E2eSession::start().await;

    // Create (interval kind is the default checked radio).
    s.goto("/schedules?new=true").await;
    s.wait_and_fill("input[name=\"name\"]", "standup").await;
    s.fill("textarea[name=\"message\"]", "daily standup time")
        .await;
    s.fill("input[name=\"interval_every\"]", "01:00").await;
    // `enabled` left untouched => the new-schedule default (checked) applies.
    s.click("button[type=\"submit\"].btn.primary").await;

    // Card appears, active. CSS `text-transform` may uppercase the rendered
    // "off"/"on", so normalise to lowercase before comparing.
    let toggle_btn = "article.schedule-card form.card-toggle button.toggle-switch";
    let before = s.wait_text(toggle_btn).await.to_lowercase();
    assert_eq!(before, "on", "new schedule should start enabled");

    // Toggle off (full form POST + redirect to /schedules). The paused card
    // only exists after the reload, so waiting for it confirms the POST.
    s.click(toggle_btn).await;
    s.wait_for("article.schedule-card.paused").await;
    let after = s.wait_text(toggle_btn).await.to_lowercase();
    assert_eq!(after, "off", "toggle did not disable the schedule");

    // Delete (JS confirm auto-accepted).
    s.click("form.js-delete-schedule[data-name=\"standup\"] button.chip.danger")
        .await;
    s.wait_gone("form.js-delete-schedule[data-name=\"standup\"]")
        .await;

    s.close().await;
}

/// Change a numeric setting, save (full form POST), reload, assert persisted.
#[tokio::test]
async fn settings_save_cooldown() {
    let s = E2eSession::start_as_owner().await;
    s.goto("/settings").await;

    // Pick a value inside [1, 3600] unlikely to be the default.
    s.wait_and_fill("input[name=\"cooldown_ai\"]", "47").await;
    s.click("button[type=\"submit\"].btn.primary[form=\"settings-form\"]")
        .await;

    // Reload settings and confirm the saved value renders back.
    s.goto("/settings").await;
    let el = s.wait_for("input[name=\"cooldown_ai\"]").await;
    let val = el
        .prop("value")
        .await
        .unwrap_or_default()
        .unwrap_or_default();
    assert_eq!(val, "47", "cooldown_ai did not persist");

    s.close().await;
}

/// Discard restores the load-time value in place. A reload would let
/// Firefox's form restoration put the edit straight back, so the page must
/// survive the click (the marker set before it is still there).
#[tokio::test]
async fn settings_discard_restores_baseline() {
    let s = E2eSession::start_as_owner().await;
    s.goto("/settings").await;

    let input = "input[name=\"cooldown_ai\"]";
    let baseline = s
        .wait_for(input)
        .await
        .prop("value")
        .await
        .unwrap_or_default()
        .unwrap_or_default();
    assert_ne!(baseline, "47", "fixture default collides with the edit");

    s.fill(input, "47").await;
    s.wait_for("#settings-save-bar.visible").await;
    s.exec("window.__e2eMarker = true;").await;

    s.click("#settings-save-bar [data-discard]").await;

    s.wait_gone("#settings-save-bar.visible").await;
    let restored = s
        .find(input)
        .await
        .prop("value")
        .await
        .unwrap_or_default()
        .unwrap_or_default();
    assert_eq!(restored, baseline, "discard must restore the baseline");
    assert_eq!(
        s.eval("return window.__e2eMarker === true;").await,
        serde_json::Value::Bool(true),
        "discard must not reload the page"
    );

    s.close().await;
}

/// Every visible settings control has an accessible name (its row key), and
/// controls are shown at rest rather than behind a hover-only overlay.
#[tokio::test]
async fn settings_controls_have_accessible_names() {
    let s = E2eSession::start_as_owner().await;
    s.goto("/settings").await;
    s.wait_for("#settings-form").await;

    let controls = s
        .find_all("#settings-form .settings-row :is(input:not([type=hidden]), select, textarea)")
        .await;
    assert!(controls.len() > 50, "settings form lost its controls");
    let mut unnamed = Vec::new();
    for el in &controls {
        let (role, name) = s.accessibility(el).await;
        if name.trim().is_empty() {
            let id = el.attr("id").await.ok().flatten().unwrap_or_default();
            unnamed.push(format!("{role} #{id}"));
        }
    }
    assert!(
        unnamed.is_empty(),
        "controls without an accessible name: {unnamed:?}"
    );

    // Query by role and name: a number row, a switch, a bytes row's visible
    // amount (its posted value lives in a hidden input), and a select.
    for (css, role, name) in [
        ("#cooldown_ai", "spinbutton", "ai"),
        ("#alerts_quake_enabled", "switch", "enabled"),
        ("#ai_memory_soul_bytes_display", "spinbutton", "soul_bytes"),
        ("#ai_aurora_profile", "combobox", "ai.aurora.profile"),
    ] {
        let got = s.accessibility(&s.find(css).await).await;
        assert_eq!(got, (role.to_owned(), name.to_owned()), "{css}");
    }

    // Resting state: no hover, no focus — the control itself is visible.
    assert_eq!(
        s.eval(
            "document.activeElement.blur(); \
             return getComputedStyle(document.querySelector('#cooldown_ai').closest('.num-wrap')).opacity;"
        )
        .await,
        serde_json::Value::String("1".into()),
    );

    s.close().await;
}

/// Ctrl+S in the memory editor submits the form and the edit persists.
#[tokio::test]
async fn memory_editor_ctrl_s_saves() {
    use fantoccini::key::Key;

    let s = E2eSession::start().await;
    s.goto("/memory/lore").await;

    let body = "textarea[name=\"body\"]";
    s.wait_and_fill(body, "saved via keyboard shortcut").await;
    // Non-Mac platform in headless Chrome, so the hint reads Ctrl.
    assert_eq!(s.wait_text("kbd[data-mod-key]").await, "Ctrl");
    s.exec("window.__e2eMarker = true;").await;

    s.find(body)
        .await
        .send_keys(&(Key::Control + "s"))
        .await
        .expect("send ctrl+s");

    // The marker disappears once the POST navigated to a fresh document.
    s.wait_js("return document.readyState === 'complete' && window.__e2eMarker === undefined;")
        .await;
    s.goto("/memory/lore").await;
    let saved = s
        .wait_for(body)
        .await
        .prop("value")
        .await
        .unwrap_or_default()
        .unwrap_or_default();
    assert!(
        saved.contains("saved via keyboard shortcut"),
        "Ctrl+S did not save; body was {saved:?}"
    );

    s.close().await;
}

/// Every authed route at phone width (390×844) fits the viewport: no
/// horizontal page scroll, and no inner scroller wider than its box.
#[tokio::test]
async fn phone_width_routes_have_no_horizontal_scroll() {
    let s = E2eSession::start_as_owner().await;

    // A row with a long unbreakable name, so the stacked pings table has
    // real content to squeeze.
    s.goto("/pings/new").await;
    s.wait_and_fill("#ping-name", "a_very_long_ping_name_that_never_breaks")
        .await;
    s.fill(
        "#ping-template",
        "yo {mentions} lets go raid somewhere fun tonight",
    )
    .await;
    s.click("button[type=\"submit\"].btn.primary").await;
    s.wait_for("a.ping-name").await;

    s.set_window_size(390, 844).await;
    let paths = [
        "/pings",
        "/pings/new",
        "/pings/a_very_long_ping_name_that_never_breaks",
        "/leaderboard",
        "/flights",
        "/schedules",
        "/schedules?new=true",
        "/memory",
        "/memory/soul",
        "/memory/lore",
        "/memory/users",
        "/memory/users/new",
        "/memory/scratch",
        "/memory/scratch/new",
        "/emotes",
        "/emotes/audit",
        "/emotes/usage",
        "/logs",
        "/ai-profiles",
        "/settings",
    ];
    for path in paths {
        s.goto(path).await;
        s.wait_for(".topbar").await;
        let fit = s
            .eval(
                "const d = document.documentElement; \
                 const sc = document.querySelector('.scroll'); \
                 return { iw: innerWidth, sw: d.scrollWidth, \
                          inner: sc ? sc.scrollWidth - sc.clientWidth : 0 };",
            )
            .await;
        let iw = fit["iw"].as_u64().unwrap();
        assert!(iw <= 400, "{path}: viewport not at phone width ({iw}px)");
        assert!(
            fit["sw"].as_u64().unwrap() <= iw,
            "{path}: page scrolls horizontally: {fit}"
        );
        assert!(
            fit["inner"].as_i64().unwrap() <= 0,
            "{path}: .scroll overflows horizontally: {fit}"
        );
    }

    s.close().await;
}

/// At phone width the sidebar is a drawer behind ☰: it opens with focus
/// inside, closes on Esc and on a scrim click, and hands focus back to ☰.
#[tokio::test]
async fn phone_width_drawer_opens_and_closes() {
    let s = E2eSession::start().await;
    s.set_window_size(390, 844).await;
    s.goto("/pings").await;

    let toggle = "button[data-drawer-toggle]";
    let drawer_shown =
        "return getComputedStyle(document.getElementById('sidebar')).visibility === 'visible';";
    let drawer_hidden =
        "return getComputedStyle(document.getElementById('sidebar')).visibility === 'hidden';";
    let focus_on_toggle =
        "return document.activeElement === document.querySelector('[data-drawer-toggle]');";

    // Closed at rest: the rail is off-canvas and out of the tab order.
    s.wait_js(drawer_hidden).await;
    assert_eq!(
        s.find(toggle).await.attr("aria-expanded").await.unwrap(),
        Some("false".to_owned())
    );

    // Open: aria-expanded flips and focus lands inside the drawer.
    s.click(toggle).await;
    s.wait_js(drawer_shown).await;
    assert_eq!(
        s.find(toggle).await.attr("aria-expanded").await.unwrap(),
        Some("true".to_owned())
    );
    s.wait_js("return document.getElementById('sidebar').contains(document.activeElement);")
        .await;

    // Esc closes and returns focus to ☰.
    s.find("#sidebar .nav-item")
        .await
        .send_keys(&char::from(fantoccini::key::Key::Escape).to_string())
        .await
        .expect("send Escape");
    s.wait_js(drawer_hidden).await;
    s.wait_js(focus_on_toggle).await;
    assert_eq!(
        s.find(toggle).await.attr("aria-expanded").await.unwrap(),
        Some("false".to_owned())
    );

    // Reopen, then a scrim click closes. The scrim's centre sits under the
    // drawer, so dispatch the click on the scrim itself.
    s.click(toggle).await;
    s.wait_js(drawer_shown).await;
    s.exec("document.querySelector('.drawer-scrim').click();")
        .await;
    s.wait_js(drawer_hidden).await;
    s.wait_js(focus_on_toggle).await;

    s.close().await;
}
