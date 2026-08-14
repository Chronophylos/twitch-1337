use std::collections::HashSet;

use serde::Deserialize;

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

        repos.sort_by(|a, b| suite_rank(b, family).cmp(&suite_rank(a, family)));
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
