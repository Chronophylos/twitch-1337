//! On-demand `lookup_network` chat-turn tool: IP, AS number and domain
//! ownership via RDAP, with optional ipinfo enrichment for IPs.

use std::net::IpAddr;

use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use serde::Deserialize;
use serde_json::json;
use url::Host;

use crate::ipinfo::IpinfoClient;
use crate::rdap::{RdapClient, RdapError};
use crate::util::net::is_non_public_ip;

pub const LOOKUP_NETWORK_TOOL_NAME: &str = "lookup_network";

/// Top-level labels with no public registry behind them (RFC 6761, 6762,
/// 7686, 8375, 9476, and `.internal`). `arpa` also covers `home.arpa` and
/// reverse-DNS names, which would leak the address they encode.
const SPECIAL_USE_TLDS: [&str; 9] = [
    "localhost",
    "local",
    "internal",
    "test",
    "invalid",
    "example",
    "onion",
    "alt",
    "arpa",
];

#[derive(Debug, Deserialize)]
struct LookupNetworkArgs {
    query: String,
}

/// What the query names, after validation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Ip(IpAddr),
    Asn(u32),
    /// Lowercase ASCII (punycode) domain name.
    Domain(String),
}

/// Why a query never reaches the network.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rejection {
    Invalid(&'static str),
    NotPublic,
}

pub fn lookup_network_tool() -> ToolDefinition {
    ToolDefinition {
        name: LOOKUP_NETWORK_TOOL_NAME.into(),
        description: "Schlag nach, wem eine IP-Adresse, ein AS (Autonomous System) oder eine \
            Domain gehört — aus den offiziellen Registry-Daten (RDAP). Nutz das bei Fragen wie \
            „wem gehört 1.1.1.1?“, „was ist AS3320?“, „wer betreibt dieses Netz?“ oder „bei \
            welchem Registrar liegt example.de?“. Liefert für IPs Netzname, Bereich, Registry, \
            Land, Inhaber, Abuse-Kontakt und, falls verfügbar, Origin-AS und Standort; für AS \
            Name, Inhaber und Registry; für Domains Registrar, Registrierungs-, Ablauf- und \
            Änderungsdatum, Status und Nameserver. Private, lokale oder reservierte Adressen \
            werden abgelehnt (not_public). Geschwärzte Inhaberdaten bleiben geschwärzt — rate \
            niemals, wem etwas gehört. not_found heißt: kein Eintrag oder die Registry bietet \
            kein RDAP an, nicht zwingend, dass es die Domain nicht gibt."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Genau eins von: IPv4- oder IPv6-Adresse (1.1.1.1, 2606:4700::1111), AS-Nummer immer mit AS-Präfix (AS3320), oder Domainname ohne Schema und Pfad (example.de).",
                }
            },
            "required": ["query"]
        }),
    }
}

/// Classify the query. Nothing here touches the network: non-public input
/// is refused before any RDAP or ipinfo request is built.
fn classify(raw: &str) -> Result<Target, Rejection> {
    let q = raw.trim();
    if q.is_empty() {
        return Err(Rejection::Invalid("query must not be empty"));
    }

    let unbracketed = q
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(q);
    if let Ok(ip) = unbracketed.parse::<IpAddr>() {
        return if is_non_public_ip(ip) {
            Err(Rejection::NotPublic)
        } else {
            Ok(Target::Ip(ip))
        };
    }

    if let Some(prefix) = q.get(..2)
        && prefix.eq_ignore_ascii_case("as")
    {
        let digits = q[2..].trim_start();
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            let Ok(asn) = digits.parse::<u32>() else {
                return Err(Rejection::Invalid("AS number out of range"));
            };
            return if is_non_public_asn(asn) {
                Err(Rejection::NotPublic)
            } else {
                Ok(Target::Asn(asn))
            };
        }
        // Not an ASN after all ("asus.com"); fall through to domain.
    }

    if q.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Rejection::Invalid(
            "bare numbers are ambiguous; write AS numbers with the AS prefix, e.g. AS3320",
        ));
    }

    classify_domain(q)
}

