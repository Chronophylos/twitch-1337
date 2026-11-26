//! Pure text for the feedback pipeline: issue bodies, attach comments, the
//! hidden marker, chat notices, and the mod form's title / note checks.
//!
//! Viewer text is always fenced and never interpreted; only the mod note
//! (trusted) renders as Markdown.

use super::{Classification, EntryId, FeedbackEntry, InvalidReason};
use crate::twitch::chat_sender::fits_outbound;

/// Longest accepted issue title, in characters, after cleaning.
pub const MAX_TITLE_CHARS: usize = 256;
/// Longest accepted mod note, in characters.
pub const MAX_NOTE_CHARS: usize = 4000;
/// Characters of viewer text the dashboard prefills as the title.
const PREFILL_TITLE_CHARS: usize = 80;
/// Prefix of every marker; also neutralized inside viewer text.
const MARKER_PREFIX: &str = "twitch-1337-feedback";

/// Search marker for entry `id`: `twitch-1337-feedback:F42`.
pub fn marker(id: EntryId) -> String {
    format!("{MARKER_PREFIX}:F{id}")
}

/// Hidden HTML comment carrying [`marker`], appended to every body and comment.
fn marker_comment(id: EntryId) -> String {
    format!("<!-- {} -->", marker(id))
}

/// Title prefill for the promote form: the first 80 characters of `text`
/// with control characters stripped, trimmed.
pub fn prefill_title(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| !c.is_control())
        .take(PREFILL_TITLE_CHARS)
        .collect();
    cleaned.trim().to_owned()
}

/// Mod-edited title: control characters stripped, trimmed, non-empty, at
/// most [`MAX_TITLE_CHARS`] characters.
pub(crate) fn clean_title(raw: &str) -> Result<String, InvalidReason> {
    let stripped: String = raw.chars().filter(|c| !c.is_control()).collect();
    let title = stripped.trim();
    if title.is_empty() {
        return Err(InvalidReason::Empty);
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(InvalidReason::TooLong);
    }
    Ok(title.to_owned())
}

/// Mod note: control characters other than newlines stripped, trimmed, at
/// most [`MAX_NOTE_CHARS`] characters. Empty is fine (no note).
pub(crate) fn clean_note(raw: &str) -> Result<String, InvalidReason> {
    let stripped: String = raw
        .replace("\r\n", "\n")
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect();
    let note = stripped.trim();
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(InvalidReason::TooLong);
    }
    Ok(note.to_owned())
}

