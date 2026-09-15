//! Recap Notes rendering for the system-message suffix.

use crate::ai::recap::RecapNote;

/// Render a Recap Notes section for injection into the system message suffix.
///
/// Invocation-first ordering is the caller's responsibility when concatenating
/// multiple channel logs; `notes` are joined oldest-to-newest within one channel.
pub fn render_recap_section(login: &str, notes: &[RecapNote]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut s = format!("## Recap (#{login})\n");
    s.push_str(
        &notes
            .iter()
            .map(|n| n.body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    );
    s
}
