# ADR-0016: Chat feedback reaches GitHub only through a Mod review gate

**Status:** Accepted (2026-11-26)
**Relates to:** issue #775, ADR-0007 (ring-only TLS), ADR-0010 (Decision
profiles), ADR-0015 (lifecycle)

## Context

`!fb` appended lines to `$DATA_DIR/feedback.txt`, which nobody read in time.
The goal is a pipeline from chat feedback to GitHub issues.

The repository is public, and coding agents pick up issues labelled
`status:ready-for-agent`. Issue text written by viewers is therefore public,
attributed to whatever account files it, and potential instructions for an
agent. Anything in chat can reach an issue body: spam, slurs, personal data,
prompt injection.

Options considered:

- `!fb` files an issue directly. The only control is the cooldown.
- A model decides and files on its own. Same exposure, plus false positives.
- A weekly digest issue. Low noise, but items cannot be triaged one by one.
- An inbox reviewed by a Mod on the dashboard, who promotes or attaches each
  entry.

## Decision

1. **Review gate.** `!fb` stores a Feedback Entry in the Feedback Inbox
   (`$DATA_DIR/feedback.ron`). Only a Mod or the Owner, on `/feedback`, can
   turn an entry into an issue (Promote) or a `+1` comment on an existing
   issue (Attach). Viewers on the allowlist cannot see the inbox.
2. **Viewer text is quoted, never authored.** It is always inside a code
   fence longer than any backtick run it contains, and the Mod cannot edit it.
   The Mod writes the title and an optional trusted note. Issues are labelled
   `status:needs-triage` and `source:chat`; nothing from this path is ever
   `ready-for-agent` without a human relabelling it.
3. **Identity: a GitHub App** (`twitch-1337[bot]`) with Issues read/write and
   Metadata read, installed on this repository only. The App JWT (RS256) is
   signed with `ring` and the PEM is decoded with `rustls-pki-types`, both
   already in the tree. `jsonwebtoken` was rejected: its backends are aws-lc
   (banned, ADR-0007) or the `rsa` crate (RUSTSEC-2023-0071).
4. **Classification only.** An optional Decision profile (`feedback.profile`)
   picks a kind (bug, feature, task, noise) and a `system:` label for each
   entry. It writes no text. Noise at or above `feedback.noise_floor` is
   dismissed automatically and can be restored; dismissed entries are deleted
   after 30 days.
5. **Idempotency by marker.** Every issue body and comment carries
   `<!-- twitch-1337-feedback:F<id> -->`. A per-entry state machine blocks
   concurrent attempts; an ambiguous failure (timeout, 5xx) searches for the
   marker before trying again.
6. **Polling, not webhooks.** Closed issues are polled every
   `feedback.close_poll_secs` with a `since` cursor. Only `completed` closes are
   announced in chat, and only when found within 24 hours. A webhook would add
   the dashboard's first unauthenticated POST route for a few minutes of
   latency.

## Consequences

- A Mod has to clear the inbox; auto-dismissed noise keeps the queue short.
- A leaked App key can create issues as `twitch-1337[bot]`, not as the
  Owner; revoke it in the App settings.
- Without `[github]`, the inbox and classifier still work and Promote/Attach
  are disabled.
- Losing `feedback.ron` loses entry→issue links; the markers make them
  recoverable by search.
