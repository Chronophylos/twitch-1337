//! Keyless RDAP client backing Aurora's `lookup_network` tool. Queries go to
//! `rdap.org`, which resolves the IANA bootstrap registries and redirects to
//! the authoritative RIR or domain registry. Responses are projected down to
//! a handful of fields; vCards are never forwarded raw.

use std::net::IpAddr;
use std::time::Duration;

use eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize};

use crate::APP_USER_AGENT;
use crate::util::http::{SendTraced as _, read_capped};
use crate::util::net::is_blocked_host_literal;

pub const DEFAULT_BASE_URL: &str = "https://rdap.org";
const TIMEOUT: Duration = Duration::from_secs(10);
/// rdap.org answers with one redirect to the authoritative server; an RIR
/// may hand transferred space on once more. Five hops is generous.
const MAX_REDIRECTS: usize = 5;
/// Body cap. A network with a long entity tree is tens of KiB.
const MAX_BODY_BYTES: u64 = 1024 * 1024;
/// Nameservers forwarded to the model.
const MAX_NAMESERVERS: usize = 8;
/// Entity nesting the role search descends into (registrant → abuse is 2).
const MAX_ENTITY_DEPTH: usize = 4;

/// Why an RDAP lookup failed. The tool branches on `NotFound`.
#[derive(Debug, thiserror::Error)]
pub enum RdapError {
    /// The registry has no record, or the TLD/range has no RDAP service in
    /// the IANA bootstrap.
    #[error("no RDAP record")]
    NotFound,
    #[error("RDAP lookup failed")]
    Upstream(#[source] eyre::Report),
}

/// Trimmed RDAP IP network record.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct IpNetwork {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cidr: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abuse_contact: Option<String>,
    /// Originating AS numbers, when the registry publishes them (ARIN's
    /// `arin_originas0` extension). Other RIRs leave it out of RDAP.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub origin_as: Vec<String>,
}

/// Trimmed RDAP autnum record.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Autnum {
    pub asn: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
}

/// Trimmed RDAP domain record. Registrant data appears only as the
/// registry publishes it; a redacted name stays redacted.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Domain {
    pub domain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registrar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registrant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registered: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_changed: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub status: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub nameservers: Vec<String>,
}

// --- RDAP wire types (RFC 9083): only the fields the projection reads. ---

#[derive(Debug, Deserialize)]
struct WireEntity {
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default, rename = "vcardArray")]
    vcard_array: Option<serde_json::Value>,
    #[serde(default)]
    entities: Vec<WireEntity>,
}

#[derive(Debug, Deserialize)]
struct WireLink {
    #[serde(default)]
    rel: Option<String>,
    #[serde(default)]
    href: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireCidr {
    #[serde(default)]
    v4prefix: Option<String>,
    #[serde(default)]
    v6prefix: Option<String>,
    #[serde(default)]
    length: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct WireIpNetwork {
    #[serde(default)]
    handle: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "startAddress")]
    start_address: Option<String>,
    #[serde(default, rename = "endAddress")]
    end_address: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    port43: Option<String>,
    #[serde(default)]
    cidr0_cidrs: Vec<WireCidr>,
    #[serde(default)]
    arin_originas0_originautnums: Vec<u32>,
    #[serde(default)]
    entities: Vec<WireEntity>,
    #[serde(default)]
    links: Vec<WireLink>,
}

#[derive(Debug, Deserialize)]
struct WireAutnum {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    port43: Option<String>,
    #[serde(default)]
    entities: Vec<WireEntity>,
    #[serde(default)]
    links: Vec<WireLink>,
}

