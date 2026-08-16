//! Hand-rolled YAML-ish frontmatter parser. Schema is fixed:
//! `updated_at` (RFC3339, required), `username` (optional), `display_name`
//! (optional), `created_by` (optional), `pronouns` (optional), `links`
//! (optional indented `platform: url` block).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::ai::memory::types::Frontmatter;

#[derive(Debug, Error)]
pub enum FrontmatterError {
    #[error("missing or malformed frontmatter delimiters")]
    Delimiters,
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("invalid timestamp: {0}")]
    BadTimestamp(String),
}

pub fn parse(raw: &str) -> Result<(Frontmatter, String), FrontmatterError> {
    let body_start;
    let yaml = if let Some(rest) = raw.strip_prefix("---\n") {
        let end = rest.find("\n---\n").ok_or(FrontmatterError::Delimiters)?;
        body_start = "---\n".len() + end + "\n---\n".len();
        &rest[..end]
    } else {
        return Err(FrontmatterError::Delimiters);
    };

    let mut updated_at = None;
    let mut username = None;
    let mut display_name = None;
    let mut created_by = None;
    let mut pronouns = None;
    let mut links = BTreeMap::new();
    // `links:` opens an indented block; the first unindented line closes it.
    let mut in_links = false;
    for line in yaml.lines() {
        let indented = line.starts_with([' ', '\t']);
        if in_links && indented {
            if let Some((k, v)) = line.split_once(':') {
                let (k, v) = (k.trim(), v.trim());
                if !k.is_empty() && !v.is_empty() {
                    links.insert(k.to_string(), v.to_string());
                }
            }
            continue;
        }
        in_links = false;
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "links" => in_links = true,
            "updated_at" => {
                let dt = DateTime::parse_from_rfc3339(v)
                    .map_err(|e| FrontmatterError::BadTimestamp(e.to_string()))?
                    .with_timezone(&Utc);
                updated_at = Some(dt);
            }
            "username" if !v.is_empty() => username = Some(v.to_string()),
            "display_name" if !v.is_empty() => display_name = Some(v.to_string()),
            "created_by" if !v.is_empty() => created_by = Some(v.to_string()),
            "pronouns" if !v.is_empty() => pronouns = Some(v.to_string()),
            _ => {}
        }
    }

    Ok((
        Frontmatter {
            updated_at: updated_at.ok_or(FrontmatterError::MissingField("updated_at"))?,
            username,
            display_name,
            created_by,
            pronouns,
            links,
        },
        raw[body_start..].to_string(),
    ))
}

pub fn emit(fm: &Frontmatter, body: &str) -> String {
    let mut s = String::with_capacity(body.len() + 128);
    s.push_str("---\n");
    s.push_str(&format!(
        "updated_at: {}\n",
        fm.updated_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    ));
    // `Some("")` emits a bare `key:` placeholder (no trailing space) so an
    // unknown-but-expected field is visible for later backfill; `parse` reads
    // it straight back as `None`.
    let mut kv = |k: &str, v: Option<&String>| {
        if let Some(v) = v {
            s.push_str(k);
            if v.is_empty() {
                s.push_str(":\n");
            } else {
                s.push_str(&format!(": {v}\n"));
            }
        }
    };
    kv("username", fm.username.as_ref());
    kv("display_name", fm.display_name.as_ref());
    kv("created_by", fm.created_by.as_ref());
    kv("pronouns", fm.pronouns.as_ref());
    if !fm.links.is_empty() {
        s.push_str("links:\n");
        for (k, v) in &fm.links {
            s.push_str(&format!("  {k}: {v}\n"));
        }
    }
    s.push_str("---\n");
    s.push_str(body);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::memory::types::Frontmatter;
    use chrono::TimeZone as _;

    fn ts() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .with_ymd_and_hms(2026, 4, 28, 18, 42, 0)
            .unwrap()
    }

    #[test]
    fn split_returns_frontmatter_and_body() {
        let raw = "---\nupdated_at: 2026-04-28T18:42:00Z\nusername: alicepleb\ndisplay_name: AlicePleb\n---\nhello\nworld\n";
        let (fm, body) = parse(raw).unwrap();
        assert_eq!(fm.updated_at, ts());
        assert_eq!(fm.username.as_deref(), Some("alicepleb"));
        assert_eq!(fm.display_name.as_deref(), Some("AlicePleb"));
        assert_eq!(body, "hello\nworld\n");
    }

    #[test]
    fn missing_frontmatter_errors() {
        assert!(parse("hello\n").is_err());
        assert!(parse("---\nno-end\n").is_err());
    }

    #[test]
    fn emit_round_trips() {
        let fm = Frontmatter {
            updated_at: ts(),
            username: Some("alice".into()),
            display_name: Some("Alice".into()),
            created_by: None,
            pronouns: None,
            links: BTreeMap::new(),
        };
        let raw = emit(&fm, "body line\n");
        let (fm2, body2) = parse(&raw).unwrap();
        assert_eq!(fm, fm2);
        assert_eq!(body2, "body line\n");
    }

    #[test]
    fn links_block_round_trips() {
        let fm = Frontmatter {
            updated_at: ts(),
            username: Some("alice".into()),
            display_name: Some("Alice".into()),
            created_by: None,
            pronouns: Some("They/Them".into()),
            links: BTreeMap::from([
                ("steam".into(), "https://steamcommunity.com/id/alice".into()),
                ("github".into(), "https://github.com/alice".into()),
            ]),
        };
        let raw = emit(&fm, "body\n");
        // URLs contain colons; the block must survive the naive line splitter.
        assert!(
            raw.contains("links:\n  github: https://github.com/alice\n  steam: https://steamcommunity.com/id/alice\n"),
            "{raw}"
        );
        let (fm2, body2) = parse(&raw).unwrap();
        assert_eq!(fm, fm2);
        assert_eq!(body2, "body\n");
    }

    #[test]
    fn links_block_ends_at_the_next_unindented_key() {
        let raw = "---\nupdated_at: 2026-04-28T18:42:00Z\nlinks:\n  steam: https://s.example/1\ndisplay_name: Alice\n---\nbody\n";
        let (fm, _) = parse(raw).unwrap();
        assert_eq!(fm.links.len(), 1);
        assert_eq!(fm.display_name.as_deref(), Some("Alice"));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let raw = "---\nupdated_at: 2026-04-28T18:42:00Z\nfoo: bar\n---\nbody\n";
        let (fm, body) = parse(raw).unwrap();
        assert_eq!(fm.updated_at, ts());
        assert_eq!(body, "body\n");
    }
}
