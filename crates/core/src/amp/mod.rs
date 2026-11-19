//! CubeCoders AMP client backing `!servers`. AMP is JSON-RPC over POST with
//! the session id in the body. Game instances sit behind the controller's
//! proxy (`ADSModule/Servers/{id}/API/...`) and need their own login there;
//! the controller session is rejected. Probed against AMP 2.8.0.10; see the
//! `[amp]` section in AGENTS.md.

use std::{collections::HashMap, time::Duration};

use eyre::{Result, WrapErr};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::APP_USER_AGENT;

const TIMEOUT: Duration = Duration::from_secs(10);
/// Session-map key for the controller itself; instances use their id.
const CONTROLLER: &str = "";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmpError {
    /// Login refused, or a fresh session was rejected again.
    Auth,
    /// Valid session, but the AMP role lacks the method's permission.
    Permission,
    Unavailable,
}

/// AMP nulls optional fields freely; treat null like a missing key.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// AMP `AppState` for a game that is up and accepting players.
const APP_STATE_READY: i32 = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct AmpServer {
    pub id: String,
    pub name: String,
    pub instance_name: String,
    pub module: String,
    pub running: bool,
    /// Active users raw/max. `None` when stopped (AMP keeps stale metrics).
    pub players: Option<(u32, u32)>,
    pub memory_mb: Option<(u32, u32)>,
    pub address: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AmpDetails {
    pub uptime: Option<Duration>,
    pub players: Vec<String>,
}

// --- AMP wire types: only the fields the projection uses. ---

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WireTarget {
    #[serde(default, deserialize_with = "null_default")]
    available_instances: Vec<WireInstance>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WireInstance {
    #[serde(rename = "InstanceID", default, deserialize_with = "null_default")]
    instance_id: String,
    #[serde(default)]
    app_state: Option<i32>,
    #[serde(default, deserialize_with = "null_default")]
    instance_name: String,
    #[serde(default, deserialize_with = "null_default")]
    friendly_name: String,
    #[serde(default, deserialize_with = "null_default")]
    module: String,
    #[serde(default, deserialize_with = "null_default")]
    running: bool,
    #[serde(default, deserialize_with = "null_default")]
    metrics: HashMap<String, WireMetric>,
    #[serde(default, deserialize_with = "null_default")]
    application_endpoints: Vec<WireEndpoint>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WireMetric {
    #[serde(default, deserialize_with = "null_default")]
    raw_value: f64,
    #[serde(default, deserialize_with = "null_default")]
    max_value: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WireEndpoint {
    #[serde(default, deserialize_with = "null_default")]
    display_name: String,
    #[serde(default, deserialize_with = "null_default")]
    endpoint: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WireStatus {
    #[serde(default)]
    uptime: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireLogin {
    #[serde(default, deserialize_with = "null_default")]
    success: bool,
    #[serde(default, rename = "sessionID")]
    session_id: Option<String>,
}

pub struct AmpClient {
    http: reqwest::Client,
    base_url: String,
    username: String,
    password: SecretString,
    // ponytail: one lock over all sessions, concurrent first calls may log in twice; harmless
    sessions: Mutex<HashMap<String, String>>,
}

impl AmpClient {
    pub fn new(
        base_url: impl Into<String>,
        username: String,
        password: SecretString,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .timeout(TIMEOUT)
            .build()
            .wrap_err("build amp HTTP client")?;
        Ok(Self::with_http(http, base_url, username, password))
    }

    /// Test hook: caller supplies the HTTP client and base URL (e.g. wiremock).
    pub fn with_http(
        http: reqwest::Client,
        base_url: impl Into<String>,
        username: String,
        password: SecretString,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            username,
            password,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Every game instance (controller filtered out), running ones first.
    pub async fn servers(&self) -> Result<Vec<AmpServer>, AmpError> {
        let targets: Vec<WireTarget> = self.call(CONTROLLER, "ADSModule/GetInstances").await?;
        Ok(project(targets))
    }

    /// Uptime and player names of one running instance, via the proxy.
    pub async fn details(&self, instance_id: &str) -> Result<AmpDetails, AmpError> {
        // Each piece stands alone; only fail when both calls failed.
        let status = self.call::<WireStatus>(instance_id, "Core/GetStatus").await;
        let users = self
            .call::<HashMap<String, String>>(instance_id, "Core/GetUserList")
            .await;
        let (status, users) = match (status, users) {
            (Err(e), Err(_)) => return Err(e),
            (s, u) => (s.ok(), u.ok()),
        };
        let mut players: Vec<String> = users.map(|u| u.into_values().collect()).unwrap_or_default();
        players.sort();
        Ok(AmpDetails {
            uptime: status
                .and_then(|s| s.uptime)
                .as_deref()
                .and_then(parse_uptime),
            players,
        })
    }

    fn url(&self, target: &str, method: &str) -> String {
        if target == CONTROLLER {
            format!("{}/API/{method}", self.base_url)
        } else {
            format!(
                "{}/API/ADSModule/Servers/{target}/API/{method}",
                self.base_url
            )
        }
    }

    async fn post(&self, target: &str, method: &str, body: &Value) -> Result<Value, AmpError> {
        let resp = self
            .http
            .post(self.url(target, method))
            .header("Accept", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|error| {
                let error = error.without_url();
                tracing::warn!(?error, method, "amp request failed");
                AmpError::Unavailable
            })?;
        let status = resp.status();
        if !status.is_success() {
            tracing::warn!(%status, method, "amp returned an unexpected status");
            return Err(AmpError::Unavailable);
        }
        resp.json().await.map_err(|error| {
            let error = error.without_url();
            tracing::warn!(?error, method, "amp response was not JSON");
            AmpError::Unavailable
        })
    }

    async fn session(&self, target: &str) -> Result<String, AmpError> {
        if let Some(sid) = self.sessions.lock().await.get(target) {
            return Ok(sid.clone());
        }
        let body = json!({
            "username": self.username,
            "password": self.password.expose_secret(),
            "token": "",
            "rememberMe": false,
        });
        let login: WireLogin = serde_json::from_value(
            self.post(target, "Core/Login", &body).await?,
        )
        .map_err(|error| {
            tracing::warn!(?error, "amp login response did not parse");
            AmpError::Unavailable
        })?;
        let sid = match login.session_id {
            Some(sid) if login.success => sid,
            _ => {
                tracing::warn!(target, "amp login refused");
                return Err(AmpError::Auth);
            }
        };
        self.sessions
            .lock()
            .await
            .insert(target.to_owned(), sid.clone());
        Ok(sid)
    }

    /// Authenticated call. A rejected session is dropped and re-logged once.
    async fn call<R: DeserializeOwned>(&self, target: &str, method: &str) -> Result<R, AmpError> {
        let mut retried = false;
        loop {
            let sid = self.session(target).await?;
            let value = self
                .post(target, method, &json!({ "SESSIONID": sid }))
                .await?;
            let title = value.get("Title").and_then(Value::as_str);
            let message = value.get("Message").and_then(Value::as_str);
            match (title, message) {
                // A missing/expired session names Session.Exists; any other
                // permission is the role, and a re-login would not help.
                (Some("Unauthorized Access"), Some(msg)) if msg.contains("Session.Exists") => {
                    self.sessions.lock().await.remove(target);
                    if retried {
                        tracing::warn!(method, "amp rejected a fresh session");
                        return Err(AmpError::Auth);
                    }
                    retried = true;
                }
                (Some("Unauthorized Access"), _) => {
                    tracing::warn!(method, ?message, "amp role lacks permission");
                    return Err(AmpError::Permission);
                }
                (Some(title), Some(_)) => {
                    tracing::warn!(method, title, ?message, "amp returned an error");
                    return Err(AmpError::Unavailable);
                }
                _ => {
                    return serde_json::from_value(value).map_err(|error| {
                        tracing::warn!(?error, method, "amp response did not parse");
                        AmpError::Unavailable
                    });
                }
            }
        }
    }
}

fn project(targets: Vec<WireTarget>) -> Vec<AmpServer> {
    let mut servers: Vec<AmpServer> = targets
        .into_iter()
        .flat_map(|t| t.available_instances)
        .filter(|i| i.module != "ADS")
        .map(|i| {
            // `Running` is AMP's instance process; the game is only up at Ready.
            let online = i.running && i.app_state == Some(APP_STATE_READY);
            let metric = |key: &str| {
                i.metrics
                    .get(key)
                    .filter(|_| online)
                    .map(|m| (m.raw_value.round() as u32, m.max_value.round() as u32))
            };
            AmpServer {
                players: metric("Active Users"),
                memory_mb: metric("Memory Usage"),
                address: pick_address(&i.application_endpoints),
                id: i.instance_id,
                name: i.friendly_name,
                instance_name: i.instance_name,
                module: i.module,
                running: online,
            }
        })
        .collect();
    servers.sort_by(|a, b| b.running.cmp(&a.running).then_with(|| a.name.cmp(&b.name)));
    servers
}

/// First non-SFTP endpoint with a concrete host; else the first non-SFTP one.
/// Generic modules report `0.0.0.0:port` as the application address and the
/// real bind IP under "Unknown Endpoint".
fn pick_address(endpoints: &[WireEndpoint]) -> Option<String> {
    let candidates = || {
        endpoints
            .iter()
            .filter(|e| !e.display_name.contains("SFTP") && !e.endpoint.is_empty())
    };
    let concrete = |e: &&WireEndpoint| {
        let host = e
            .endpoint
            .rsplit_once(':')
            .map_or(e.endpoint.as_str(), |(h, _)| h);
        !matches!(host, "" | "0.0.0.0" | "::" | "[::]")
    };
    candidates()
        .find(concrete)
        .or_else(|| candidates().next())
        .map(|e| e.endpoint.clone())
}

/// AMP uptime: `d:hh:mm:ss`, day part omitted below one day.
fn parse_uptime(s: &str) -> Option<Duration> {
    let parts: Vec<u64> = s
        .split(':')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let (d, h, m, sec) = match parts[..] {
        [d, h, m, s] => (d, h, m, s),
        [h, m, s] => (0, h, m, s),
        _ => return None,
    };
    Some(Duration::from_secs(((d * 24 + h) * 60 + m) * 60 + sec))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use secrecy::SecretString;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;

    const INSTANCES: &str = include_str!("fixtures/get_instances.json");
    const STATUS: &str = include_str!("fixtures/status.json");
    const UNAUTHORIZED: &str = r#"{"Title":"Unauthorized Access","Message":"You do not have permission to use this method (ADSModule.GetInstances) at this time. This method requires the Session.Exists permission.","StackTrace":""}"#;
    const FORBIDDEN: &str = r#"{"Title":"Unauthorized Access","Message":"You do not have permission to use this method (Core.GetUserList) at this time. This method requires the Core.UserManagement.ViewUsers permission.","StackTrace":""}"#;

    fn one_instance(fields: &str) -> String {
        format!(
            r#"[{{"AvailableInstances":[{{"InstanceID":"x","InstanceName":"X01","FriendlyName":"X","Module":"Minecraft",{fields}}}]}}]"#
        )
    }

    fn fixture_servers() -> Vec<AmpServer> {
        project(serde_json::from_str(INSTANCES).unwrap())
    }

    fn by_name<'a>(servers: &'a [AmpServer], name: &str) -> &'a AmpServer {
        servers.iter().find(|s| s.name == name).unwrap()
    }

    fn endpoint(name: &str, ep: &str) -> WireEndpoint {
        WireEndpoint {
            display_name: name.to_owned(),
            endpoint: ep.to_owned(),
        }
    }

    fn client(server: &MockServer) -> AmpClient {
        crate::install_crypto_provider();
        AmpClient::with_http(
            reqwest::Client::new(),
            server.uri(),
            "bot".to_owned(),
            SecretString::new("pw".into()),
        )
    }

    fn json(body: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/json")
    }

    #[test]
    fn parses_fixture_instances() {
        let servers = fixture_servers();
        assert_eq!(servers.len(), 5, "ADS01 must be filtered: {servers:?}");
        assert!(servers.iter().all(|s| s.module != "ADS"));
        assert_eq!(servers[0].name, "GregTech: New Horizon", "running first");

        let gt = by_name(&servers, "GregTech: New Horizon");
        assert_eq!(gt.instance_name, "GregTechNewHorizon01");
        assert_eq!(gt.id, "ad13df52-a04f-4614-b6e7-9172fdd696ae");
        assert!(gt.running);
        assert_eq!(gt.players, Some((0, 20)));
        assert_eq!(gt.memory_mb, Some((9496, 10240)));
        assert_eq!(gt.address.as_deref(), Some("192.0.2.10:25566"));
    }

    #[test]
    fn running_requires_app_state_ready() {
        // AMP's instance process is up, but the game itself is stopped (0),
        // failed (100) or updating (75): not online, metrics stale.
        for state in [0, 100, 75, -1] {
            let raw = one_instance(&format!(
                r#""Running":true,"AppState":{state},"Metrics":{{"Active Users":{{"RawValue":3,"MaxValue":20}}}}"#
            ));
            let servers = project(serde_json::from_str(&raw).unwrap());
            assert!(!servers[0].running, "AppState {state}");
            assert_eq!(servers[0].players, None, "AppState {state}");
        }
        let raw = one_instance(
            r#""Running":true,"AppState":20,"Metrics":{"Active Users":{"RawValue":3,"MaxValue":20}}"#,
        );
        let servers = project(serde_json::from_str(&raw).unwrap());
        assert!(servers[0].running);
        assert_eq!(servers[0].players, Some((3, 20)));
    }

    #[test]
    fn nulls_do_not_break_the_list() {
        let raw = one_instance(
            r#""Running":null,"AppState":null,"Metrics":null,"ApplicationEndpoints":null"#,
        )
        .replace(r#""FriendlyName":"X""#, r#""FriendlyName":null"#);
        let servers = project(serde_json::from_str(&raw).unwrap());
        assert_eq!(servers.len(), 1);
        assert!(!servers[0].running);

        let raw = one_instance(
            r#""Running":true,"AppState":20,"Metrics":{"CPU Usage":{"RawValue":null,"MaxValue":100}},"ApplicationEndpoints":[{"DisplayName":null,"Endpoint":null}]"#,
        );
        let servers = project(serde_json::from_str(&raw).unwrap());
        assert!(servers[0].running);
        assert_eq!(servers[0].address, None);
    }

    #[test]
    fn stopped_instance_drops_stale_metrics() {
        let servers = fixture_servers();
        let vs = by_name(&servers, "Vintage Story Forever World");
        assert!(!vs.running);
        assert_eq!(vs.players, None);
        assert_eq!(vs.memory_mb, None);
        assert_eq!(vs.address.as_deref(), Some("192.0.2.10:42420"));
    }

    #[test]
    fn pick_address_skips_wildcard_and_sftp() {
        let arma = [
            endpoint("Application Address", "0.0.0.0:2001"),
            endpoint("SFTP Server", "0.0.0.0:2224"),
            endpoint("Unknown Endpoint", "192.0.2.10:2001"),
        ];
        assert_eq!(pick_address(&arma).as_deref(), Some("192.0.2.10:2001"));

        let only_wildcard = [endpoint("Application Address", "0.0.0.0:2001")];
        assert_eq!(
            pick_address(&only_wildcard).as_deref(),
            Some("0.0.0.0:2001")
        );

        let only_sftp = [endpoint("SFTP Server", "192.0.2.10:2230")];
        assert_eq!(pick_address(&only_sftp), None);
    }

    #[test]
    fn parse_uptime_variants() {
        let d =
            |d: u64, h: u64, m: u64, s: u64| Duration::from_secs(((d * 24 + h) * 60 + m) * 60 + s);
        assert_eq!(parse_uptime("5:23:48:43"), Some(d(5, 23, 48, 43)));
        assert_eq!(parse_uptime("23:48:43"), Some(d(0, 23, 48, 43)));
        assert_eq!(parse_uptime("garbage"), None);
        assert_eq!(parse_uptime(""), None);
    }

    #[tokio::test]
    async fn list_relogs_once_on_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/API/Core/Login"))
            .respond_with(json(r#"{"success":true,"sessionID":"s1"}"#))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/API/ADSModule/GetInstances"))
            .respond_with(json(UNAUTHORIZED))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/API/ADSModule/GetInstances"))
            .respond_with(json(INSTANCES))
            .mount(&server)
            .await;

        let servers = client(&server).servers().await.unwrap();
        assert_eq!(servers.len(), 5);
    }

    #[tokio::test]
    async fn repeated_unauthorized_is_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/API/Core/Login"))
            .respond_with(json(r#"{"success":true,"sessionID":"s1"}"#))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/API/ADSModule/GetInstances"))
            .respond_with(json(UNAUTHORIZED))
            .mount(&server)
            .await;

        assert_eq!(client(&server).servers().await, Err(AmpError::Auth));
    }

    #[tokio::test]
    async fn permission_denied_is_not_a_relogin() {
        let server = MockServer::start().await;
        let base = "/API/ADSModule/Servers/abc/API/Core";
        Mock::given(method("POST"))
            .and(path(format!("{base}/Login")))
            .respond_with(json(r#"{"success":true,"sessionID":"i1"}"#))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{base}/GetStatus")))
            .respond_with(json(FORBIDDEN))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{base}/GetUserList")))
            .respond_with(json(FORBIDDEN))
            .mount(&server)
            .await;

        assert_eq!(
            client(&server).details("abc").await,
            Err(AmpError::Permission)
        );
    }

    #[tokio::test]
    async fn details_keeps_uptime_when_user_list_fails() {
        let server = MockServer::start().await;
        let base = "/API/ADSModule/Servers/abc/API/Core";
        Mock::given(method("POST"))
            .and(path(format!("{base}/Login")))
            .respond_with(json(r#"{"success":true,"sessionID":"i1"}"#))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{base}/GetStatus")))
            .respond_with(json(STATUS))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{base}/GetUserList")))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let details = client(&server).details("abc").await.unwrap();
        assert!(details.uptime.is_some());
        assert!(details.players.is_empty());
    }

    #[tokio::test]
    async fn error_envelope_without_stacktrace_is_not_player_names() {
        let server = MockServer::start().await;
        let base = "/API/ADSModule/Servers/abc/API/Core";
        Mock::given(method("POST"))
            .and(path(format!("{base}/Login")))
            .respond_with(json(r#"{"success":true,"sessionID":"i1"}"#))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{base}/GetStatus")))
            .respond_with(json(STATUS))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{base}/GetUserList")))
            .respond_with(json(
                r#"{"Title":"Instance Unavailable","Message":"Module not ready"}"#,
            ))
            .mount(&server)
            .await;

        let details = client(&server).details("abc").await.unwrap();
        assert!(details.players.is_empty(), "{:?}", details.players);
    }

    #[tokio::test]
    async fn login_failure_is_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/API/Core/Login"))
            .respond_with(json(r#"{"success":false,"sessionID":null}"#))
            .expect(1)
            .mount(&server)
            .await;

        assert_eq!(client(&server).servers().await, Err(AmpError::Auth));
    }

    #[tokio::test]
    async fn unreachable_is_unavailable() {
        crate::install_crypto_provider();
        let c = AmpClient::with_http(
            reqwest::Client::new(),
            "http://127.0.0.1:1",
            "bot".to_owned(),
            SecretString::new("pw".into()),
        );
        assert_eq!(c.servers().await, Err(AmpError::Unavailable));
    }

    #[tokio::test]
    async fn details_logs_into_instance() {
        let server = MockServer::start().await;
        let base = "/API/ADSModule/Servers/abc/API/Core";
        Mock::given(method("POST"))
            .and(path(format!("{base}/Login")))
            .respond_with(json(r#"{"success":true,"sessionID":"i1"}"#))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{base}/GetStatus")))
            .respond_with(json(STATUS))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{base}/GetUserList")))
            .respond_with(json(r#"{"u1":"Steve","u2":"Alex"}"#))
            .mount(&server)
            .await;

        let details = client(&server).details("abc").await.unwrap();
        assert_eq!(
            details,
            AmpDetails {
                uptime: Some(Duration::from_secs(((5 * 24 + 23) * 60 + 48) * 60 + 43)),
                players: vec!["Alex".to_owned(), "Steve".to_owned()],
            }
        );
    }
}