#[derive(Debug, Deserialize)]
struct WireEvent {
    #[serde(default, rename = "eventAction")]
    action: Option<String>,
    #[serde(default, rename = "eventDate")]
    date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireNameserver {
    #[serde(default, rename = "ldhName")]
    ldh_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireDomain {
    #[serde(default, rename = "ldhName")]
    ldh_name: Option<String>,
    #[serde(default)]
    status: Vec<String>,
    #[serde(default)]
    events: Vec<WireEvent>,
    #[serde(default)]
    nameservers: Vec<WireNameserver>,
    #[serde(default)]
    entities: Vec<WireEntity>,
}

pub struct RdapClient {
    http: reqwest::Client,
    base_url: String,
}

impl RdapClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: http_client()?,
            base_url: DEFAULT_BASE_URL.to_string(),
        })
    }

    /// Test hook: custom base URL (e.g. wiremock). Caller supplies the HTTP
    /// client; production [`Self::new`] uses [`http_client`].
    pub fn with_base_url(http: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into(),
        }
    }

    pub async fn ip(&self, ip: IpAddr) -> Result<IpNetwork, RdapError> {
        let wire: WireIpNetwork = self.get(&format!("ip/{ip}")).await?;
        Ok(project_ip(wire))
    }

    pub async fn autnum(&self, asn: u32) -> Result<Autnum, RdapError> {
        let wire: WireAutnum = self.get(&format!("autnum/{asn}")).await?;
        Ok(project_autnum(asn, wire))
    }

    /// `domain` must already be a validated ASCII (punycode) name.
    pub async fn domain(&self, domain: &str) -> Result<Domain, RdapError> {
        let wire: WireDomain = self.get(&format!("domain/{domain}")).await?;
        Ok(project_domain(domain, wire))
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, RdapError> {
        let url = format!("{}/{path}", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/rdap+json")
            .send_traced()
            .await
            .wrap_err("RDAP request")
            .map_err(RdapError::Upstream)?;

        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(RdapError::NotFound);
        }
        if !status.is_success() {
            return Err(RdapError::Upstream(eyre!("RDAP returned {status}")));
        }

        let body = read_capped(resp, MAX_BODY_BYTES)
            .await
            .wrap_err("read RDAP response body")
            .map_err(RdapError::Upstream)?;
        serde_json::from_slice(&body)
            .wrap_err("decode RDAP response")
            .map_err(RdapError::Upstream)
    }
}

/// A redirect the RDAP policy refused. Travels through reqwest's source
/// chain; the target is for logs only.
#[derive(Debug, thiserror::Error)]
#[error("RDAP redirect refused: {0}")]
struct RedirectRefused(String);

/// rdap.org and the registries it points at are trusted to redirect, but
/// only onward to another public HTTPS host: never a downgrade to plain
/// HTTP, an internal IP literal or `localhost`, and at most
/// [`MAX_REDIRECTS`] hops.
fn http_client() -> Result<reqwest::Client> {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error(RedirectRefused("too many redirects".into()))
        } else if !redirect_allowed(attempt.url()) {
            let target = attempt.url().to_string();
            attempt.error(RedirectRefused(target))
        } else {
            attempt.follow()
        }
    });
    reqwest::Client::builder()
        .user_agent(APP_USER_AGENT)
        .timeout(TIMEOUT)
        .redirect(policy)
        .build()
        .wrap_err("build RDAP HTTP client")
}

fn redirect_allowed(url: &reqwest::Url) -> bool {
    url.scheme() == "https" && !is_blocked_host_literal(url)
}

fn project_ip(w: WireIpNetwork) -> IpNetwork {
    let cidr = w
        .cidr0_cidrs
        .iter()
        .filter_map(|c| {
            let prefix = c.v4prefix.as_deref().or(c.v6prefix.as_deref())?;
            Some(format!("{prefix}/{}", c.length?))
        })
        .collect();
    let range = match (w.start_address, w.end_address) {
        (Some(start), Some(end)) => Some(format!("{start} - {end}")),
        _ => None,
    };
    IpNetwork {
        name: non_empty(w.name),
        handle: non_empty(w.handle),
        cidr,
        range,
        registry: registry(w.port43.as_deref(), &w.links),
        country: non_empty(w.country),
        org: role_name(&w.entities, "registrant", 0),
        abuse_contact: role_name(&w.entities, "abuse", 0),
        origin_as: w
            .arin_originas0_originautnums
            .iter()
            .map(|n| format!("AS{n}"))
            .collect(),
    }
}

