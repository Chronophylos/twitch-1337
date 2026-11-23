# Triage Labels

The skills speak in terms of five canonical triage roles. This file maps those roles to the actual label strings used in this repo's issue tracker.

This repo uses a `prefix:value` label taxonomy (`status:`, `system:`, `topic:`, `deps:`, plus `type:` on PRs only), so the triage roles live under the existing `status:` prefix. An issue's category is its GitHub issue type (Bug, Feature, Task, Security), set with `gh issue edit <n> --type <Type>` — see `CONTRIBUTING.md` § Labels for the mapping.

| Canonical role    | Label in our tracker     | Meaning                                  |
| ----------------- | ------------------------ | ---------------------------------------- |
| `needs-triage`    | `status:needs-triage`    | Maintainer needs to evaluate this issue  |
| `needs-info`      | `status:needs-info`      | Waiting on reporter for more information |
| `ready-for-agent` | `status:ready-for-agent` | Fully specified, ready for an AFK agent  |
| `ready-for-human` | `status:ready-for-human` | Requires human implementation            |
| `wontfix`         | `status:wontfix`         | Will not be actioned                     |

When a skill mentions a role (e.g. "apply the AFK-ready triage label"), use the corresponding label string from this table.

Edit the right-hand column to match whatever vocabulary you actually use.