fn classify_domain(q: &str) -> Result<Target, Rejection> {
    const SHAPE: &str = "query must be an IP address, an AS number like AS3320, or a bare \
        domain name like example.de (no scheme, path, or port)";
    if q.contains(['/', ':', '@', '?', '#', ' ', '\\']) {
        return Err(Rejection::Invalid(SHAPE));
    }
    let name = q.strip_suffix('.').unwrap_or(q);
    // WHATWG host parsing lowercases and converts IDNs to punycode. A
    // numeric shape it reads as IPv4 (`0x7f.1`) is not a canonical address
    // and not a domain either.
    let Ok(Host::Domain(ascii)) = Host::parse(name) else {
        return Err(Rejection::Invalid(SHAPE));
    };
    let labels: Vec<&str> = ascii.split('.').collect();
    let label_ok = |l: &&str| {
        (1..=63).contains(&l.len())
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    let Some(tld) = labels.last() else {
        return Err(Rejection::Invalid(SHAPE));
    };
    if ascii.len() > 253 || !labels.iter().all(label_ok) {
        return Err(Rejection::Invalid(SHAPE));
    }
    if SPECIAL_USE_TLDS.contains(tld) {
        return Err(Rejection::NotPublic);
    }
    if labels.len() < 2 || tld.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Rejection::Invalid(SHAPE));
    }
    Ok(Target::Domain(ascii))
}

/// AS_TRANS, documentation, private-use and reserved AS numbers (RFC 5398,
/// 6793, 6996, 7300, IANA special registry). None has a public holder.
fn is_non_public_asn(asn: u32) -> bool {
    matches!(asn, 0 | 23456 | 64496..=131_071 | 4_200_000_000..)
}

pub async fn execute_lookup_network(
    rdap: &RdapClient,
    ipinfo: Option<&IpinfoClient>,
    call: &ToolCall,
) -> ToolResultMessage {
    let args = match call.parse_args::<LookupNetworkArgs>() {
        Ok(a) => a,
        Err(e) => {
            return ToolResultMessage::for_call(
                call,
                json!({
                    "error": "invalid_arguments",
                    "details": e.to_string(),
                })
                .to_string(),
            );
        }
    };

    let target = match classify(&args.query) {
        Ok(t) => t,
        Err(Rejection::Invalid(details)) => {
            return ToolResultMessage::for_call(
                call,
                json!({ "error": "invalid_arguments", "details": details }).to_string(),
            );
        }
        Err(Rejection::NotPublic) => {
            return ToolResultMessage::for_call(
                call,
                json!({
                    "error": "not_public",
                    "details": "privater, lokaler oder reservierter Bereich, dazu gibt es keinen öffentlichen Eintrag",
                })
                .to_string(),
            );
        }
    };

    let result = match target {
        Target::Ip(ip) => lookup_ip(rdap, ipinfo, ip).await,
        Target::Asn(asn) => rdap
            .autnum(asn)
            .await
            .map(|a| json!({ "type": "asn", "query": format!("AS{asn}"), "rdap": a })),
        Target::Domain(name) => rdap
            .domain(&name)
            .await
            .map(|d| json!({ "type": "domain", "query": name, "rdap": d })),
    };

    let body = match result {
        Ok(v) => v,
        Err(RdapError::NotFound) => json!({
            "error": "not_found",
            "details": "kein RDAP-Eintrag: nicht registriert, oder die Registry bietet kein RDAP an",
        }),
        Err(error @ RdapError::Upstream(_)) => {
            tracing::warn!(?error, "lookup_network RDAP lookup failed");
            json!({ "error": "upstream_error" })
        }
    };
    ToolResultMessage::for_call(call, body.to_string())
}

