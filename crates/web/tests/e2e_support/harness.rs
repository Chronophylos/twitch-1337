//! E2E harness: in-process dashboard server + headless Chrome client with
//! an authenticated mod session. Reuses `tests/helpers` for state/auth.

#![allow(dead_code, reason = "not every e2e test uses every harness helper")]

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use fantoccini::{Client, ClientBuilder, Locator};
use hyper_util::client::legacy::connect::HttpConnector;
use serde_json::json;
use tokio::task::JoinHandle;
use twitch_1337_core::lifecycle::Lifecycle;
use twitch_1337_web::{bind, build_router, serve_app};

#[path = "../helpers/mod.rs"]
mod helpers;

/// Default WebDriver endpoint; override with `WEBDRIVER_URL`.
fn webdriver_url() -> String {
    std::env::var("WEBDRIVER_URL").unwrap_or_else(|_| "http://localhost:9515".to_owned())
}

/// Bounded wait defaults for element queries.
const WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_POLL: Duration = Duration::from_millis(100);

/// WebDriver "Get Computed Role" / "Get Computed Label" for one element.
/// fantoccini has no wrapper for these endpoints.
#[derive(Debug)]
struct ComputedA11y {
    element: String,
    endpoint: &'static str,
}

impl fantoccini::wd::WebDriverCompatibleCommand for ComputedA11y {
    fn endpoint(
        &self,
        base_url: &url::Url,
        session_id: Option<&str>,
    ) -> Result<url::Url, url::ParseError> {
        let session = session_id.expect("computed a11y needs a live session");
        base_url.join(&format!(
            "session/{session}/element/{}/{}",
            self.element, self.endpoint
        ))
    }

    fn method_and_body(&self, _: &url::Url) -> (axum::http::Method, Option<String>) {
        (axum::http::Method::GET, None)
    }
}

/// chromedriver's "Execute CDP command" (`goog/cdp/execute`).
#[derive(Debug)]
struct CdpExecute {
    cmd: &'static str,
    params: serde_json::Value,
}

impl fantoccini::wd::WebDriverCompatibleCommand for CdpExecute {
    fn endpoint(
        &self,
        base_url: &url::Url,
        session_id: Option<&str>,
    ) -> Result<url::Url, url::ParseError> {
        let session = session_id.expect("cdp execute needs a live session");
        base_url.join(&format!("session/{session}/goog/cdp/execute"))
    }

    fn method_and_body(&self, _: &url::Url) -> (axum::http::Method, Option<String>) {
        let body = json!({ "cmd": self.cmd, "params": self.params });
        (axum::http::Method::POST, Some(body.to_string()))
    }
}

/// chromedriver's legacy "Get Log" (`se/log`): drains the browser console
/// entries collected since the previous call. Needs `goog:loggingPrefs`.
#[derive(Debug)]
struct BrowserLog;

impl fantoccini::wd::WebDriverCompatibleCommand for BrowserLog {
    fn endpoint(
        &self,
        base_url: &url::Url,
        session_id: Option<&str>,
    ) -> Result<url::Url, url::ParseError> {
        let session = session_id.expect("browser log needs a live session");
        base_url.join(&format!("session/{session}/se/log"))
    }

    fn method_and_body(&self, _: &url::Url) -> (axum::http::Method, Option<String>) {
        let body = json!({ "type": "browser" });
        (axum::http::Method::POST, Some(body.to_string()))
    }
}

/// Installed into every new document before any page script runs, so it
/// sees CSP violations and errors raised while the page loads.
const PROBLEM_PROBE: &str = r"(() => {
  const problems = [];
  Object.defineProperty(window, '__e2eProblems', { value: problems });
  document.addEventListener('securitypolicyviolation', (e) => {
    problems.push(`csp ${e.effectiveDirective}: ${e.blockedURI || 'inline'} at ${e.sourceFile}:${e.lineNumber}`);
  }, true);
  window.addEventListener('error', (e) => {
    const what = e.message || e.target?.src || e.target?.href || e.type;
    problems.push(`error: ${what}`);
  }, true);
  window.addEventListener('unhandledrejection', (e) => problems.push(`unhandledrejection: ${e.reason}`));
  const consoleError = console.error.bind(console);
  console.error = (...args) => {
    problems.push(`console.error: ${args.join(' ')}`);
    consoleError(...args);
  };
})();";

