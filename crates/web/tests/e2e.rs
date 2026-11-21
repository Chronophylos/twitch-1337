#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test crate: a panic is a test failure"
)]
//! Browser-level e2e tests for the dashboard. Compiled only under
//! `--features e2e` (see Cargo.toml `[[test]]`). Needs a running
//! chromedriver at `$WEBDRIVER_URL` (default http://localhost:9515).

#[path = "e2e_support/harness.rs"]
mod harness;

use harness::E2eSession;

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
    // The heading count is refreshed out of band, no reload needed.
    s.wait_js("return document.querySelector('#pings-total').textContent === '(0)';")
        .await;

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
    s.fill("input[name=\"interval_hours\"]", "1").await;
    s.fill("input[name=\"interval_minutes\"]", "0").await;
    // `enabled` left untouched => the new-schedule default (checked) applies.
    s.click("button[type=\"submit\"].btn.primary").await;

    // Card appears, active. CSS `text-transform` may uppercase the rendered
    // "off"/"on", so normalise to lowercase before comparing.
    let toggle_btn = "article.schedule-card form.card-toggle button.toggle-switch";
    let before = s.wait_text(toggle_btn).await.to_lowercase();
    assert_eq!(before, "on", "new schedule should start enabled");

    // Toggle off via htmx: only the card is swapped, so a marker set on the
    // window before the click survives (a reload would wipe it). No paused
    // card exists before the swap, so waiting for one confirms it landed.
    s.exec("window.__e2eMarker = true;").await;
    s.click(toggle_btn).await;
    s.wait_for("article.schedule-card.paused").await;
    let after = s.wait_text(toggle_btn).await.to_lowercase();
    assert_eq!(after, "off", "toggle did not disable the schedule");
    assert_eq!(
        s.eval("return window.__e2eMarker === true;").await,
        serde_json::Value::Bool(true),
        "toggling a schedule reloaded the page"
    );
    // The stat strip is refreshed out of band.
    s.wait_js("return document.querySelector('#schedule-stat-active').textContent.trim() === '0';")
        .await;

    // Delete via htmx (confirm auto-accepted): the card goes, the page stays.
    let delete_form = "form[hx-post=\"/schedules/standup/delete\"]";
    s.click(&format!("{delete_form} button.chip.danger")).await;
    s.wait_gone(delete_form).await;
    s.wait_js("return document.querySelector('#schedules-total').textContent === '(0)';")
        .await;
    assert_eq!(
        s.eval("return window.__e2eMarker === true;").await,
        serde_json::Value::Bool(true),
        "deleting a schedule reloaded the page"
    );

    s.close().await;
}

/// Change a numeric setting, save (full form POST), reload, assert persisted.
#[tokio::test]
async fn settings_save_cooldown() {
    let s = E2eSession::start_as_owner().await;
    s.goto("/settings").await;

    // Pick a value inside [1, 3600] unlikely to be the default.
    s.wait_and_fill("input[name=\"cooldowns.ai\"]", "47").await;
    s.click("button[type=\"submit\"].btn.primary[form=\"settings-form\"]")
        .await;

    // Reload settings and confirm the saved value renders back.
    s.goto("/settings").await;
    let el = s.wait_for("input[name=\"cooldowns.ai\"]").await;
    let val = el
        .prop("value")
        .await
        .unwrap_or_default()
        .unwrap_or_default();
    assert_eq!(val, "47", "cooldowns.ai did not persist");

    s.close().await;
}

