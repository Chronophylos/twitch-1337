# OCI Image Labels Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bake the full OpenContainers annotation set into the final `scratch` image so GHCR and local builds both carry title, description (repo blurb), source/url/docs, licenses, authors, vendor, revision, version, and created.

**Architecture:** Dockerfile final stage is the source of truth for all eleven OCI labels. Dynamic fields use build-args (`GIT_SHA`, `BUILD_NUM`, new `CREATED`) re-declared on the final stage. CI and `just build` pass those args; `docker/metadata-action` labels stay wired as today.

**Tech Stack:** Dockerfile multi-stage build, GitHub Actions (`docker.yml`), Justfile/`podman build`, OCI annotation labels.

**Spec:** `docs/superpowers/specs/2026-07-18-oci-image-labels-design.md`

## Global Constraints

- Description text verbatim: `Opinionated personal Rust Twitch bot with a mind of its own — in a tiny static binary.`
- `licenses` = `MIT OR Apache-2.0`; `authors` = `https://github.com/Chronophylos`; `vendor` = `Chronophylos`.
- `source` / `url` = `https://github.com/Chronophylos/twitch-1337`; `documentation` = that URL + `#readme`.
- `revision` ← `GIT_SHA` (default `unknown`); `version` ← `BUILD_NUM` (default `dev`); `created` ← `CREATED` (default empty).
- Do not change image tags, cache, dockhand webhook, or remove `labels: ${{ steps.meta.outputs.labels }}`.
- Re-declare ARGs on the final stage — Docker ARG scope does not cross `FROM`.
- Work in worktree `.worktrees/feature-oci-image-labels` on branch `feature/oci-image-labels`.
- Commits: Conventional Commits with stupid subject + serious body (repo `git-commits.mdc`).
- No Rust edits → skip clippy/nextest; still keep Dockerfile/Justfile/workflow valid.

---

## File Structure

| File | Responsibility |
|---|---|
| `Dockerfile` | Full OCI `LABEL` set on final stage; re-declare `GIT_SHA` / `BUILD_NUM` / `CREATED` |
| `.github/workflows/docker.yml` | Compute UTC `CREATED`; pass as build-arg |
| `Justfile` | Pass `CREATED` (and keep `GIT_SHA`) on `build` / `build-no-cache` |

---

### Task 1: Final-stage OCI labels in Dockerfile

**Files:**
- Modify: `Dockerfile` (final `FROM scratch` stage; leave builder-stage `ARG`/`ENV` for binary provenance unchanged)

**Interfaces:**
- Consumes: build-args `GIT_SHA`, `BUILD_NUM`, `CREATED` (must be re-declared after `FROM scratch`)
- Produces: eleven `org.opencontainers.image.*` labels on the runtime image

- [ ] **Step 1: Replace the single description LABEL with the full set**

On the final stage, replace the current block:

```dockerfile
FROM scratch
LABEL org.opencontainers.image.description="Rust Twitch IRC bot: 1337 tracker, community pings, AI/flight commands, and scheduled messages — single persistent IRC connection with broadcast-based handler routing."
ENV DATA_DIR=/data
```

with:

```dockerfile
FROM scratch

# Re-declare build-args here — ARG scope does not cross FROM.
# Builder stage still owns ENV GIT_SHA/BUILD_NUM for the binary;
# these feed OCI labels only.
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

ENV DATA_DIR=/data
```

Do not move or duplicate the builder-stage `ARG GIT_SHA` / `ARG BUILD_NUM` / `ENV` block — binary provenance stays as-is.

- [ ] **Step 2: Sanity-check Dockerfile syntax (no full musl build yet)**

Run: `hadolint Dockerfile` (if installed) or skip if missing.

Expected: no new errors related to the LABEL/ARG block. Existing `# hadolint ignore=DL3008` on the base stage is unrelated.

- [ ] **Step 3: Commit**

```bash
git add Dockerfile
git commit -m "$(cat <<'EOF'
feat(docker): image labels went full oci drip for the ghcr stans 🏷️✨

Bake the full OpenContainers annotation set into the final scratch
stage, using the GitHub repo blurb for description and wiring
revision/version/created to build-args (re-declared on the final stage).

EOF
)"
```

---

### Task 2: Pass `CREATED` from CI

**Files:**
- Modify: `.github/workflows/docker.yml` (build job steps around the build-push action)

**Interfaces:**
- Consumes: Task 1 final-stage `ARG CREATED`
- Produces: CI builds with non-empty `org.opencontainers.image.created` (ISO-8601 UTC)

- [ ] **Step 1: Add a timestamp step before build-push**

Insert a step immediately before the `docker/build-push-action` step (after `meta`):

```yaml
      - name: Set image created timestamp
        run: echo "CREATED=$(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$GITHUB_ENV"
```

- [ ] **Step 2: Pass `CREATED` in build-args**

Update the build-push step so `env` and `build-args` include `CREATED`:

