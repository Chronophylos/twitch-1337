# OCI Image Labels Design

**Date:** 2026-07-18
**Status:** Approved, ready for implementation plan

## Context

The final `FROM scratch` stage already carries a single static
`org.opencontainers.image.description` LABEL with a long feature-list
string. CI (`docker.yml`) passes `docker/metadata-action` labels into
`docker/build-push-action`, but that path has not been reliably
surfacing a description on the GHCR package page. Local/`just build`
images only get whatever is baked into the Dockerfile.

The GitHub repo description is the preferred human-facing blurb:

> Opinionated personal Rust Twitch bot with a mind of its own — in a
> tiny static binary.

Rolling release identity remains `BUILD_NUM` + `GIT_SHA` (see
[rolling-release design](./2026-05-26-rolling-release-design.md)).
Those values already exist as Docker build-args; they should also feed
OCI `version` and `revision`.

## Goals

- Bake the full OpenContainers annotation set into the final image stage
  so every build (CI and local) carries them.
- Use the repo blurb as `org.opencontainers.image.description`.
- Map existing build provenance into OCI fields: `revision` ← `GIT_SHA`,
  `version` ← `BUILD_NUM`.
- Pass a UTC `CREATED` timestamp from CI (and local recipes when easy)
  into `org.opencontainers.image.created`.

## Non-Goals

- Changing the GitHub repository or GHCR package “About” text outside
  image labels (those UIs are separate from OCI annotations).
- Replacing `docker/metadata-action` for tags; tags stay as today
  (`latest`, `bN`).
- Adding non-OCI / custom vendor labels beyond the standard keys below.
- SemVer image versions — `version` stays the rolling `BUILD_NUM`
  (`bN` in CI, `dev` locally).

## Approach

**Dockerfile as source of truth** for the full label set on the final
`scratch` stage. Dynamic fields use build-args (existing `GIT_SHA` /
`BUILD_NUM`, new `CREATED`). CI continues to pass
`labels: ${{ steps.meta.outputs.labels }}`; overlapping keys from
metadata-action are acceptable when they agree (e.g. repo description
matches our blurb). No requirement to strip metadata-action labels.

## Annotation map

All keys are under the `org.opencontainers.image.` prefix.

| Key | Source | Value |
|---|---|---|
| `title` | static | `twitch-1337` |
| `description` | static | repo blurb (see Context) |
| `source` | static | `https://github.com/Chronophylos/twitch-1337` |
| `url` | static | same as `source` |
| `documentation` | static | `https://github.com/Chronophylos/twitch-1337#readme` |
| `licenses` | static | `MIT OR Apache-2.0` (matches workspace `license`) |
| `authors` | static | `https://github.com/Chronophylos` |
| `vendor` | static | `Chronophylos` |
| `revision` | `ARG GIT_SHA` | default `unknown` (unchanged) |
| `version` | `ARG BUILD_NUM` | default `dev` (unchanged) |
| `created` | `ARG CREATED` | default empty string; CI/local pass ISO-8601 UTC |

Replace the current long-form description LABEL with this set. Keep all
`LABEL` lines on the final stage only (not builder/cacher).

### Dockerfile sketch

```dockerfile
ARG GIT_SHA=unknown
ARG BUILD_NUM=dev
ARG CREATED=

LABEL org.opencontainers.image.title="twitch-1337" \
  org.opencontainers.image.description="Opinionated personal Rust Twitch bot with a mind of its own — in a tiny static binary." \
  org.opencontainers.image.source="https://github.com/Chronophylos/twitch-1337" \
  org.opencontainers.image.url="https://github.com/Chronophylos/twitch-1337" \
  org.opencontainers.image.documentation="https://github.com/Chronophylos/twitch-1337#readme" \
  org.opencontainers.image.licenses="MIT OR Apache-2.0" \
  org.opencontainers.image.authors="https://github.com/Chronophylos" \
  org.opencontainers.image.vendor="Chronophylos" \
  org.opencontainers.image.revision="${GIT_SHA}" \
  org.opencontainers.image.version="${BUILD_NUM}" \
  org.opencontainers.image.created="${CREATED}"
```

Note: `GIT_SHA` / `BUILD_NUM` are already declared on the **builder**
stage for `ENV` injection into the binary. Docker ARG scope does not
carry across stages — re-declare the three ARGs on the final stage
before the `LABEL` that consumes them (same values via `--build-arg`
apply to every stage that declares the ARG).

## CI changes

`.github/workflows/docker.yml` build step:

1. Set `CREATED` to an ISO-8601 UTC timestamp for the run
   (e.g. `date -u +%Y-%m-%dT%H:%M:%SZ` into env, or equivalent).
2. Pass `CREATED=${{ env.CREATED }}` in `build-args` alongside the
   existing `GIT_SHA` and `BUILD_NUM`.

No change to image tags, cache, or dockhand webhook. Keep
`metadata-action` outputs wired to `labels:` as today.

## Local build (`Justfile`)

`just build` / `just build-nc` already pass `GIT_SHA`. Extend them to
also pass:

- `BUILD_NUM` — optional; default `dev` is fine if omitted.
- `CREATED` — `date -u +%Y-%m-%dT%H:%M:%SZ` (or shell equivalent) so
  local images are not left with an empty `created` label.

## Verification

After implementation:

1. `podman`/`docker` inspect a locally built image and confirm all
   eleven annotations are present with expected values.
2. On the next CI image push to `main`, confirm GHCR shows the repo
   blurb as the image description (package UI may lag; inspect the
   pushed image config/annotations if the UI is ambiguous).

## Out of scope / follow-ups

- Syncing the GHCR package description field independently of image
  labels (GitHub package settings API) if the UI still ignores OCI
  annotations after this lands.
- Documenting labels in CLAUDE.md beyond a one-line pointer if desired
  later — not required for this change.
