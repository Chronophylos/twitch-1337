# Contributing

Thanks for contributing to twitch-1337. This file covers repo conventions that aren't obvious from the code. For build/test/CI details see [AGENTS.md](AGENTS.md).

## Pull requests

PRs are welcome, from forks or otherwise. For larger changes, opening an issue first to agree on the approach saves rework. Bug reports and feature requests go in issues; a PR description is not a substitute for one.

1. Fork (or branch from `main`), commit, push.
2. Open a PR against `main` with a Conventional Commit title (squash merge uses it as the commit subject), a **Summary**, and a **Test plan**.
3. Workflow runs on PRs from outside contributors wait for maintainer approval before they start. Fork runs get a read-only token and no repository secrets.
4. All required status checks must pass (see AGENTS.md § CI & branch policy). The `strict` policy means the branch must be up to date with `main`; rebase or use **Update branch** when it falls behind.
5. The maintainer merges with squash.

`main` is protected by a repository ruleset that applies to everyone, including admins: no direct pushes, no force-push or deletion, linear history, squash-only merges, and all review conversations resolved before merge. No approving review is required.

## Labels

Issues are classified by a GitHub **issue type**, PRs by a `type:` label. Both carry `system:` labels when applicable; `topic:`, `deps:`, and `status:` stack on top as needed.

### Issue types — what kind of issue

| Type | Use when |
|---|---|
| Bug | Functional defect — something is broken |
| Feature | New feature or capability |
| Security | Security vulnerability or hardening (supersedes Bug) |
| Task | Refactor, docs, debt, or any other work; add `topic:refactor`, `topic:docs`, or `topic:debt` to say which |

Pick exactly one type per issue (`gh issue edit <n> --type Bug`). Issues carry no `type:` label.

### `type:` — what kind of change (PRs only)

| Label | Use when |
|---|---|
| `type:bug` | Functional defect — something is broken |
| `type:feat` | New feature or capability |
| `type:refactor` | Structural refactor or code-quality cleanup (no user-visible change) |
| `type:security` | Security vulnerability or hardening (supersedes `type:bug`) |
| `type:docs` | Docs-only change |
| `type:debt` | Accumulated technical debt — cleanup, coverage gaps, sunset |

Pick exactly one `type:` per PR. `type:security` takes precedence over `type:bug` when both apply.

### `system:` — which subsystem

| Label | Scope |
|---|---|
| `system:ai` | LLM, `!ai` command, memory, prefill |
| `system:aviation` | ADS-B, flight tracker, `!up`/`!fl`/`!track` |
| `system:ping` | Ping subsystem — `!p`, ping templates, render/validate |
| `system:tracker` | 1337 tracker + leaderboard (`!lb`) + hall of shame (`!sb`, `!pw`) |
| `system:schedule` | Scheduled messages + hot-reload |
| `system:irc` | IRC client core, broadcast channel, latency monitor |
| `system:config` | `config.toml` schema, loader, file watcher |

Stack multiple when a change crosses subsystems.

### `topic:` — cross-cutting concern

| Label | Use when |
|---|---|
| `topic:concurrency` | Race conditions, locking, async correctness |
| `topic:reliability` | Error handling, fallbacks, timeouts, retries |
| `topic:perf` | Performance, latency, throughput |
| `topic:no-deploy` | CI/workflow-only; merge does not rebuild or redeploy the bot |
| `topic:refactor` | Issue (type Task): structural refactor or code-quality cleanup |
| `topic:docs` | Issue (type Task): docs-only change |
| `topic:debt` | Issue (type Task): accumulated technical debt — cleanup, coverage gaps, sunset |

### `deps:` — Dependabot buckets

| Label | Ecosystem |
|---|---|
| `deps:rust` | Cargo dependency update |
| `deps:actions` | GitHub Actions update |
| `deps:docker` | Docker base image update |

Applied automatically by Dependabot per `.github/dependabot.yml`.

**Dependabot merge guidance:** `deps:actions` PRs (labeled `topic:no-deploy`) update CI workflows only — merge them in batches when convenient; path filters prevent a Docker rebuild or homelab redeploy. `deps:rust` and `deps:docker` PRs change the binary or base image and **do** trigger a rolling release on merge to `main`.

### `status:` — triage state

| Label | Meaning |
|---|---|
| `status:needs-triage` | Maintainer needs to evaluate this issue |
| `status:needs-info` | Waiting on the reporter for more information |
| `status:ready-for-agent` | Fully specified, ready for an AFK agent |
| `status:ready-for-human` | Requires human implementation |
| `status:wontfix` | Intentionally not pursuing |
| `status:duplicate` | Duplicate of another issue or PR |

Contributors don't need to set labels; the maintainer applies them during triage.

## Commit messages

Conventional Commits. Scope is optional but helpful — prefer the subsystem (`ping`, `schedule`, `aviation`, `ai`, `config`, `ci`, etc.).

```
feat(ping): add !p pause command
fix(schedule): reconcile hot-reload on content changes
refactor: split main.rs into handler modules
```

Body explains the *why* when it isn't obvious from the diff.

### AI agents (Cursor, Claude Code, etc.)

Agents should follow [AGENTS.md § Agent workflow](AGENTS.md#agent-workflow): keep the conventional `type(scope):` prefix, but write the **subject** in unhinged gen-z slang. The **body** stays normal, professional prose — stupid subject, serious explanation. Example:

```
fix(chat): bestie the websocket literally ate shit and died 💀

The handler assumed every inbound message included a user ID. Anonymous
viewer messages omitted the field and caused a panic. Guard the optional
field and skip processing when absent.
```

Human contributors can use plain subjects; the gen-z bit is for agents only.

## GitHub Actions dependencies

Reference actions by a specific version tag, without an inline commit SHA or
version comment. `.github/workflows/actions.lock` records the exact commits,
transitive dependencies, and source-repository identities for every workflow.

After adding a workflow or changing any `uses:` reference or action definition,
regenerate and verify the lockfile from the repository root:

```bash
gh extension install github/gh-actions-lock --pin v0.5.1
gh actions-lock
gh actions-lock --verify
```

Never edit `actions.lock` by hand. Include its regenerated changes in the same
PR as the workflow changes. Review changed commits as dependency code changes;
investigate any changed `owner_id` or `repo_id`, because that means the
source-repository identity changed. Dependabot updates both action tags and
their matching lockfile entries.

Pull requests changing workflows, the lockfile, or an `action.yml` /
`action.yaml` run the read-only `actions lockfile` verification check. It does
not commit updates automatically.

## Before committing

Run the CI gate locally before pushing (CI rejects anything that fails):

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace   # `cargo test --workspace` if nextest is missing
```

`cargo deny check` runs in CI; run it locally when you touch dependencies.