/// Selects which session role the harness injects.
enum SessionKind {
    /// Authenticated mod (no owner configured). Default for `start()`.
    Mod,
    /// Authenticated owner (user marked as the configured owner).
    /// Required for owner-gated pages like /settings.
    Owner,
}

/// A running dashboard + browser session, authenticated as a mod.
/// Holds tempdirs alive for the server's lifetime and tears everything
/// down on `close()`. On a panic before `close()`, the browser session
/// leaks until chromedriver exits — acceptable here since the CI/local
/// driver is ephemeral (fantoccini's async `close` consumes `self`, so it
/// can't run from `Drop`).
pub struct E2eSession {
    client: Client,
    base_url: String,
    lifecycle: Lifecycle,
    server: JoinHandle<()>,
    // Kept alive so the server's data dirs are not removed mid-test.
    _dirs: (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir),
    /// The server's tracing ring, so tests can feed the `/logs` page.
    pub log_ring: std::sync::Arc<twitch_1337_core::util::log_ring::LogRing>,
}

impl E2eSession {
    /// Build state + router, serve on an ephemeral port, connect a headless
    /// Chrome, and inject signed session cookies for a mod user.
    pub async fn start() -> Self {
        Self::start_inner(SessionKind::Mod).await
    }

    /// Like [`start`] but configures an owner session.
    /// Required for owner-gated pages like `/settings`.
    pub async fn start_as_owner() -> Self {
        Self::start_inner(SessionKind::Owner).await
    }

    async fn start_inner(kind: SessionKind) -> Self {
        helpers::install_crypto();
        let user_id = "9001";
        let helix = helpers::admin_helix(user_id);
        let (state, d1, d2, d3) = helpers::build_state_with_all_dirs(helix).await;
        let (signed_sid, signed_csrf, _bare) = match kind {
            SessionKind::Mod => helpers::insert_session(&state, user_id, "admin"),
            SessionKind::Owner => {
                helpers::set_owner(&state, Some(user_id));
                helpers::insert_session_as(
                    &state,
                    user_id,
                    "owner",
                    twitch_1337_web::auth::Role::Owner,
                )
            }
        };

        // Serve on an ephemeral loopback port.
        let listener = bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("bind ephemeral");
        let port = listener.local_addr().expect("local_addr").port();
        // Use "localhost" (not "127.0.0.1") so WebDriver's AddCookie command
        // accepts the domain: Chrome rejects IP-address domains with
        // "invalid argument: invalid 'domain'".
        let base_url = format!("http://localhost:{port}");
        let log_ring = state.log_ring.clone();
        let app = build_router(state);
        let lifecycle = Lifecycle::new();
        let server = tokio::spawn({
            let lifecycle = lifecycle.clone();
            async move {
                serve_app(listener, app, lifecycle)
                    .await
                    .expect("E2E web server failed");
            }
        });

        // Headless Chrome capabilities; auto-accept confirm() dialogs so
        // htmx hx-confirm and the JS delete-confirm proceed. The browser log
        // feeds `problems()`.
        let mut caps = json!({
            "goog:chromeOptions": {
                "args": ["--headless=new", "--no-sandbox", "--disable-gpu", "--window-size=1400,900"],
            },
            "goog:loggingPrefs": { "browser": "ALL" },
            "unhandledPromptBehavior": "accept",
        });
        // Pin the Chrome binary when given (CI sets this to the
        // setup-chrome-installed browser). Without it, chromedriver launches
        // whatever chrome it finds by default — on GitHub runners that's a
        // preinstalled Chrome whose version may not match the installed
        // chromedriver, yielding "session not created: only supports Chrome N".
        if let Ok(bin) = std::env::var("E2E_CHROME_BINARY")
            && !bin.is_empty()
        {
            caps["goog:chromeOptions"]["binary"] = json!(bin);
        }
        let caps = caps.as_object().expect("caps is a json object").clone();

        // Plain HTTP connector: chromedriver and the in-process app are both
        // http:// loopback. Skipping fantoccini's rustls-tls feature keeps
        // aws-lc-rs out of the workspace graph.
        let mut builder = ClientBuilder::new(HttpConnector::new());
        builder.capabilities(caps);
        let client = builder
            .connect(&webdriver_url())
            .await
            .unwrap_or_else(|e| panic!("connect chromedriver at {}: {e}", webdriver_url()));
        client
            .issue_cmd(CdpExecute {
                cmd: "Page.addScriptToEvaluateOnNewDocument",
                params: json!({ "source": PROBLEM_PROBE }),
            })
            .await
            .expect("install problem probe");

        // Cookies require being on the origin first. Navigate to the public
        // login page so the browser is on the right origin before we inject
        // cookies. It renders without a session and links its favicon, so
        // Chrome doesn't log a 404 for `/favicon.ico` into `problems()`.
        client
            .goto(&format!("{base_url}/login"))
            .await
            .expect("goto origin");
        // Use JavaScript to set the cookies — WebDriver's AddCookie command
        // rejects loopback origins with "invalid 'domain'" on Chrome 148.
        set_cookie_js(&client, "tw1337_sid", &signed_sid).await;
        set_cookie_js(&client, "tw1337_csrf", &signed_csrf).await;

        Self {
            client,
            base_url,
            lifecycle,
            server,
            _dirs: (d1, d2, d3),
            log_ring,
        }
    }