/// Discard restores the load-time value in place. A reload would let
/// Firefox's form restoration put the edit straight back, so the page must
/// survive the click (the marker set before it is still there).
#[tokio::test]
async fn settings_discard_restores_baseline() {
    let s = E2eSession::start_as_owner().await;
    s.goto("/settings").await;

    let input = "input[name=\"cooldowns.ai\"]";
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
        ("[id=\"cooldowns.ai\"]", "spinbutton", "ai"),
        ("[id=\"alerts.quake.enabled\"]", "switch", "enabled"),
        (
            "[id=\"ai.memory.soul_bytes_display\"]",
            "spinbutton",
            "soul_bytes",
        ),
        (
            "[id=\"ai.aurora.profile\"]",
            "combobox",
            "ai.aurora.profile",
        ),
    ] {
        let got = s.accessibility(&s.find(css).await).await;
        assert_eq!(got, (role.to_owned(), name.to_owned()), "{css}");
    }

    // Resting state: no hover, no focus — the control itself is visible.
    assert_eq!(
        s.eval(
            "document.activeElement.blur(); \
             return getComputedStyle(document.querySelector('[id=\"cooldowns.ai\"]').closest('.num-wrap')).opacity;"
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

/// The editor's byte counter follows typing in UTF-8 bytes and warns once
/// the body passes the cap.
#[tokio::test]
async fn memory_editor_byte_counter_is_live() {
    let s = E2eSession::start().await;
    s.goto("/memory/users/12345").await;

    let body = "textarea[name=\"body\"]";
    // `€` is 3 UTF-8 bytes but 1 UTF-16 unit.
    s.wait_and_fill(body, "a€").await;
    assert_eq!(s.wait_text("[data-bytes-used]").await, "4");
    assert_eq!(
        s.eval("return document.querySelector('[data-bytes-warn]').hidden;")
            .await,
        serde_json::Value::Bool(true)
    );

    s.exec(
        "const ta = document.querySelector('textarea[name=\"body\"]');\
         ta.value = 'x'.repeat(Number(ta.dataset.byteCap) + 1);\
         ta.dispatchEvent(new Event('input'));",
    )
    .await;
    s.wait_js(
        "return !document.querySelector('[data-bytes-warn]').hidden \
         && document.querySelector('.editor .bytes').classList.contains('over');",
    )
    .await;

    s.close().await;
}

/// Forget asks for confirmation naming the user, then deletes the sheet.
#[tokio::test]
async fn memory_forget_character_sheet() {
    let s = E2eSession::start().await;
    s.goto("/memory/users/12345").await;
    s.wait_and_fill("input[name=\"fm_display_name\"]", "Doomed")
        .await;
    s.fill("textarea[name=\"body\"]", "likes trains").await;
    s.exec("window.__e2eMarker = true;").await;
    s.click("form[data-save-shortcut] button[type=\"submit\"]")
        .await;
    s.wait_js("return document.readyState === 'complete' && window.__e2eMarker === undefined;")
        .await;

    s.goto("/memory/users").await;
    let forget = "form[action=\"/memory/users/12345/forget\"] button";
    s.wait_for(forget).await;

    // Declining the confirm keeps the sheet.
    s.exec("window.confirm = () => false;").await;
    s.click(forget).await;
    assert!(
        !s.find_all(forget).await.is_empty(),
        "declined Forget still submitted"
    );

    // The page reloads on submit, so record the prompt in sessionStorage.
    s.exec("window.confirm = (m) => { sessionStorage.setItem('confirm', m); return true; };")
        .await;
    // This list already shows a `.flash` (the save above), so wait for the
    // Forget POST to replace the document before reading the new one.
    s.exec("window.__e2eMarker = true;").await;
    s.click(forget).await;
    s.wait_js("return document.readyState === 'complete' && window.__e2eMarker === undefined;")
        .await;
    let flash = s.wait_text(".flash").await;
    assert!(flash.contains("Doomed"), "flash: {flash}");
    let prompt = s.eval("return sessionStorage.getItem('confirm');").await;
    assert!(
        prompt
            .as_str()
            .is_some_and(|m| m.starts_with("Forget Doomed (12345)?")),
        "confirm prompt: {prompt}"
    );
    assert!(s.find_all(forget).await.is_empty(), "sheet still listed");

    s.close().await;
}

/// `/logs` appends new ring entries via the 2s poll, and skips polling while
/// paused or while the tab is hidden; the cursor catches up afterwards.
#[tokio::test]
async fn logs_tail_polls_and_pauses() {
    let s = E2eSession::start_as_owner().await;
    let push = |msg: &str| {
        s.log_ring.push(
            jiff::Timestamp::now(),
            tracing::Level::INFO,
            "e2e".to_owned(),
            msg.to_owned(),
            Vec::new(),
        );
    };
    let row_present = |msg: &str| {
        format!(
            "return [...document.querySelectorAll('#log-rows .log-msg')]\
             .some((el) => el.textContent.includes('{msg}'));"
        )
    };
    let quiet = std::time::Duration::from_millis(3500);

    push("snapshot-row");
    s.goto("/logs").await;
    s.wait_for("#log-rows .log-row").await;
    assert!(s.source().await.contains("snapshot-row"));

    push("polled-row");
    s.wait_js(&row_present("polled-row")).await;

    s.click("[data-log-pause]").await;
    push("paused-row");
    tokio::time::sleep(quiet).await;
    assert_eq!(
        s.eval(&row_present("paused-row")).await,
        serde_json::Value::Bool(false),
        "no polling while paused"
    );
    s.click("[data-log-pause]").await;
    s.wait_js(&row_present("paused-row")).await;

    s.exec("Object.defineProperty(document, 'hidden', { configurable: true, get: () => true });")
        .await;
    push("hidden-row");
    tokio::time::sleep(quiet).await;
    assert_eq!(
        s.eval(&row_present("hidden-row")).await,
        serde_json::Value::Bool(false),
        "no polling while the tab is hidden"
    );
    s.exec("delete document.hidden;").await;
    s.wait_js(&row_present("hidden-row")).await;

    let rows = s
        .eval("return document.querySelectorAll('#log-rows .log-row').length;")
        .await;
    assert_eq!(rows, serde_json::json!(4), "each row appended exactly once");

    s.close().await;
}

/// The jump palette is open with focus in its combobox.
const PALETTE_OPEN: &str = "return document.getElementById('palette').open \
     && document.activeElement === document.getElementById('palette-input');";
const PALETTE_CLOSED: &str = "return !document.getElementById('palette').open;";

/// Labels of the palette's listed options, in order.
async fn palette_labels(s: &E2eSession) -> Vec<String> {
    let v = s
        .eval(
            "return [...document.querySelectorAll('#palette-list [role=option] .palette-opt-label')] \
             .map((el) => el.textContent);",
        )
        .await;
    serde_json::from_value(v).unwrap()
}

/// Ctrl+K opens the palette; typing "sett" and Enter lands on /settings.
#[tokio::test]
async fn palette_ctrl_k_jumps_to_settings() {
    use fantoccini::key::Key;

    let s = E2eSession::start_as_owner().await;
    s.goto("/pings").await;
    // Non-Mac platform in headless Chrome, so the trigger hint reads Ctrl K.
    assert_eq!(s.wait_text(".cmdk kbd[data-mod-key]").await, "Ctrl");
    s.exec("window.__e2eMarker = true;").await;

    s.find("#sidebar .nav-item")
        .await
        .send_keys(&(Key::Control + "k"))
        .await
        .expect("send ctrl+k");
    s.wait_js(PALETTE_OPEN).await;
    let input = s.find("#palette-input").await;
    assert_eq!(
        s.accessibility(&input).await,
        ("combobox".to_owned(), "Jump to".to_owned())
    );

    input.send_keys("sett").await.expect("type query");
    s.wait_js(
        "const opt = document.getElementById(\
           document.getElementById('palette-input').getAttribute('aria-activedescendant')); \
         return !!opt && opt.getAttribute('aria-selected') === 'true' \
           && opt.querySelector('.palette-opt-label').textContent === 'Settings';",
    )
    .await;
    input
        .send_keys(&char::from(Key::Enter).to_string())
        .await
        .expect("send Enter");

    // The marker disappears once the click navigated to a fresh document.
    s.wait_js(
        "return document.readyState === 'complete' && window.__e2eMarker === undefined \
         && location.pathname === '/settings';",
    )
    .await;
    assert!(s.current_url().await.ends_with("/settings"));

    // On /settings the palette also lists sections; matching covers the
    // group name, and choosing one scrolls to its card.
    s.wait_for("#sidebar .nav-item")
        .await
        .send_keys(&(Key::Control + "k"))
        .await
        .expect("send ctrl+k");
    s.wait_js(PALETTE_OPEN).await;
    let input = s.find("#palette-input").await;
    input.send_keys("agents grok").await.expect("type query");
    s.wait_js(
        "const opt = document.getElementById('palette-opt-0'); \
         return !!opt && opt.getAttribute('aria-selected') === 'true' \
           && opt.textContent === 'GrokSettings · Agents';",
    )
    .await;
    input
        .send_keys(&char::from(Key::Enter).to_string())
        .await
        .expect("send Enter");
    s.wait_js(PALETTE_CLOSED).await;
    s.wait_js(
        "const top = document.getElementById('sec-ai-grok').getBoundingClientRect().top; \
         return top >= 0 && top < 200;",
    )
    .await;

    s.close().await;
}

/// `/` opens the palette with every page and page action listed, arrows move
/// the active option, Esc closes it and focus returns to where it was. At
/// phone width the icon trigger opens it.
#[tokio::test]
async fn palette_slash_opens_and_esc_restores_focus() {
    use fantoccini::key::Key;

    let s = E2eSession::start().await;
    s.goto("/pings").await;
    let link = s.wait_for("#sidebar .nav-item").await;
    link.send_keys("/").await.expect("send /");
    s.wait_js(PALETTE_OPEN).await;

    let input = s.find("#palette-input").await;
    assert_eq!(input.prop("value").await.unwrap(), Some(String::new()));
    let labels = palette_labels(&s).await;
    for want in ["New ping", "Pings", "Schedules", "Emote audit"] {
        assert!(
            labels.iter().any(|l| l == want),
            "{want} missing: {labels:?}"
        );
    }

    input
        .send_keys(&char::from(Key::Down).to_string())
        .await
        .expect("send Down");
    s.wait_js(
        "return document.getElementById('palette-input') \
         .getAttribute('aria-activedescendant') === 'palette-opt-1';",
    )
    .await;

    input
        .send_keys(&char::from(Key::Escape).to_string())
        .await
        .expect("send Escape");
    s.wait_js(PALETTE_CLOSED).await;
    s.wait_js("return document.activeElement === document.querySelector('#sidebar .nav-item');")
        .await;

    // Phone width: the icon-only trigger opens it too.
    s.set_window_size(390, 844).await;
    s.wait_and_click("button.cmdk").await;
    s.wait_js(PALETTE_OPEN).await;
    s.find("#palette-input")
        .await
        .send_keys(&char::from(Key::Escape).to_string())
        .await
        .expect("send Escape");
    s.wait_js(PALETTE_CLOSED).await;
    s.wait_js("return document.activeElement === document.querySelector('button.cmdk');")
        .await;

    s.close().await;
}

/// The inline Filter input above the table live-filters `[data-filter]` rows.
#[tokio::test]
async fn inline_filter_hides_non_matching_rows() {
    let s = E2eSession::start().await;
    for name in ["alpha", "beta"] {
        s.goto("/pings/new").await;
        s.wait_and_fill("#ping-name", name).await;
        s.fill("#ping-template", "yo {mentions}").await;
        s.click("button[type=\"submit\"].btn.primary").await;
        s.wait_for(&format!("a.ping-name[href=\"/pings/{name}\"]"))
            .await;
    }

    let visible = "return [...document.querySelectorAll('.tr[data-filter]')] \
         .filter((r) => r.style.display !== 'none') \
         .map((r) => r.querySelector('.ping-name').textContent.trim()).join(',')";
    s.wait_and_fill("input[data-row-filter]", "alp").await;
    s.wait_js(&format!("{visible} === 'alpha';")).await;

    // Element Clear fires no `input` event, so erase with keystrokes.
    let backspace = char::from(fantoccini::key::Key::Backspace).to_string();
    s.find("input[data-row-filter]")
        .await
        .send_keys(&backspace.repeat(3))
        .await
        .expect("send Backspace");
    s.wait_js(&format!("{visible} === 'alpha,beta';")).await;

    s.close().await;
}

/// Add an AI profile through the dashboard form.
async fn add_profile(s: &E2eSession, name: &str) {
    s.goto("/ai-profiles?new=true").await;
    s.wait_and_fill("form.profile-form input[name=\"name\"]", name)
        .await;
    s.fill("form.profile-form input[name=\"key\"]", "k").await;
    s.fill("form.profile-form input[name=\"model\"]", "some-model")
        .await;
    s.click("form.profile-form button[type=\"submit\"]").await;
    s.wait_for(&format!("form[action=\"/ai-profiles/{name}/delete\"]"))
        .await;
}

/// Every route renders under the strict CSP: no `securitypolicyviolation`
/// events, JS errors or console errors. A deliberate inline `<style>` at the
/// end proves the probe does see violations.
#[tokio::test]
async fn every_route_runs_clean_under_the_csp() {
    let s = E2eSession::start_as_owner().await;

    // One of everything that has its own page.
    s.goto("/pings/new").await;
    s.wait_and_fill("#ping-name", "squad").await;
    s.fill("#ping-template", "yo {mentions}").await;
    s.click("button[type=\"submit\"].btn.primary").await;
    s.wait_for("a.ping-name").await;

    s.goto("/schedules?new=true").await;
    s.wait_and_fill("input[name=\"name\"]", "standup").await;
    s.fill("textarea[name=\"message\"]", "daily standup time")
        .await;
    s.fill("input[name=\"interval_hours\"]", "1").await;
    s.click("button[type=\"submit\"].btn.primary").await;
    s.wait_for("article.schedule-card").await;

    add_profile(&s, "alpha").await;

    s.goto("/memory/scratch/new").await;
    s.wait_and_fill("input[name=\"slug\"]", "notes").await;
    s.fill("textarea[name=\"body\"]", "remember the raid").await;
    s.click("form[data-save-shortcut] button[type=\"submit\"]")
        .await;
    s.wait_js("return location.pathname !== '/memory/scratch/new';")
        .await;

    let seeding = s.problems().await;
    assert!(seeding.is_empty(), "while seeding: {seeding:#?}");

    let paths = [
        "/pings",
        "/pings/new",
        "/pings/squad",
        "/leaderboard",
        "/flights",
        "/schedules",
        "/schedules?new=true",
        "/schedules?edit=standup",
        "/memory",
        "/memory/soul",
        "/memory/lore",
        "/memory/users",
        "/memory/users/new",
        "/memory/users/12345",
        "/memory/scratch",
        "/memory/scratch/new",
        "/memory/scratch/notes",
        "/emotes",
        "/emotes/audit",
        "/emotes/usage",
        "/logs",
        "/ai-profiles",
        "/ai-profiles?new=true",
        "/ai-profiles?edit=alpha",
        "/settings",
    ];
    for path in paths {
        s.goto(path).await;
        s.wait_for(".topbar").await;
        let problems = s.problems().await;
        assert!(problems.is_empty(), "{path}: {problems:#?}");
    }

    s.goto("/login").await;
    s.wait_for(".auth-card").await;
    let problems = s.problems().await;
    assert!(problems.is_empty(), "/login: {problems:#?}");

    // The 404 page itself logs the failed document load, nothing else.
    s.goto("/no-such-page").await;
    s.wait_for("body").await;
    let problems = s.problems().await;
    assert!(
        problems
            .iter()
            .all(|p| p.starts_with("console:") && p.contains("404")),
        "/no-such-page: {problems:#?}"
    );

    // Positive control: an injected `<style>` element is refused and seen.
    s.goto("/pings").await;
    s.wait_for(".topbar").await;
    s.exec(
        "const st = document.createElement('style'); \
         st.textContent = 'body { color: red; }'; \
         document.head.append(st);",
    )
    .await;
    s.wait_js("return window.__e2eProblems.some((p) => p.startsWith('csp style-src'));")
        .await;

    s.close().await;
}

/// The schedule form shows only the checked trigger's fields (app.js, not an
/// inline script, so it runs under the CSP).
#[tokio::test]
async fn schedule_form_shows_only_the_checked_trigger() {
    let s = E2eSession::start().await;
    s.goto("/schedules?new=true").await;
    s.wait_for("form.schedule-form").await;
    let shown = "return [...document.querySelectorAll('form.schedule-form .trigger-fields')] \
                 .filter((el) => getComputedStyle(el).display !== 'none') \
                 .map((el) => el.dataset.for).join();";
    assert_eq!(s.eval(shown).await, "interval");

    s.click("input[name=\"kind\"][value=\"calendar\"]").await;
    assert_eq!(s.eval(shown).await, "calendar");
    s.click("input[name=\"kind\"][value=\"interval\"]").await;
    assert_eq!(s.eval(shown).await, "interval");

    let problems = s.problems().await;
    assert!(problems.is_empty(), "{problems:#?}");
    s.close().await;
}

/// AI profile delete asks first (`data-confirm`, handled in app.js), and the
/// form's provider picker enables only the chosen provider's controls.
#[tokio::test]
async fn ai_profile_delete_confirms_and_provider_picker_toggles() {
    let s = E2eSession::start_as_owner().await;
    add_profile(&s, "alpha").await;
    add_profile(&s, "doomed").await;

    s.goto("/ai-profiles").await;
    let delete = "form[action=\"/ai-profiles/doomed/delete\"] button";
    s.wait_for(delete).await;
    // Declining the confirm keeps the profile.
    s.exec("window.confirm = () => false;").await;
    s.click(delete).await;
    assert!(
        !s.find_all(delete).await.is_empty(),
        "declined delete still submitted"
    );
    // Accepting deletes it; the prompt survives the reload in sessionStorage.
    s.exec("window.confirm = (m) => { sessionStorage.setItem('confirm', m); return true; };")
        .await;
    s.exec("window.__e2eMarker = true;").await;
    s.click(delete).await;
    s.wait_js("return document.readyState === 'complete' && window.__e2eMarker === undefined;")
        .await;
    assert_eq!(
        s.eval("return sessionStorage.getItem('confirm');").await,
        "Delete profile “doomed”?"
    );
    assert!(s.find_all(delete).await.is_empty(), "profile still listed");

    s.goto("/ai-profiles?new=true").await;
    s.wait_for("form.profile-form").await;
    let enabled = "return [...document.querySelectorAll('form.profile-form fieldset[data-provider]')] \
                   .filter((el) => !el.hidden && !el.disabled) \
                   .map((el) => el.dataset.provider).join();";
    let provider = "form.profile-form select[name=\"provider\"]";
    let kind = "form.profile-form select[name=\"kind\"]";
    assert_eq!(s.eval(enabled).await, "open_router");
    let key_state = "const el = document.querySelector('form.profile-form input[name=\"key\"]'); \
                     return [el.closest('.row').hidden, el.disabled, el.required].join(',');";
    assert_eq!(s.eval(key_state).await, "false,false,true");
    let base_state = "const el = document.querySelector('form.profile-form input[name=\"base_url\"]'); \
                      return [el.required, el.placeholder, el.closest('.row').querySelector('small').textContent].join('|');";
    assert_eq!(
        s.eval(base_state).await,
        "false|https://openrouter.ai/api/v1|Optional. Leave blank to use https://openrouter.ai/api/v1."
    );
    assert_eq!(
        s.eval(
            "return document.querySelector('form.profile-form input[name=\"model\"]').placeholder;"
        )
        .await,
        "provider/model-id"
    );
    assert_eq!(
        s.eval("return document.querySelector('form.profile-form select[name=\"kind\"]').selectedOptions[0].textContent;")
            .await,
        "Chat agent"
    );
    s.find(provider)
        .await
        .select_by_value("open_ai")
        .await
        .expect("pick open_ai");
    assert_eq!(s.eval(enabled).await, "open_ai");
    assert_eq!(
        s.eval(base_state).await,
        "false|https://api.openai.com/v1|Optional. Leave blank to use https://api.openai.com/v1."
    );
    s.find(provider)
        .await
        .select_by_value("open_ai_compatible")
        .await
        .expect("pick open_ai_compatible");
    assert_eq!(s.eval(enabled).await, "open_ai_compatible");
    assert_eq!(
        s.eval(base_state).await,
        "true|https://llm.example.com/v1|Required for an OpenAI-compatible endpoint."
    );
    assert_eq!(
        s.eval("const el = document.querySelector('fieldset[data-provider=\"open_ai_compatible\"] select[name=\"service_tier\"]'); return el ? [...el.options].map(o => o.value).join(',') : 'missing';")
            .await,
        ",flex,priority"
    );
    s.find(provider)
        .await
        .select_by_value("ollama")
        .await
        .expect("pick ollama");
    assert_eq!(s.eval(enabled).await, "ollama");
    assert_eq!(s.eval(key_state).await, "true,true,false");
    assert_eq!(
        s.eval(base_state).await,
        "false|http://localhost:11434|Optional. Leave blank to use http://localhost:11434."
    );
    // Decision profiles are OpenRouter-only and carry no effort/tier.
    s.find(kind)
        .await
        .select_by_value("decision")
        .await
        .expect("pick decision");
    assert_eq!(
        s.eval(&format!(
            "return document.querySelector('{provider}').value;"
        ))
        .await,
        "open_router"
    );
    assert_eq!(s.eval(enabled).await, "");
    // Back to chat: the form is pristine again, so leaving it won't prompt.
    s.find(kind)
        .await
        .select_by_value("chat")
        .await
        .expect("pick chat");
    assert_eq!(s.eval(enabled).await, "open_router");

    let problems = s.problems().await;
    assert!(problems.is_empty(), "{problems:#?}");
    s.close().await;
}
