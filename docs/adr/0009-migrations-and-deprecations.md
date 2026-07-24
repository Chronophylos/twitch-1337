# ADR-0009: Migrations and deprecations procedure

**Status:** Accepted (2026-07-24)
**Relates to:** rolling release (no semver); ADR-0004/0008 (SQLite path examples);
`docs/superpowers/specs/2026-07-24-emote-meanings-sqlite-design.md`

## Context

This repo mixes several “migration” shapes: permanent SQLite `user_version`
upgrades, one-shot config/data importers with `$DATA_DIR` sentinels, and
temporary compat bridges (file renames, dual gitignore, API shims). Without a
shared procedure, cleanup PRs either leave dead bridges forever or delete
schema steps / re-run importers unsafely.

## Decision

### Taxonomy (three classes)

| Class | What it is | Removable? |
|-------|------------|------------|
| **Schema upgrade** | Ordered store format bumps (e.g. SQLite `PRAGMA user_version`, `MIGRATE_n_TO_m`) | **Not** by casual cleanup. Drop old steps only with an explicit **minimum supported version** gate in the same PR (refuse older files with a clear error). |
| **One-shot import** | Runs once per env (config hoist, TOML→SQLite seed). Often uses a sentinel file under `$DATA_DIR` | Importer **code** may go after exit criterion; **sentinel files may remain** as cheap insurance. Do not delete a sentinel while the importer still exists (re-run footgun). Tag the migrator, not the on-disk marker name. |
| **Compat bridge** | Dual paths so old and new coexist briefly (rename-on-open, dual gitignore, deprecated successor APIs) | **Yes** — primary target of cleanup PRs once the exit criterion is met. |

### Marking

**Grep tags** (required on one-shot importers and compat bridges; **not** on normal schema upgrade arms):

```text
DEPRECATED(<slug>)
MIGRATION(<slug>): <what> — remove when <exit criterion>
```

`<slug>` is a stable feature slug (e.g. `emote-meanings`), not a date.

**`#[deprecated]`** — only when there is a **successor API** (same job, better shape). Note should name the replacement. If that item is also scheduled for deletion, put the `DEPRECATED`/`MIGRATION` text in the `note` (and/or an adjacent comment) so grep still works.

Everything else temporary (Python seeds, Just recipes, gitignore dual lines, rename helpers with no public replacement) gets **comment/doc tags only** — not a zombie Rust API.

Schema upgrade SQL and `match user_version` arms are **not** tagged `MIGRATION(...)` disposable unless the PR also raises the minimum supported version.

### Feature docs vs this ADR

This ADR owns **how**. Feature specs/ADRs keep a **short table**: slug, class, tagged items, exit criteria — and link here. They do not restate the whole procedure.

### Cleanup PR contract

Exit criterion in the tag is the contract. A cleanup PR must:

1. Quote the criterion  
2. Show evidence it holds (env check, `rg` for callers/legacy paths, etc.)  
3. Delete tags + code + tests together  

No living “deprecation registry” file. No calendar-only removals.

### Compiler policy

Do **not** enable workspace-wide deny-on-deprecated. `#[deprecated]` is guidance for successor APIs; remaining call sites may `#[allow(deprecated)]` with the same slug tag until removal.

## Consequences

- **+** Cleanup is mechanical (`rg DEPRECATED(` / `MIGRATION(`); schema upgrades stay sacred.  
- **+** Successor APIs and disposable bridges are not confused.  
- **−** Authors must write real exit criteria up front.  
- **−** Min-version schema squashes stay rare and deliberate.