    /// Navigate to a same-origin path (leading slash).
    pub async fn goto(&self, path: &str) {
        let url = format!("{}{}", self.base_url, path);
        self.client.goto(&url).await.expect("goto");
    }

    /// Resize the browser window (headless Chrome: the viewport).
    pub async fn set_window_size(&self, width: u32, height: u32) {
        self.client
            .set_window_size(width, height)
            .await
            .expect("set_window_size");
    }

    /// Current browser URL as a string.
    pub async fn current_url(&self) -> String {
        self.client
            .current_url()
            .await
            .expect("current_url")
            .to_string()
    }

    /// Full page HTML source.
    pub async fn source(&self) -> String {
        self.client.source().await.expect("source")
    }

    /// Wait until a CSS selector matches, returning the element.
    pub async fn wait_for(&self, css: &str) -> fantoccini::elements::Element {
        self.client
            .wait()
            .at_most(WAIT_TIMEOUT)
            .every(WAIT_POLL)
            .for_element(Locator::Css(css))
            .await
            .unwrap_or_else(|e| panic!("wait_for {css}: {e}"))
    }

    /// Wait until a CSS selector no longer matches any element.
    pub async fn wait_gone(&self, css: &str) {
        let deadline = tokio::time::Instant::now() + WAIT_TIMEOUT;
        loop {
            match self.client.find(Locator::Css(css)).await {
                Err(e) if e.is_no_such_element() => return,
                Ok(_) => {}
                Err(e) => panic!("wait_gone {css}: {e}"),
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("wait_gone {css}: still present after {WAIT_TIMEOUT:?}");
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }

    /// Find one element now (no wait).
    pub async fn find(&self, css: &str) -> fantoccini::elements::Element {
        self.client
            .find(Locator::Css(css))
            .await
            .unwrap_or_else(|e| panic!("find {css}: {e}"))
    }

    /// Find all elements matching a selector now (no wait).
    pub async fn find_all(&self, css: &str) -> Vec<fantoccini::elements::Element> {
        self.client
            .find_all(Locator::Css(css))
            .await
            .unwrap_or_default()
    }

    /// Type text into an input, clearing it first.
    pub async fn fill(&self, css: &str, text: &str) {
        let el = self.find(css).await;
        el.clear().await.expect("clear");
        el.send_keys(text).await.expect("send_keys");
    }

    /// Click the element matching a selector.
    pub async fn click(&self, css: &str) {
        self.find(css).await.click().await.expect("click");
    }

    /// Wait for an input to appear, then fill it (clearing first). Reuses the
    /// waited element — no second `find` round-trip.
    pub async fn wait_and_fill(&self, css: &str, text: &str) {
        let el = self.wait_for(css).await;
        el.clear().await.expect("clear");
        el.send_keys(text).await.expect("send_keys");
    }

    /// Wait for an element to appear, then click it (reusing the waited element).
    pub async fn wait_and_click(&self, css: &str) {
        self.wait_for(css).await.click().await.expect("click");
    }

    /// Wait for an element, then return its trimmed visible text.
    pub async fn wait_text(&self, css: &str) -> String {
        self.wait_for(css)
            .await
            .text()
            .await
            .unwrap_or_default()
            .trim()
            .to_owned()
    }

    /// Run a JavaScript snippet in the page.
    pub async fn exec(&self, script: &str) {
        self.client
            .execute(script, vec![])
            .await
            .unwrap_or_else(|e| panic!("exec {script}: {e}"));
    }

    /// Run a synchronous script in the page and return its result.
    pub async fn eval(&self, js: &str) -> serde_json::Value {
        self.client
            .execute(js, vec![])
            .await
            .unwrap_or_else(|e| panic!("eval {js}: {e}"))
    }

    /// The browser's computed ARIA role and accessible name for `el`.
    pub async fn accessibility(&self, el: &fantoccini::elements::Element) -> (String, String) {
        let mut out = [String::new(), String::new()];
        for (slot, endpoint) in out.iter_mut().zip(["computedrole", "computedlabel"]) {
            let cmd = ComputedA11y {
                element: el.element_id().to_string(),
                endpoint,
            };
            let value = self
                .client
                .issue_cmd(cmd)
                .await
                .unwrap_or_else(|e| panic!("{endpoint}: {e}"));
            *slot = value.as_str().unwrap_or_default().to_owned();
        }
        let [role, name] = out;
        (role, name)
    }

    /// Wait until `js` (a script that `return`s a value) yields `true`.
    /// Script errors count as "not yet": polling across a navigation can hit
    /// a destroyed execution context.
    pub async fn wait_js(&self, js: &str) {
        let deadline = tokio::time::Instant::now() + WAIT_TIMEOUT;
        loop {
            if let Ok(serde_json::Value::Bool(true)) = self.client.execute(js, vec![]).await {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("wait_js {js}: not true after {WAIT_TIMEOUT:?}");
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }

    /// CSP violations and errors on the current page, plus every `SEVERE`
    /// browser console entry logged since the previous call (any page).
    pub async fn problems(&self) -> Vec<String> {
        let page = self.eval("return window.__e2eProblems ?? ['problem probe missing'];");
        let mut out: Vec<String> = serde_json::from_value(page.await).expect("probe list");
        let log = self
            .client
            .issue_cmd(BrowserLog)
            .await
            .expect("browser log");
        for entry in log.as_array().expect("log entries") {
            if entry["level"] == "SEVERE" {
                out.push(format!("console: {}", entry["message"]));
            }
        }
        out
    }

    /// Close the browser and stop the server.
    pub async fn close(self) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "teardown: a browser session that already ended is fine"
        )]
        let _ = self.client.close().await;
        self.lifecycle.request_graceful_stop();
        self.server
            .await
            .expect("E2E web server task panicked during shutdown");
    }
}

/// Set a session cookie via `document.cookie`. Bypasses WebDriver's
/// `AddCookie` command which Chrome rejects for loopback origins.
async fn set_cookie_js(client: &Client, name: &str, value: &str) {
    // Percent-encode so `=`/`;`/space in the signed cookie string don't corrupt
    // the cookie; the server's `parse_encoded` percent-decodes it back.
    let encoded_value = urlencoding::encode(value);
    let script = format!("document.cookie = \"{name}={encoded_value}; path=/\";");
    client
        .execute(&script, vec![])
        .await
        .unwrap_or_else(|e| panic!("set_cookie_js {name}: {e}"));
}
