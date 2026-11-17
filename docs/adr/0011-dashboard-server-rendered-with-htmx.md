# ADR-0011: Dashboard stays server-rendered, htmx for partial updates

**Status:** Accepted (2026-11-16)

## Context

The 2026-11 dashboard review found that htmx 2.0.4 (50 KB) was bundled but used
by exactly two delete buttons, without handling for error responses or expired
sessions. Several open problems (dead filter chips, stale flights page,
full-page reloads on toggles, a logs tail) need some kind of partial update.

Options considered:

- A SPA framework (Svelte, React) on a JSON API. Needs a node build step next
  to cargo, duplicates validation on both sides, and doesn't fit a
  `FROM scratch` single-binary image used by a handful of people.
- Drop htmx and write the few dynamic bits as plain `fetch` in `app.js`. Small
  today, but every new dynamic control would grow its own fetch-and-swap code.
- Keep askama as the only renderer and use htmx on purpose.

## Decision

askama templates stay the single source of HTML. htmx is the way a page
updates part of itself (swaps, polling, inline actions). Controls may rely on
JS; a no-JS fallback is not required.

Every htmx request shares one contract:

- An expired session answers an `HX-Request` with `HX-Redirect: /login?next=…`
  instead of a 303, so the login page never lands inside a swap target.
- A 4xx/5xx response shows its plain-text body in one global toast region in
  `base.html`; controls don't wire their own error targets.

## Consequences

- No JS build step and no JSON API for the dashboard.
- Handlers that serve htmx return fragments, so templates for swappable parts
  live as partials.
- Every response carries a strict CSP (`script-src 'self'; style-src 'self'`,
  set in `crates/web/src/lib.rs`), so templates hold no inline `<script>`,
  `<style>`, `style=""` or `on*=` handlers; behaviour lives in `app.js`, keyed
  off `data-*` attributes. htmx runs with `includeIndicatorStyles: false` and
  `allowEval: false`, so trigger filters, `hx-on` and `js:` values are out.