fn project_autnum(asn: u32, w: WireAutnum) -> Autnum {
    Autnum {
        asn: format!("AS{asn}"),
        name: non_empty(w.name),
        holder: role_name(&w.entities, "registrant", 0),
        registry: registry(w.port43.as_deref(), &w.links),
        country: non_empty(w.country),
    }
}

fn project_domain(queried: &str, w: WireDomain) -> Domain {
    let event = |action: &str| {
        w.events
            .iter()
            .find(|e| {
                e.action
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(action))
            })
            .and_then(|e| e.date.clone())
    };
    Domain {
        domain: w
            .ldh_name
            .as_deref()
            .map_or_else(|| queried.to_string(), str::to_ascii_lowercase),
        registrar: role_name(&w.entities, "registrar", 0),
        registrant: role_name(&w.entities, "registrant", 0),
        registered: event("registration"),
        expires: event("expiration"),
        last_changed: event("last changed"),
        status: w.status,
        nameservers: w
            .nameservers
            .iter()
            .filter_map(|n| n.ldh_name.as_deref())
            .map(str::to_ascii_lowercase)
            .take(MAX_NAMESERVERS)
            .collect(),
    }
}

/// Name of the first entity carrying `role`, searched depth-first in
/// document order. Abuse contacts usually nest under the registrant.
fn role_name(entities: &[WireEntity], role: &str, depth: usize) -> Option<String> {
    if depth > MAX_ENTITY_DEPTH {
        return None;
    }
    entities.iter().find_map(|e| {
        let has_role = e.roles.iter().any(|r| r.eq_ignore_ascii_case(role));
        has_role
            .then(|| entity_name(e))
            .flatten()
            .or_else(|| role_name(&e.entities, role, depth + 1))
    })
}