/// RDAP and ipinfo run concurrently. RDAP is the base source: its failure
/// fails the lookup. An ipinfo failure only drops the enrichment.
async fn lookup_ip(
    rdap: &RdapClient,
    ipinfo: Option<&IpinfoClient>,
    ip: IpAddr,
) -> Result<serde_json::Value, RdapError> {
    let (network, enrichment) = match ipinfo {
        Some(client) => {
            let (network, enrichment) = tokio::join!(rdap.ip(ip), client.lookup(ip));
            (network, Some(enrichment))
        }
        None => (rdap.ip(ip).await, None),
    };
    let enrichment = match enrichment {
        Some(Ok(record)) => Some(record),
        Some(Err(error)) => {
            tracing::warn!(?error, "lookup_network ipinfo enrichment failed");
            None
        }
        None => None,
    };
    let network = network?;
    let mut v = json!({ "type": "ip", "query": ip.to_string(), "rdap": network });
    if let Some(record) = enrichment {
        v["ipinfo"] = json!(record);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    use secrecy::SecretString;
    use wiremock::matchers::{any, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn call(query: &str) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: LOOKUP_NETWORK_TOOL_NAME.into(),
            arguments: json!({ "query": query }),
            arguments_parse_error: None,
        }
    }

    fn rdap(server: &MockServer) -> RdapClient {
        crate::install_crypto_provider();
        RdapClient::with_base_url(reqwest::Client::new(), server.uri())
    }

    fn ipinfo(server: &MockServer) -> IpinfoClient {
        crate::install_crypto_provider();
        IpinfoClient::with_base_url(
            reqwest::Client::new(),
            server.uri(),
            SecretString::new("test-token".into()),
        )
    }

    fn parse(msg: &ToolResultMessage) -> serde_json::Value {
        serde_json::from_str(&msg.content).expect("json tool result")
    }

    fn vcard_entity(roles: &[&str], name: &str, nested: serde_json::Value) -> serde_json::Value {
        json!({
            "objectClassName": "entity",
            "roles": roles,
            "vcardArray": ["vcard", [
                ["version", {}, "text", "4.0"],
                ["fn", {}, "text", name],
                ["email", {}, "text", "noc@example.net"],
                ["adr", {"label": "Somewhere 1\n12345 City"}, "text", ["", "", "", "", "", "", ""]]
            ]],
            "entities": nested,
        })
    }

    fn rdap_ip_body() -> serde_json::Value {
        json!({
            "objectClassName": "ip network",
            "handle": "APNIC-LABS",
            "name": "APNIC-LABS",
            "startAddress": "1.1.1.0",
            "endAddress": "1.1.1.255",
            "ipVersion": "v4",
            "country": "AU",
            "cidr0_cidrs": [{ "v4prefix": "1.1.1.0", "length": 24 }],
            "port43": "whois.apnic.net",
            "remarks": [{ "description": ["a very long remark the model never needs"] }],
            "entities": [
                vcard_entity(&["registrant"], "APNIC Research and Development", json!([])),
                vcard_entity(&["abuse"], "IRT-APNICRANDNET-AU", json!([])),
            ]
        })
    }

    #[tokio::test]
    async fn ipv4_without_ipinfo_returns_rdap_fields_only() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ip/1.1.1.1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rdap_ip_body()))
            .expect(1)
            .mount(&server)
            .await;

        let v = parse(&execute_lookup_network(&rdap(&server), None, &call("1.1.1.1")).await);

        assert_eq!(v["type"], "ip");
        assert_eq!(v["query"], "1.1.1.1");
        assert_eq!(v["rdap"]["name"], "APNIC-LABS");
        assert_eq!(v["rdap"]["cidr"], json!(["1.1.1.0/24"]));
        assert_eq!(v["rdap"]["range"], "1.1.1.0 - 1.1.1.255");
        assert_eq!(v["rdap"]["registry"], "APNIC");
        assert_eq!(v["rdap"]["country"], "AU");
        assert_eq!(v["rdap"]["org"], "APNIC Research and Development");
        assert_eq!(v["rdap"]["abuse_contact"], "IRT-APNICRANDNET-AU");
        assert!(v.get("ipinfo").is_none(), "no ipinfo without config: {v}");
        // Trimmed: no vCard or remark dumps reach the model.
        assert!(!v.to_string().contains("vcard"), "{v}");
        assert!(!v.to_string().contains("remark"), "{v}");
        assert!(!v.to_string().contains("noc@example.net"), "{v}");
    }

    #[tokio::test]
    async fn ip_lookup_with_ipinfo_merges_geo() {
        let rdap_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ip/1.1.1.1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rdap_ip_body()))
            .expect(1)
            .mount(&rdap_server)
            .await;
        let ipinfo_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/1.1.1.1/json"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ip": "1.1.1.1",
                "hostname": "one.one.one.one",
                "city": "Brisbane",
                "region": "Queensland",
                "country": "AU",
                "loc": "-27.4820,153.0136",
                "org": "AS13335 Cloudflare, Inc.",
                "postal": "4101",
                "timezone": "Australia/Brisbane",
            })))
            .expect(1)
            .mount(&ipinfo_server)
            .await;

        let v = parse(
            &execute_lookup_network(
                &rdap(&rdap_server),
                Some(&ipinfo(&ipinfo_server)),
                &call("1.1.1.1"),
            )
            .await,
        );

        assert_eq!(v["rdap"]["name"], "APNIC-LABS");
        assert_eq!(v["ipinfo"]["city"], "Brisbane");
        assert_eq!(v["ipinfo"]["region"], "Queensland");
        assert_eq!(v["ipinfo"]["country"], "AU");
        assert_eq!(v["ipinfo"]["asn"], "AS13335");
        assert_eq!(v["ipinfo"]["as_name"], "Cloudflare, Inc.");
        assert_eq!(v["ipinfo"]["hostname"], "one.one.one.one");
        assert!(v["ipinfo"].get("loc").is_none(), "{v}");
        assert!(v["ipinfo"].get("postal").is_none(), "{v}");
    }

    #[tokio::test]
    async fn ipinfo_failure_keeps_the_rdap_answer() {
        let rdap_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rdap_ip_body()))
            .mount(&rdap_server)
            .await;
        let ipinfo_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&ipinfo_server)
            .await;

        let v = parse(
            &execute_lookup_network(
                &rdap(&rdap_server),
                Some(&ipinfo(&ipinfo_server)),
                &call("1.1.1.1"),
            )
            .await,
        );

        assert_eq!(v["rdap"]["name"], "APNIC-LABS");
        assert!(v.get("ipinfo").is_none(), "{v}");
    }

    #[tokio::test]
    async fn asn_lookup_returns_autnum_record() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/autnum/3320"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "objectClassName": "autnum",
                "handle": "AS3320",
                "name": "DTAG",
                "startAutnum": 3320,
                "endAutnum": 3320,
                "links": [{
                    "rel": "self",
                    "href": "https://rdap.db.ripe.net/autnum/3320",
                    "value": "https://rdap.db.ripe.net/autnum/3320"
                }],
                "entities": [
                    vcard_entity(&["registrant"], "Deutsche Telekom AG", json!([])),
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let v = parse(&execute_lookup_network(&rdap(&server), None, &call("as3320")).await);

        assert_eq!(v["type"], "asn");
        assert_eq!(v["query"], "AS3320");
        assert_eq!(v["rdap"]["asn"], "AS3320");
        assert_eq!(v["rdap"]["name"], "DTAG");
        assert_eq!(v["rdap"]["holder"], "Deutsche Telekom AG");
        assert_eq!(v["rdap"]["registry"], "RIPE NCC");
    }

    #[tokio::test]
    async fn domain_lookup_returns_registrar_dates_status_and_nameservers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/domain/example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "objectClassName": "domain",
                "ldhName": "EXAMPLE.COM",
                "status": ["client delete prohibited", "client transfer prohibited"],
                "events": [
                    { "eventAction": "registration", "eventDate": "1995-08-14T04:00:00Z" },
                    { "eventAction": "expiration", "eventDate": "2026-08-13T04:00:00Z" },
                    { "eventAction": "last changed", "eventDate": "2026-01-16T18:26:50Z" },
                    { "eventAction": "last update of RDAP database", "eventDate": "2026-11-20T10:00:00Z" }
                ],
                "nameservers": [
                    { "objectClassName": "nameserver", "ldhName": "A.IANA-SERVERS.NET" },
                    { "objectClassName": "nameserver", "ldhName": "B.IANA-SERVERS.NET" }
                ],
                "entities": [
                    vcard_entity(&["registrar"], "RESERVED-Internet Assigned Numbers Authority", json!([
                        vcard_entity(&["abuse"], "Registrar Abuse", json!([]))
                    ])),
                    vcard_entity(&["registrant"], "REDACTED FOR PRIVACY", json!([])),
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let v = parse(&execute_lookup_network(&rdap(&server), None, &call("Example.COM.")).await);

        assert_eq!(v["type"], "domain");
        assert_eq!(v["query"], "example.com");
        let d = &v["rdap"];
        assert_eq!(d["domain"], "example.com");
        assert_eq!(
            d["registrar"],
            "RESERVED-Internet Assigned Numbers Authority"
        );
        assert_eq!(d["registrant"], "REDACTED FOR PRIVACY");
        assert_eq!(d["registered"], "1995-08-14T04:00:00Z");
        assert_eq!(d["expires"], "2026-08-13T04:00:00Z");
        assert_eq!(d["last_changed"], "2026-01-16T18:26:50Z");
        assert_eq!(
            d["status"],
            json!(["client delete prohibited", "client transfer prohibited"])
        );
        assert_eq!(
            d["nameservers"],
            json!(["a.iana-servers.net", "b.iana-servers.net"])
        );
    }

    #[tokio::test]
    async fn rdap_404_is_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let v = parse(&execute_lookup_network(&rdap(&server), None, &call("nope-xyz.de")).await);
        assert_eq!(v["error"], "not_found");
    }

    #[tokio::test]
    async fn rdap_5xx_is_upstream_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let v = parse(&execute_lookup_network(&rdap(&server), None, &call("AS3320")).await);
        assert_eq!(v["error"], "upstream_error");
    }

    /// The gate is worthless if it does not actually prevent the call: no
    /// internal address may ever be sent to RDAP or ipinfo.
    #[tokio::test]
    async fn non_public_input_is_rejected_without_any_request() {
        let rdap_server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&rdap_server)
            .await;
        let ipinfo_server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&ipinfo_server)
            .await;
        let rdap = rdap(&rdap_server);
        let ipinfo = ipinfo(&ipinfo_server);

        for query in [
            "10.0.0.1",
            "192.168.178.1",
            "127.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "224.0.0.1",
            "192.0.2.1",
            "240.0.0.1",
            "0.0.0.0",
            "::1",
            "[::1]",
            "fe80::1",
            "fd00::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:10.0.0.1",
            "AS64512",
            "AS4200000001",
            "AS0",
            "fritzbox.local",
            "router.home.arpa",
            "localhost",
        ] {
            let v = parse(&execute_lookup_network(&rdap, Some(&ipinfo), &call(query)).await);
            assert_eq!(v["error"], "not_public", "{query}: {v}");
        }

        assert!(
            rdap_server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty()
        );
        assert!(
            ipinfo_server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty()
        );
    }

    #[test]
    fn classify_picks_one_contract() {
        assert_eq!(
            classify(" 1.1.1.1 "),
            Ok(Target::Ip("1.1.1.1".parse().expect("ip")))
        );
        assert_eq!(
            classify("[2606:4700::1111]"),
            Ok(Target::Ip("2606:4700::1111".parse().expect("ip")))
        );
        assert_eq!(classify("AS3320"), Ok(Target::Asn(3320)));
        assert_eq!(classify("as3320"), Ok(Target::Asn(3320)));
        assert_eq!(classify("AS 3320"), Ok(Target::Asn(3320)));
        assert!(matches!(classify("3320"), Err(Rejection::Invalid(_))));
        assert!(matches!(
            classify("AS99999999999"),
            Err(Rejection::Invalid(_))
        ));
        assert_eq!(classify("asus.com"), Ok(Target::Domain("asus.com".into())));
        assert_eq!(
            classify("München.de"),
            Ok(Target::Domain("xn--mnchen-3ya.de".into()))
        );
        for bad in [
            "",
            "https://example.de",
            "example.de/path",
            "example.de:443",
            "nur prosa",
            "0x7f.1",
            "-bad-.de",
            "under_score.de",
        ] {
            assert!(
                matches!(classify(bad), Err(Rejection::Invalid(_))),
                "{bad}: {:?}",
                classify(bad)
            );
        }
    }

    #[test]
    fn special_asns_are_not_public() {
        for asn in [
            0,
            23456,
            64496,
            64511,
            64512,
            65534,
            65535,
            65551,
            4_294_967_295,
        ] {
            assert!(is_non_public_asn(asn), "{asn}");
        }
        for asn in [1, 3320, 13335, 64495, 131_072, 4_199_999_999] {
            assert!(!is_non_public_asn(asn), "{asn}");
        }
    }
}