```yaml
      - uses: docker/build-push-action@53b7df96c91f9c12dcc8a07bcb9ccacbed38856a # v7
        env:
          GIT_SHA: ${{ github.sha }}
          BUILD_NUM: b${{ github.run_number }}
          CREATED: ${{ env.CREATED }}
        with:
          context: .
          push: true
          tags: ${{ steps.meta.outputs.tags }}
          labels: ${{ steps.meta.outputs.labels }}
          build-args: |
            GIT_SHA=${{ env.GIT_SHA }}
            BUILD_NUM=${{ env.BUILD_NUM }}
            CREATED=${{ env.CREATED }}
          cache-from: type=gha
          cache-to: type=gha,mode=max
```

Keep the dockhand redeploy step unchanged. Do not remove `labels: ${{ steps.meta.outputs.labels }}`.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/docker.yml
git commit -m "$(cat <<'EOF'
ci(docker): created timestamp finally shows up for the label girlies ⏰🏷️

Pass an ISO-8601 UTC CREATED build-arg into the image build so
org.opencontainers.image.created is set on every main push.

EOF
)"
```

---

### Task 3: Pass `CREATED` from Justfile local builds

**Files:**
- Modify: `Justfile` (`build` and `build-no-cache` recipes)

**Interfaces:**
- Consumes: Task 1 final-stage `ARG CREATED` (and existing `GIT_SHA`)
- Produces: local `just build` images with non-empty `created` label

- [ ] **Step 1: Update both build recipes**

Replace:

```just
build:
  podman build --build-arg GIT_SHA=$(git rev-parse --short=12 HEAD) -t chronophylos/twitch-1337:latest .

build-no-cache:
  podman build --no-cache --build-arg GIT_SHA=$(git rev-parse --short=12 HEAD) -t chronophylos/twitch-1337:latest .
```

with:

```just
build:
  podman build \
    --build-arg GIT_SHA=$(git rev-parse --short=12 HEAD) \
    --build-arg CREATED=$(date -u +%Y-%m-%dT%H:%M:%SZ) \
    -t chronophylos/twitch-1337:latest .

build-no-cache:
  podman build --no-cache \
    --build-arg GIT_SHA=$(git rev-parse --short=12 HEAD) \
    --build-arg CREATED=$(date -u +%Y-%m-%dT%H:%M:%SZ) \
    -t chronophylos/twitch-1337:latest .
```

`BUILD_NUM` may stay at Dockerfile default `dev` for local builds (per spec).

- [ ] **Step 2: Commit**

```bash
git add Justfile
git commit -m "$(cat <<'EOF'
chore(just): local builds pass created so the oci drip is not mid 🏷️

Forward an ISO-8601 UTC CREATED build-arg from just build / build-no-cache
so locally built images carry org.opencontainers.image.created.

EOF
)"
```

---

### Task 4: Verify labels on a local image

**Files:**
- None (verification only)

**Interfaces:**
- Consumes: Tasks 1–3
- Produces: inspected label map confirming all eleven keys

- [ ] **Step 1: Build the image**

From the worktree root:

```bash
just build
```

Expected: `podman build` succeeds and tags `chronophylos/twitch-1337:latest` (or `localhost/chronophylos/twitch-1337:latest` depending on podman short-name mode).

This is a full musl release build — expect several minutes on a cold cache.

- [ ] **Step 2: Inspect OCI labels**

```bash
podman inspect chronophylos/twitch-1337:latest --format '{{json .Config.Labels}}' | jq .
```

If the short name fails, try `localhost/chronophylos/twitch-1337:latest`.

Expected keys (all present):

| Key | Expected value pattern |
|---|---|
| `org.opencontainers.image.title` | `twitch-1337` |
| `org.opencontainers.image.description` | repo blurb (exact) |
| `org.opencontainers.image.source` | `https://github.com/Chronophylos/twitch-1337` |
| `org.opencontainers.image.url` | same as source |
| `org.opencontainers.image.documentation` | source + `#readme` |
| `org.opencontainers.image.licenses` | `MIT OR Apache-2.0` |
| `org.opencontainers.image.authors` | `https://github.com/Chronophylos` |
| `org.opencontainers.image.vendor` | `Chronophylos` |
| `org.opencontainers.image.revision` | 12-char short SHA from `git rev-parse --short=12 HEAD` |
| `org.opencontainers.image.version` | `dev` |
| `org.opencontainers.image.created` | ISO-8601 UTC matching build time (e.g. `2026-07-18T09:21:00Z`) |

- [ ] **Step 3: No further commit unless Step 2 forced a fix**

If inspection fails, fix the offending file in a new commit (do not amend), then re-run Steps 1–2.

---

## Self-review (plan vs spec)

| Spec requirement | Task |
|---|---|
| Full OCI set on final stage | Task 1 |
| Repo blurb as description | Task 1 |
| `revision`/`version` from existing args | Task 1 |
| New `CREATED` arg | Task 1 |
| CI passes `CREATED` | Task 2 |
| Keep metadata-action labels | Task 2 (explicit non-change) |
| Justfile passes `CREATED` | Task 3 |
| Local inspect verification | Task 4 |
| No GHCR package About API / CLAUDE.md churn | omitted (non-goals) |
