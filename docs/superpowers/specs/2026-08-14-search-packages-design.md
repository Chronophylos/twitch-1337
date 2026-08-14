# Aurora `search_packages` (Repology)

Status: approved (issue #428 agent brief)
Date: 2026-08-14
Branch: `feature/search-packages`

## Goal

Aurora gets one chat-turn tool, `search_packages`, backed by Repology, so she
looks up distro/repo packages instead of inventing them. Grok is unchanged.

## Non-goals

- deps.dev, npm, crates, PyPI, Maven, Go modules
- Docker / OCI image search
- Install command generation
- Grok
- Dashboard toggle
- Families beyond AUR, winget, Debian, Ubuntu
- Caching beyond a single Turn
- Repology database dump / mirroring
- Raising the Repology rate above 1 request/second
- Per-service User-Agent strings

## User-Agent

Expand `APP_USER_AGENT` to include the source repo URL. Example shape:

`twitch-1337-core/0.0.0 (+https://github.com/Chronophylos/twitch-1337)`

That string is the process-wide identity. Every existing
`user_agent(APP_USER_AGENT)` call site picks it up. Repology uses the same
string — no second User-Agent.

## Rate limit

Process-wide maximum of **1 request per second** to Repology, on that client
only (mutex + interval). Exact-then-search and concurrent Turns share the
gate so two GETs never start less than 1s apart. Other HTTP clients are
unchanged.

Live API only. No dump. Chat volume cannot approach bulk (>1000/day); do not
add a daily counter.

## Tool

Aurora-only, next to `search_emotes` / `doener_index` / `draw_tarot`.

Arguments: `query` (required string), `ecosystem` (optional string).

### Ecosystem allowlist

| Input (case-insensitive) | Families |
|---|---|
| omitted / empty | AUR, winget, Debian, Ubuntu |
| `aur`, `arch` | AUR |
| `winget` | winget |
| `debian` | Debian |
| `ubuntu` | Ubuntu |
| `apt` | Debian and Ubuntu |
| anything else (`npm`, `docker`, `pypi`, `cargo`, …) | unsupported-ecosystem tool error, **no HTTP** |

### Exact then search

1. `GET /api/v1/project/{query}`
2. On **404 only**, `GET /api/v1/projects/?search={query}` and take the **first**
   project in the JSON object (never a search menu).
3. Empty search → not-found tool result.
4. Error / timeout / 5xx → short tool error (`repology unavailable`). The Turn
   still finishes.

The search response already contains that project's packages — do not fetch
the project a third time.

### Collapse

`ecosystem_count` is the number of **unique `repo` values** on the project
(all repos, not just the four families).

A Repology `repo` maps to a v1 family as:

- `aur` → AUR
- `winget` → winget
- `repo` starts with `debian` (not raspbian) → Debian
- `repo` starts with `ubuntu` → Ubuntu
- anything else → ignored for hits

**Unfiltered** (or multi-family `apt`): at most **one row per requested
family**. Prefer the highest-ranked suite in that family.

**Filtered** (single family, or `apt`'s two): still capped at **3 suites per
family**, not every `debian_*` / `ubuntu_*` codename.

Suite rank:

- Debian: `debian_unstable` first, then `debian_<N>` by N descending
- Ubuntu: `ubuntu_<YY>_<MM>` by (YY, MM) descending
- AUR / winget: the single repo id

Each hit: `{ family, name, version, url }`.

- `name`: `visiblename`, else `srcname`, else first `binnames` entry
- `version`: Repology `version`
- `url`: AUR → `https://aur.archlinux.org/packages/{name}`; Debian →
  `https://packages.debian.org/{name}`; Ubuntu →
  `https://packages.ubuntu.com/{name}`; winget →
  `https://repology.org/project/{project}/information`

No install commands in the result.

Tool JSON:

- success: `{ "ecosystem_count": <u32>, "hits": [ ... ] }`
- not-found: `{ "error": "not_found" }`
- unsupported: `{ "error": "unsupported_ecosystem" }`
- upstream fail: `{ "error": "repology unavailable" }`

## Prompt

Aurora system appendix (same pattern as `search_emotes`): when the question is
whether something is packaged or which version lives in a distro/repo, call
this tool; do not invent packages. Do not mention 404-then-search or the rate
limit. Grok does not get the appendix.

## Tests

- Collapse: in-module fixtures (ripgrep-shaped debian/ubuntu/aur rows)
- HTTP: wiremock for exact, 404→search, empty search, 5xx, timeout, User-Agent,
  1 req/s gate
- Tool: unsupported ecosystem makes zero requests
- Integration: Aurora registers `search_packages`, Grok does not; Aurora
  system message contains the appendix and does not describe fallback
