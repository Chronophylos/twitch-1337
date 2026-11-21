use std::collections::HashSet;
use std::time::Duration;

use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::Map;

use crate::APP_USER_AGENT;
use crate::util::http::SendTraced as _;

pub const FILTERED_SUITE_CAP: usize = 3;

pub const ALL_FAMILIES: [PackageFamily; 4] = [
    PackageFamily::Aur,
    PackageFamily::Winget,
    PackageFamily::Debian,
    PackageFamily::Ubuntu,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PackageFamily {
    Aur,
    Winget,
    Debian,
    Ubuntu,
}

impl PackageFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aur => "aur",
            Self::Winget => "winget",
            Self::Debian => "debian",
            Self::Ubuntu => "ubuntu",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedEcosystem;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Package {
    pub repo: String,
    pub visiblename: Option<String>,
    pub srcname: Option<String>,
    #[serde(default)]
    pub binnames: Vec<String>,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PackageHit {
    pub family: PackageFamily,
    pub name: String,
    pub version: String,
    pub url: String,
}

pub fn parse_ecosystem(raw: Option<&str>) -> Result<Vec<PackageFamily>, UnsupportedEcosystem> {
    let key = raw.map(str::trim).unwrap_or("").to_ascii_lowercase();
    match key.as_str() {
        "" => Ok(ALL_FAMILIES.to_vec()),
        "aur" | "arch" => Ok(vec![PackageFamily::Aur]),
        "winget" => Ok(vec![PackageFamily::Winget]),
        "debian" => Ok(vec![PackageFamily::Debian]),
        "ubuntu" => Ok(vec![PackageFamily::Ubuntu]),
        "apt" => Ok(vec![PackageFamily::Debian, PackageFamily::Ubuntu]),
        _ => Err(UnsupportedEcosystem),
    }
}

fn family_of(repo: &str) -> Option<PackageFamily> {
    match repo {
        "aur" => Some(PackageFamily::Aur),
        "winget" => Some(PackageFamily::Winget),
        _ if repo.starts_with("debian") => Some(PackageFamily::Debian),
        _ if repo.starts_with("ubuntu") => Some(PackageFamily::Ubuntu),
        _ => None,
    }
}

fn suite_rank(repo: &str, family: PackageFamily) -> u32 {
    match family {
        PackageFamily::Debian => {
            if repo == "debian_unstable" {
                u32::MAX
            } else if let Some(n) = repo.strip_prefix("debian_") {
                n.parse().unwrap_or(0)
            } else {
                0
            }
        }
        PackageFamily::Ubuntu => repo
            .strip_prefix("ubuntu_")
            .and_then(|rest| {
                let (yy, mm) = rest.split_once('_')?;
                Some(yy.parse::<u32>().ok()? * 100 + mm.parse::<u32>().ok()?)
            })
            .unwrap_or(0),
        PackageFamily::Aur | PackageFamily::Winget => 0,
    }
}

fn package_name(pkg: &Package) -> String {
    pkg.visiblename
        .clone()
        .or_else(|| pkg.srcname.clone())
        .or_else(|| pkg.binnames.first().cloned())
        .unwrap_or_default()
}

fn hit_url(family: PackageFamily, project: &str, name: &str) -> String {
    match family {
        PackageFamily::Aur => format!("https://aur.archlinux.org/packages/{name}"),
        PackageFamily::Debian => format!("https://packages.debian.org/{name}"),
        PackageFamily::Ubuntu => format!("https://packages.ubuntu.com/{name}"),
        PackageFamily::Winget => format!("https://repology.org/project/{project}/information"),
    }
}

pub fn collapse_hits(
    project: &str,
    packages: &[Package],
    families: &[PackageFamily],
    cap_per_family: usize,
) -> (u32, Vec<PackageHit>) {
    let ecosystem_count = packages
        .iter()
        .map(|p| p.repo.as_str())
        .collect::<HashSet<_>>()
        .len() as u32;

    let family_set: HashSet<PackageFamily> = families.iter().copied().collect();
    let mut hits = Vec::new();

    for &family in &ALL_FAMILIES {
        if !family_set.contains(&family) {
            continue;
        }

        let mut repos: Vec<&str> = packages
            .iter()
            .filter_map(|p| {
                family_of(&p.repo)
                    .filter(|&f| f == family)
                    .map(|_| p.repo.as_str())
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        repos.sort_by_key(|b| std::cmp::Reverse(suite_rank(b, family)));
        repos.truncate(cap_per_family);

        for repo in repos {
            if let Some(pkg) = packages.iter().find(|p| p.repo == repo) {
                let name = package_name(pkg);
                hits.push(PackageHit {
                    family,
                    name: name.clone(),
                    version: pkg.version.clone(),
                    url: hit_url(family, project, &name),
                });
            }
        }
    }

    (ecosystem_count, hits)
}

const DEFAULT_BASE_URL: &str = "https://repology.org";
const TIMEOUT: Duration = Duration::from_secs(10);
const MIN_INTERVAL: Duration = Duration::from_secs(1);

struct RateGate {
    last_start: tokio::sync::Mutex<Option<std::time::Instant>>,
}

impl RateGate {
    fn new() -> Self {
        Self {
            last_start: tokio::sync::Mutex::new(None),
        }
    }

    async fn acquire(&self) {
        let mut last = self.last_start.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < MIN_INTERVAL {
                tokio::time::sleep(MIN_INTERVAL - elapsed).await;
            }
        }
        *last = Some(std::time::Instant::now());
    }
}

pub struct RepologyClient {
    http: reqwest::Client,
    base_url: String,
    gate: RateGate,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectLookup {
    Found {
        name: String,
        packages: Vec<Package>,
    },
    NotFound,
}

#[derive(Debug, thiserror::Error)]
pub enum RepologyError {
    #[error("repology unavailable")]
    Unavailable(#[source] eyre::Report),
}

impl RepologyClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .timeout(TIMEOUT)
            .build()
            .wrap_err("build repology HTTP client")?;
        Ok(Self {
            http,
            base_url: DEFAULT_BASE_URL.to_string(),
            gate: RateGate::new(),
        })
    }

    /// Test hook: custom base URL (e.g. wiremock). Caller supplies the HTTP client
    /// (production [`Self::new`] sets [`APP_USER_AGENT`] and a 10s timeout).
    pub fn with_base_url(http: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            gate: RateGate::new(),
        }
    }

    pub async fn fetch_project(&self, query: &str) -> Result<ProjectLookup, RepologyError> {
        self.gate.acquire().await;

        let url = self
            .project_url(query)
            .map_err(RepologyError::Unavailable)?;
        let resp = self
            .http
            .get(url)
            .send_traced()
            .await
            .wrap_err("repology project request")
            .map_err(RepologyError::Unavailable)?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return self.search_project(query).await;
        }

        if !resp.status().is_success() {
            return Err(RepologyError::Unavailable(eyre::eyre!(
                "repology project returned {}",
                resp.status()
            )));
        }

        let packages = resp
            .json::<Vec<Package>>()
            .await
            .wrap_err("decode repology project")
            .map_err(RepologyError::Unavailable)?;
        Ok(ProjectLookup::Found {
            name: query.to_string(),
            packages,
        })
    }

    async fn search_project(&self, query: &str) -> Result<ProjectLookup, RepologyError> {
        self.gate.acquire().await;

        let url = self.search_url(query).map_err(RepologyError::Unavailable)?;
        let resp = self
            .http
            .get(url)
            .send_traced()
            .await
            .wrap_err("repology search request")
            .map_err(RepologyError::Unavailable)?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(ProjectLookup::NotFound);
        }

        if !resp.status().is_success() {
            return Err(RepologyError::Unavailable(eyre::eyre!(
                "repology search returned {}",
                resp.status()
            )));
        }

        let map = resp
            .json::<Map<String, serde_json::Value>>()
            .await
            .wrap_err("decode repology search")
            .map_err(RepologyError::Unavailable)?;

        let Some((name, value)) = map.into_iter().next() else {
            return Ok(ProjectLookup::NotFound);
        };
        let packages: Vec<Package> = serde_json::from_value(value)
            .wrap_err("decode repology search packages")
            .map_err(RepologyError::Unavailable)?;
        Ok(ProjectLookup::Found { name, packages })
    }

    fn base_trimmed(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    fn project_url(&self, query: &str) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&format!("{}/api/v1/project", self.base_trimmed()))?;
        #[expect(
            clippy::map_err_ignore,
            reason = "path_segments_mut fails with () only for cannot-be-a-base URLs"
        )]
        url.path_segments_mut()
            .map_err(|_| eyre::eyre!("invalid repology base URL"))?
            .push(query);
        Ok(url)
    }

    fn search_url(&self, query: &str) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&format!("{}/api/v1/projects/", self.base_trimmed()))?;
        url.query_pairs_mut().append_pair("search", query);
        Ok(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn parse_ecosystem_aliases() {
        assert_eq!(parse_ecosystem(None).unwrap(), ALL_FAMILIES.to_vec());
        assert_eq!(parse_ecosystem(Some("")).unwrap(), ALL_FAMILIES.to_vec());
        assert_eq!(
            parse_ecosystem(Some("arch")).unwrap(),
            vec![PackageFamily::Aur]
        );
        assert_eq!(
            parse_ecosystem(Some("AUR")).unwrap(),
            vec![PackageFamily::Aur]
        );
        assert_eq!(
            parse_ecosystem(Some("apt")).unwrap(),
            vec![PackageFamily::Debian, PackageFamily::Ubuntu]
        );
        assert!(parse_ecosystem(Some("npm")).is_err());
        assert!(parse_ecosystem(Some("docker")).is_err());
        assert!(parse_ecosystem(Some("pypi")).is_err());
        assert!(parse_ecosystem(Some("cargo")).is_err());
    }

    fn pkg(repo: &str, name: &str, version: &str) -> Package {
        Package {
            repo: repo.to_string(),
            visiblename: Some(name.to_string()),
            srcname: Some(name.to_string()),
            binnames: Vec::new(),
            version: version.to_string(),
        }
    }

    #[test]
    fn collapse_unfiltered_one_row_per_family_and_full_count() {
        let packages = vec![
            pkg("aur", "ripgrep-git", "15.1.0"),
            pkg("debian_11", "rust-ripgrep", "12.1.1"),
            pkg("debian_14", "rust-ripgrep", "15.2.0"),
            pkg("debian_unstable", "rust-ripgrep", "15.2.0"),
            pkg("ubuntu_24_04", "rust-ripgrep", "14.1.0"),
            pkg("ubuntu_26_10", "rust-ripgrep", "15.2.0"),
            pkg("nixpkgs_unstable", "ripgrep", "15.2.0"),
            pkg("winget", "BurntSushi.ripgrep", "15.2.0"),
        ];
        let (count, hits) = collapse_hits("ripgrep", &packages, &ALL_FAMILIES, 1);
        assert_eq!(count, 8, "count is unique repos including nix");
        let families: Vec<_> = hits.iter().map(|h| h.family).collect();
        assert_eq!(
            families,
            vec![
                PackageFamily::Aur,
                PackageFamily::Winget,
                PackageFamily::Debian,
                PackageFamily::Ubuntu,
            ]
        );
        assert_eq!(
            hits.iter()
                .filter(|h| h.family == PackageFamily::Debian)
                .count(),
            1
        );
        let deb = hits
            .iter()
            .find(|h| h.family == PackageFamily::Debian)
            .unwrap();
        assert_eq!(deb.version, "15.2.0");
        assert!(deb.url.contains("packages.debian.org"));
        assert!(
            !hits
                .iter()
                .any(|h| h.url.contains("apt install") || h.url.contains("pacman"))
        );
    }

    #[test]
    fn collapse_filtered_debian_caps_suites_keeps_full_count() {
        let packages = vec![
            pkg("debian_11", "rust-ripgrep", "12.1.1"),
            pkg("debian_12", "rust-ripgrep", "13.0.0"),
            pkg("debian_13", "rust-ripgrep", "14.1.1"),
            pkg("debian_14", "rust-ripgrep", "15.2.0"),
            pkg("debian_unstable", "rust-ripgrep", "15.2.0"),
            pkg("nixpkgs_unstable", "ripgrep", "15.2.0"),
        ];
        let (count, hits) = collapse_hits(
            "ripgrep",
            &packages,
            &[PackageFamily::Debian],
            FILTERED_SUITE_CAP,
        );
        assert_eq!(count, 6);
        assert!(hits.len() <= FILTERED_SUITE_CAP);
        assert!(hits.iter().all(|h| h.family == PackageFamily::Debian));
        assert!(hits.iter().any(|h| h.version == "15.2.0"));
    }

    fn test_client(server: &MockServer) -> RepologyClient {
        crate::install_crypto_provider();
        RepologyClient::with_base_url(reqwest::Client::new(), server.uri())
    }

    #[tokio::test]
    async fn fetch_project_exact_hit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/project/ripgrep"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"repo": "aur", "visiblename": "ripgrep-git", "version": "1"},
                {"repo": "debian_unstable", "visiblename": "rust-ripgrep", "version": "15"}
            ])))
            .mount(&server)
            .await;
        let client = test_client(&server);
        match client.fetch_project("ripgrep").await.unwrap() {
            ProjectLookup::Found { name, packages } => {
                assert_eq!(name, "ripgrep");
                assert_eq!(packages.len(), 2);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_project_404_uses_top_search_hit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/project/rip-grep"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/projects/"))
            .and(query_param("search", "rip-grep"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ripgrep": [{"repo": "aur", "visiblename": "ripgrep-git", "version": "1"}],
                "ripgrep-all": [{"repo": "aur", "visiblename": "ripgrep-all", "version": "1"}]
            })))
            .mount(&server)
            .await;
        let client = test_client(&server);
        match client.fetch_project("rip-grep").await.unwrap() {
            ProjectLookup::Found { name, .. } => assert_eq!(name, "ripgrep"),
            other => panic!("expected top hit ripgrep, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_project_404_empty_search_is_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/project/nope"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/projects/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let client = test_client(&server);
        assert!(matches!(
            client.fetch_project("nope").await.unwrap(),
            ProjectLookup::NotFound
        ));
    }

    #[tokio::test]
    async fn fetch_project_5xx_is_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/project/ripgrep"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = test_client(&server);
        assert!(matches!(
            client.fetch_project("ripgrep").await,
            Err(RepologyError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn fetch_uses_app_user_agent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/project/ripgrep"))
            .and(header("user-agent", crate::APP_USER_AGENT))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"repo": "aur", "visiblename": "ripgrep-git", "version": "1"}
            ])))
            .expect(1)
            .mount(&server)
            .await;
        crate::install_crypto_provider();
        let http = reqwest::Client::builder()
            .user_agent(crate::APP_USER_AGENT)
            .build()
            .unwrap();
        let client = RepologyClient::with_base_url(http, server.uri());
        match client.fetch_project("ripgrep").await.unwrap() {
            ProjectLookup::Found { name, packages } => {
                assert_eq!(name, "ripgrep");
                assert_eq!(packages.len(), 1);
            }
            other => panic!("expected Found, got {other:?}"),
        }
        server.verify().await;
    }

    #[tokio::test]
    async fn two_fetches_are_at_least_one_second_apart() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        let client = test_client(&server);
        let t0 = std::time::Instant::now();
        #[expect(
            clippy::let_underscore_must_use,
            reason = "only the request spacing is under test, not the lookup result"
        )]
        let _ = client.fetch_project("a").await;
        #[expect(
            clippy::let_underscore_must_use,
            reason = "only the request spacing is under test, not the lookup result"
        )]
        let _ = client.fetch_project("b").await;
        assert!(
            t0.elapsed() >= std::time::Duration::from_secs(1),
            "gate must separate request starts by 1s, elapsed {:?}",
            t0.elapsed()
        );
    }
}