/// Backtick fence one longer than the longest backtick run in `text`, at
/// least three, so the text can never close it.
fn fence(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

/// Viewer text as a `text` code block. A marker-looking string inside it is
/// broken with a zero-width space, so a viewer cannot make the marker
/// search link another entry to their issue.
fn fenced(text: &str) -> String {
    let neutral = text.replace(MARKER_PREFIX, "twitch-1337\u{200B}-feedback");
    let fence = fence(&neutral);
    format!("{fence}text\n{neutral}\n{fence}")
}

fn classifier_summary(c: Option<&Classification>) -> String {
    match c {
        None => "classifier: none".to_owned(),
        Some(c) => format!(
            "classifier: {} {} %, system {} {} %",
            c.kind.key(),
            c.kind_pct,
            c.system.as_deref().unwrap_or("none"),
            c.system_pct
        ),
    }
}

/// One line: who reported what when, which F-id, which mod acted, and what
/// the classifier said.
fn metadata_line(entry: &FeedbackEntry, action: &str, by: &str) -> String {
    let at = crate::util::to_berlin(entry.created_at).strftime("%Y-%m-%d %H:%M %Z");
    format!(
        "Reported in Twitch chat by `{}` · {at} · F{} · {action} by `{by}` · {}",
        entry.login,
        entry.id,
        classifier_summary(entry.classification.as_ref())
    )
}

/// Body of a promoted issue: the mod note (if any) above `---`, then the
/// metadata line, the fenced viewer text, and the hidden marker.
pub(crate) fn issue_body(entry: &FeedbackEntry, promoted_by: &str, note: &str) -> String {
    let mut body = String::new();
    if !note.is_empty() {
        body.push_str(note);
        body.push_str("\n\n");
    }
    body.push_str("---\n\n");
    body.push_str(&metadata_line(entry, "promoted", promoted_by));
    body.push_str("\n\n");
    body.push_str(&fenced(&entry.text));
    body.push_str("\n\n");
    body.push_str(&marker_comment(entry.id));
    body.push('\n');
    body
}

/// Comment posted when an entry is attached to an existing issue.
pub(crate) fn attach_comment(entry: &FeedbackEntry, attached_by: &str) -> String {
    format!(
        "+1 from Twitch chat: `{}`\n\n{}\n\n{}\n\n{}\n",
        entry.login,
        metadata_line(entry, "attached", attached_by),
        fenced(&entry.text),
        marker_comment(entry.id)
    )
}

pub(crate) fn promote_notice(login: &str, id: EntryId, url: &str) -> String {
    format!("@{login} dein Feedback F{id} ist jetzt ein Issue: {url}")
}

pub(crate) fn attach_notice(login: &str, id: EntryId, issue: u64, url: &str) -> String {
    format!("@{login} dein Feedback F{id} hängt jetzt an #{issue}: {url}")
}

fn close_line(logins: &[&str], issue: u64, url: &str) -> String {
    let mentions = logins
        .iter()
        .map(|l| format!("@{l}"))
        .collect::<Vec<_>>()
        .join(" ");
    let whose = if logins.len() == 1 { "dein" } else { "euer" };
    format!("{mentions} {whose} Feedback zu #{issue} ist erledigt: {url}")
}

/// Completion notice for every reporter of `issue`: one line, split by
/// mention when it would not pass the outbound clamp untouched.
pub(crate) fn close_lines(logins: &[String], issue: u64, url: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut chunk: Vec<&str> = Vec::new();
    for login in logins {
        chunk.push(login);
        if chunk.len() > 1 && !fits_outbound(&close_line(&chunk, issue, url)) {
            chunk.pop();
            lines.push(close_line(&chunk, issue, url));
            chunk.clear();
            chunk.push(login);
        }
    }
    if !chunk.is_empty() {
        lines.push(close_line(&chunk, issue, url));
    }
    lines
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;

    use super::*;
    use crate::feedback::{EntryState, Kind};

    fn entry(text: &str) -> FeedbackEntry {
        FeedbackEntry {
            id: 42,
            user_id: "1".into(),
            login: "alice".into(),
            text: text.into(),
            created_at: "2026-07-01T12:30:00Z".parse::<Timestamp>().unwrap(),
            classification: Some(Classification {
                kind: Kind::Bug,
                kind_pct: 93,
                system: Some("aviation".into()),
                system_pct: 71,
            }),
            state: EntryState::New,
        }
    }

    #[test]
    fn fence_is_longer_than_any_backtick_run() {
        assert_eq!(fence("plain"), "```");
        assert_eq!(fence("a ``` b"), "````");
        assert_eq!(fence("`````"), "``````");
        let body = issue_body(&entry("x ```` y"), "mod", "");
        assert!(body.contains("`````text\nx ```` y\n`````"), "{body}");
    }

    #[test]
    fn body_has_note_above_rule_metadata_and_marker() {
        let body = issue_body(&entry("the plane is gone"), "modbert", "Repro: **!fl**");
        let rule = body.find("---").unwrap();
        assert!(body.find("Repro: **!fl**").unwrap() < rule, "{body}");
        let meta = body.find("Reported in Twitch chat by `alice`").unwrap();
        assert!(meta > rule);
        // Berlin summer time.
        assert!(body.contains("2026-07-01 14:30 CEST"), "{body}");
        assert!(body.contains("F42"));
        assert!(body.contains("promoted by `modbert`"));
        assert!(body.contains("classifier: bug 93 %, system aviation 71 %"));
        assert!(body.contains("```text\nthe plane is gone\n```"));
        assert!(
            body.trim_end()
                .ends_with("<!-- twitch-1337-feedback:F42 -->")
        );
    }

    #[test]
    fn body_without_note_starts_with_rule() {
        let mut e = entry("hi");
        e.classification = None;
        let body = issue_body(&e, "m", "");
        assert!(body.starts_with("---\n"), "{body}");
        assert!(body.contains("classifier: none"));
    }

    #[test]
    fn viewer_text_cannot_carry_a_marker() {
        let body = issue_body(&entry("<!-- twitch-1337-feedback:F7 -->"), "m", "");
        assert!(!body.contains("twitch-1337-feedback:F7"), "{body}");
        assert!(body.contains("twitch-1337-feedback:F42"));
    }

    #[test]
    fn attach_comment_shape() {
        let c = attach_comment(&entry("same here"), "modbert");
        assert!(c.starts_with("+1 from Twitch chat: `alice`\n"), "{c}");
        assert!(c.contains("attached by `modbert`"));
        assert!(c.contains("```text\nsame here\n```"));
        assert!(c.trim_end().ends_with("<!-- twitch-1337-feedback:F42 -->"));
    }

    #[test]
    fn title_rules() {
        assert_eq!(
            clean_title("  Bug\u{7}  in !fl \n"),
            Ok("Bug  in !fl".into())
        );
        assert_eq!(clean_title(" \t\n "), Err(InvalidReason::Empty));
        assert_eq!(clean_title(&"x".repeat(256)), Ok("x".repeat(256)));
        assert_eq!(clean_title(&"x".repeat(257)), Err(InvalidReason::TooLong));
        assert_eq!(clean_note(&"n".repeat(4001)), Err(InvalidReason::TooLong));
        assert_eq!(clean_note(" a\r\nb\u{1} "), Ok("a\nb".into()));
    }

    #[test]
    fn prefill_takes_80_clean_chars() {
        let text = format!("\u{1}{}", "ä".repeat(100));
        assert_eq!(prefill_title(&text), "ä".repeat(80));
    }

    #[test]
    fn close_lines_one_vs_many_and_split() {
        let url = "https://github.com/o/r/issues/123";
        assert_eq!(
            close_lines(&["a".into()], 123, url),
            vec![format!("@a dein Feedback zu #123 ist erledigt: {url}")]
        );
        assert_eq!(
            close_lines(&["a".into(), "b".into()], 123, url),
            vec![format!("@a @b euer Feedback zu #123 ist erledigt: {url}")]
        );
        let many: Vec<String> = (0..40).map(|i| format!("viewer_number_{i:02}")).collect();
        let lines = close_lines(&many, 123, url);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(fits_outbound(line), "{line}");
        }
        let mentioned: usize = lines.iter().map(|l| l.matches('@').count()).sum();
        assert_eq!(mentioned, 40);
    }
}