/// The vCard `fn` (formatted name), falling back to `org`. jCard layout:
/// `["vcard", [[name, params, type, value], ...]]`.
fn entity_name(e: &WireEntity) -> Option<String> {
    let props = e.vcard_array.as_ref()?.get(1)?.as_array()?;
    let text = |prop: &str| {
        props.iter().find_map(|p| {
            let p = p.as_array()?;
            (p.first()?.as_str()? == prop)
                .then(|| p.get(3)?.as_str())
                .flatten()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
    };
    text("fn").or_else(|| text("org"))
}

/// Registry that answered, named from its whois host or self link.
fn registry(port43: Option<&str>, links: &[WireLink]) -> Option<String> {
    let self_host = || {
        links
            .iter()
            .find(|l| l.rel.as_deref() == Some("self"))
            .and_then(|l| reqwest::Url::parse(l.href.as_deref()?).ok())
            .and_then(|u| u.host_str().map(str::to_string))
    };
    let host = port43
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(self_host)?
        .to_ascii_lowercase();
    let rir = [
        ("arin", "ARIN"),
        ("ripe", "RIPE NCC"),
        ("apnic", "APNIC"),
        ("lacnic", "LACNIC"),
        ("afrinic", "AFRINIC"),
    ]
    .into_iter()
    .find(|(needle, _)| host.contains(needle))
    .map(|(_, name)| name.to_string());
    Some(rir.unwrap_or(host))
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(roles: &[&str], name: &str, nested: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "roles": roles,
            "vcardArray": ["vcard", [
                ["version", {}, "text", "4.0"],
                ["fn", {}, "text", name],
                ["adr", {"label": "Somewhere 1"}, "text", ["", "", "", "", "", "", ""]]
            ]],
            "entities": nested,
        })
    }

    #[test]
    fn ip_projection_finds_nested_abuse_and_names_the_rir() {
        let wire: WireIpNetwork = serde_json::from_value(serde_json::json!({
            "handle": "NET-8-8-8-0-2",
            "name": "GOGL",
            "startAddress": "8.8.8.0",
            "endAddress": "8.8.8.255",
            "cidr0_cidrs": [{ "v4prefix": "8.8.8.0", "length": 24 }],
            "arin_originas0_originautnums": [15169],
            "port43": "whois.arin.net",
            "entities": [entity(&["registrant"], "Google LLC", vec![
                entity(&["abuse"], "Abuse", vec![])
            ])]
        }))
        .expect("wire");

        let p = project_ip(wire);
        assert_eq!(p.name.as_deref(), Some("GOGL"));
        assert_eq!(p.cidr, vec!["8.8.8.0/24".to_string()]);
        assert_eq!(p.range.as_deref(), Some("8.8.8.0 - 8.8.8.255"));
        assert_eq!(p.registry.as_deref(), Some("ARIN"));
        assert_eq!(p.org.as_deref(), Some("Google LLC"));
        assert_eq!(p.abuse_contact.as_deref(), Some("Abuse"));
        assert_eq!(p.origin_as, vec!["AS15169".to_string()]);
    }

    #[test]
    fn registry_falls_back_to_self_link_host() {
        let links = [WireLink {
            rel: Some("self".into()),
            href: Some("https://rdap.db.ripe.net/autnum/3320".into()),
        }];
        assert_eq!(registry(None, &links).as_deref(), Some("RIPE NCC"));
        let other = [WireLink {
            rel: Some("self".into()),
            href: Some("https://rdap.example-registry.net/x".into()),
        }];
        assert_eq!(
            registry(None, &other).as_deref(),
            Some("rdap.example-registry.net")
        );
        assert_eq!(registry(None, &[]), None);
    }

    /// The projection forwards the redacted string verbatim and never
    /// reaches for another field to fill the gap.
    #[test]
    fn redacted_registrant_stays_redacted() {
        let wire: WireDomain = serde_json::from_value(serde_json::json!({
            "ldhName": "EXAMPLE.DE",
            "entities": [entity(&["registrant"], "REDACTED FOR PRIVACY", vec![])]
        }))
        .expect("wire");
        let p = project_domain("example.de", wire);
        assert_eq!(p.domain, "example.de");
        assert_eq!(p.registrant.as_deref(), Some("REDACTED FOR PRIVACY"));
    }

    #[test]
    fn redirect_policy_only_follows_public_https() {
        let url = |s: &str| reqwest::Url::parse(s).expect("url");
        for allowed in [
            "https://rdap.arin.net/registry/ip/8.8.8.8",
            "https://rdap.db.ripe.net/autnum/3320",
            "https://[2606:4700::1111]/ip/1.1.1.1",
        ] {
            assert!(redirect_allowed(&url(allowed)), "{allowed}");
        }
        for refused in [
            "http://rdap.arin.net/registry/ip/8.8.8.8",
            "https://10.0.0.1/ip/1.1.1.1",
            "https://127.0.0.1:8080/",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/",
            "https://[::ffff:192.168.1.1]/",
            "https://localhost/",
            "https://rdap.localhost/",
            "file:///etc/passwd",
        ] {
            assert!(!redirect_allowed(&url(refused)), "{refused}");
        }
    }

    /// The production client refuses a redirect to an internal target, so
    /// the second server never sees a request and the lookup fails upstream.
    #[tokio::test]
    async fn redirect_to_an_internal_target_is_not_followed() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        crate::install_crypto_provider();
        let internal = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&internal)
            .await;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/autnum/3320", internal.uri())),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = RdapClient::with_base_url(http_client().expect("rdap client"), server.uri());
        assert!(matches!(
            client.autnum(3320).await,
            Err(RdapError::Upstream(_))
        ));
        // `expect(0)` on `internal` is verified when it drops.
    }
}
