//! FILE fence rendering: sentinel-matched blocks the model can quote back.

use rand::Rng as _;

use crate::ai::memory::types::Frontmatter;

/// Generate a 16-hex-char nonce for one prompt build.
pub fn fresh_nonce() -> String {
    let mut bytes = [0u8; 8];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const FENCE_OPEN: &str = "<<<FILE";
const FENCE_CLOSE: &str = "<<<ENDFILE";

/// Identifies what a fenced inject block represents. Renders into the FILE
/// header attrs so the model can map a block to its subject without parsing
/// a path. Path-style addressing only re-appears in the `write_file` tool's
/// `path` argument, where it's the canonical way to address a write target.
#[derive(Debug, Clone)]
pub enum FenceLabel<'a> {
    Soul,
    Lore,
    User {
        id: &'a str,
        login: Option<&'a str>,
        display_name: Option<&'a str>,
        /// Self-declared, from the pronouns service. Rendered as an attr so
        /// the model can't mistake it for something it may rewrite.
        pronouns: Option<&'a str>,
    },
    Scratch {
        slug: &'a str,
    },
    Transcript {
        date: &'a str,
    },
    /// Recap Notes for one channel: chat-derived, rendered in the user slot.
    Recap {
        channel: &'a str,
    },
}

/// Body as the model should see it: the `links` frontmatter rendered as a
/// leading line, then the prose. Links are dashboard-managed and read-only to
/// the model — it never writes this line back, and a body write can't forge
/// one because `check_body` runs on the prose alone.
pub fn body_with_links(fm: &Frontmatter, body: &str) -> String {
    if fm.links.is_empty() {
        return body.to_string();
    }
    let links = fm
        .links
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join(" | ");
    format!("Links — {links}\n\n{body}")
}

pub fn fence_block(label: FenceLabel<'_>, nonce: &str, body: &str) -> String {
    let safe = scrub_for_inject(body);
    let attrs = render_label_attrs(&label);
    format!("<<<FILE {attrs} nonce={nonce}>>>\n{safe}\n<<<ENDFILE nonce={nonce}>>>")
}

pub(super) fn render_label_attrs(label: &FenceLabel<'_>) -> String {
    match label {
        FenceLabel::Soul => "kind=soul".into(),
        FenceLabel::Lore => "kind=lore".into(),
        FenceLabel::Scratch { slug } => format!("kind=scratch slug={slug}"),
        FenceLabel::Transcript { date } => format!("kind=transcript date={date}"),
        FenceLabel::Recap { channel } => {
            format!("kind=recap channel={}", sanitize_attr_value(channel))
        }
        FenceLabel::User {
            id,
            login,
            display_name,
            pronouns,
        } => {
            let mut s = format!("kind=user id={id}");
            // Unquoted attribute: drop whitespace too, so a login can't add attributes.
            if let Some(l) = login.filter(|l| !l.is_empty()) {
                let l: String = sanitize_attr_value(l)
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                s.push_str(&format!(" login={l}"));
            }
            if let Some(n) = display_name.filter(|n| !n.is_empty()) {
                s.push_str(&format!(" name=\"{}\"", sanitize_attr_value(n)));
            }
            if let Some(p) = pronouns.filter(|p| !p.is_empty()) {
                s.push_str(&format!(" pronouns=\"{}\"", sanitize_attr_value(p)));
            }
            s
        }
    }
}

/// Strip characters that would break the FILE header (quotes, newlines, `>`).
/// Display names are already control-stripped at write time, but we apply
/// the same hardening at render time to keep the marker syntactically clean.
pub(super) fn sanitize_attr_value(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '>' && *c != '<')
        .collect()
}

/// If a body contains either fence sentinel, replace it wholesale.
pub fn scrub_for_inject(body: &str) -> String {
    if body.contains(FENCE_OPEN) || body.contains(FENCE_CLOSE) {
        tracing::error!("memory body contained fence sentinel at inject time, scrubbed");
        return "<corrupt: rejected>".to_string();
    }
    body.to_string()
}

/// Defuse fence sentinels inside untrusted text (chat transcripts) without
/// dropping it. Every `<<<` becomes `<<‹`, so no `<<<FILE` / `<<<ENDFILE`
/// can survive: `str::replace` scans left to right without overlap, so no
/// three `<` in a row are left behind. Unlike [`scrub_for_inject`], one
/// viewer's line cannot blank everyone else's.
pub fn neutralize_sentinels(text: &str) -> String {
    text.replace("<<<", "<<‹")
}
