# mem0 memory replacement — design

Date: 2026-06-12
Status: approved

## Summary

Replace the custom v2 AI memory system (markdown SOUL/LORE/user/state files,
nonce-fenced prompt injection, model-driven write tools, nightly dreamer
ritual, dashboard editor) with [mem0](https://github.com/mem0ai/mem0)
self-hosted as a container next to the bot. The bot calls mem0's REST API:
semantic search before each `!ai` turn, automatic fact extraction after it.
The model keeps a single `forget` tool. Everything else in the old memory
module is deleted.

## Decisions (locked during brainstorming)

| Question | Decision |
|---|---|
| Scope | Full replacement of the memory module |
| Ingestion | `!ai` turns only (user message + bot reply); no ambient chat, no nightly batch |
| Persona (SOUL) | Static, lives in `prompts/system.md`; no self-evolving identity |
| mem0 backend | OpenRouter for both extraction and embeddings (nano-class extraction model + `openai/text-embedding-3-small`); one key, one vendor. Verified viable: mem0's OpenAI LLM and embedder classes both honor `openai_base_url` |
| Integration style | Automatic search-before / add-after, plus one `forget` tool |

## Research facts the design depends on

- mem0 OSS v2.0 (May 2026) ships an official REST server (FastAPI) with
  built-in auth (`X-API-Key`, `ADMIN_API_KEY`, JWT). Apache 2.0.
- There is **no usable prebuilt image**: `mem0/mem0-api-server` on Docker Hub
  was last pushed 2025-09-10 (pre-2.0: old API, no auth). The supported path
  per <https://docs.mem0.ai/open-source/setup> is building from the repo's
  `server/` directory (`server/.env` + `make up`; REST API on port 8888,
  dashboard on 3000, bundled pgvector Postgres).
- v2.0 removed graph memory from OSS; entity linking is built in and needs no
  extra infra. The stack is server + `pgvector/pgvector:pg17`. No Neo4j.
- OSS endpoints have no `/v1/` prefix: `POST /memories`, `POST /search`,
  `GET /memories`, `DELETE /memories?user_id=`, `DELETE /memories/{id}`.
  Extraction on `add` is synchronous (one LLM call).
- The Platform's "group chat" auto-attribution is platform-only. OSS stores a
  per-message `name` as `actor_id` metadata. We attribute explicitly via
  `user_id=<Twitch user ID>` instead, which is more robust (stable IDs,
  display names change).
- Scoping semantics (verified in `mem0/memory/main.py`): an `add()` with both
  `user_id` and `agent_id` stores **one** memory carrying both IDs, and
  search/list filters are a subset-match AND over only the keys provided. A
  search by `agent_id` alone therefore returns memories that also carry a
  `user_id` — there is no built-in personal/shared partition.
- Auth (verified in `server/auth.py`): `ADMIN_API_KEY` is legacy-labeled but
  accepted as `X-API-Key` on **all** memory endpoints (constant-time compare,
  treated as admin). Per-user `m0sk_` keys require the Next.js dashboard or a
  JWT login flow + `POST /api-keys` — ceremony we skip for a single-tenant
  internal service.
- Base URL (verified in `mem0/llms/openai.py` + `mem0/embeddings/openai.py`):
  both classes honor `openai_base_url`, so an OpenAI-compatible proxy
  (OpenRouter) can serve extraction and embeddings with one key. Caveat:
  env-var precedence differs between the two classes (`OPENAI_BASE_URL` vs
  `OPENAI_API_BASE` checked first), so the base URL must be set explicitly in
  both the `llm` and `embedder` config blocks, never via env.
- No official Rust SDK. The needed surface is four endpoints; a small
  `reqwest` client matches the existing rustls/musl stack.
- v2.0 is about a month old with breaking changes behind it. Pin the image
  digest; expect doc roughness.

## 1. Infrastructure (docker compose)

**Image build (CI → GHCR).** Since no usable prebuilt image exists, a new
workflow in this repo (`mem0-image.yml`) checks out `mem0ai/mem0` at a pinned
release tag, builds the `server/` Dockerfile, and pushes
`ghcr.io/chronophylos/mem0-server:<mem0-tag>` (plus `:latest`). The pinned
tag lives in one place in the workflow (or a small pin file) so a version
bump is a one-line PR. Triggers: `workflow_dispatch` plus pushes touching the
pin. Not a required status check. Like all third-party supply-chain-critical
references, the mem0 checkout is pinned to a tag resolved to a commit SHA.

Two services join the homelab stack (compose lives in `stacks/` on
docker.homelab, not in this repo):

- **`mem0`**: `ghcr.io/chronophylos/mem0-server:<mem0-tag>` (built above).
  Env: `OPENAI_API_KEY` (the OpenRouter key — mem0's OpenAI-compatible
  classes read it), `ADMIN_API_KEY` (bot auth via `X-API-Key`; legacy-labeled
  but verified to work on all memory routes), `JWT_SECRET`,
  `POSTGRES_PASSWORD`, `MEM0_TELEMETRY=false`. The mem0 config sets
  `openai_base_url` to OpenRouter explicitly in **both** the `llm` and
  `embedder` blocks (env-var precedence differs between them); embedding
  model id is `openai/text-embedding-3-small`. Internal compose network only;
  never published through the reverse proxy.
- **`mem0-postgres`**: `pgvector/pgvector:pg17`, named volume, included in
  the existing `pg_dump` backup cron.
- No mem0 Next.js dashboard container. The bot dashboard gets a read-only
  memory page instead (section 5). The mem0 dashboard can be added later.

## 2. Bot-side client

New module `crates/core/src/ai/mem0.rs`: a `reqwest` client behind a trait so
tests can fake it, mirroring the `LlmClient` pattern.

```rust
trait MemoryApi: Send + Sync {
    async fn search(&self, query: &str, scope: Scope) -> Result<Vec<MemoryHit>>; // POST /search
    async fn add(&self, messages: &[TurnMessage], user_id: &str,
                 agent_id: &str, metadata: Metadata) -> Result<()>;              // POST /memories
    async fn delete_user(&self, user_id: &str) -> Result<()>;                    // DELETE /memories?user_id=
    async fn delete_one(&self, memory_id: &str) -> Result<()>;                   // DELETE /memories/{id}
    async fn list_user(&self, user_id: &str) -> Result<Vec<MemoryHit>>;          // GET /memories
}
```

Auth via `X-API-Key`. Config split follows the aviationstack precedent:

- `config.toml` `[mem0]`: `api_key` only (`SecretString`, no `Serialize`).
- `settings.ron`: `enabled`, `base_url` (restart-required), `timeout_secs`,
  `search_limit` (default 8). New dashboard card `/settings → Memory`.

Client init failure at startup follows the aviation-client pattern: log,
disable memory, keep running.

## 3. Per-turn flow (`!ai`)

1. **Search before.** Two scoped searches with the user's message as the
   query: `user_id = speaker's Twitch ID` and `agent_id = channel`. Since
   every memory carries both IDs and filters are subset-match, the channel
   search is channel-wide — it includes other users' facts — and the speaker
   search selects a subset of the same pool. The dual search is deliberately
   a **ranking device**, not a partition: it guarantees the speaker's own
   facts hold top-k slots even when the channel-wide results would crowd
   them out. Top-k each (`search_limit`), deduped by memory id, merged into
   one "What you remember" context block injected where the old
   durable-memory block went. The recent-chat history injection survives
   unchanged; it comes from the in-memory history buffer, not the memory
   store, and its rendering code relocates out of `inject.rs` before that
   file is deleted.
2. **Agent loop.** Unchanged except memory tools. `write_file`,
   `write_state`, `delete_state` are removed. One new tool **`forget`**
   deletes the speaker's own memories only
   (`DELETE /memories?user_id=<speaker>`). It takes no target argument, so no
   permission matrix is needed. Web tools, emotes, and the rest of the loop
   are untouched.
3. **Add after.** Once the reply is sent, `tokio::spawn` a fire-and-forget
   `add()` with `[user message, bot reply]`, `user_id = speaker`,
   `agent_id = channel`, metadata `{username, display_name}`. mem0's
   extraction decides what is memorable.

   **Forget suppresses add.** If the model invoked `forget` during the turn,
   the add-after is skipped entirely. Otherwise the turn that erased a user
   would immediately mint a fresh memory from the erase request itself
   ("forget everything about me" / "done, wiped"). The executor records that
   `forget` fired and the turn flow checks that flag before spawning the
   add.

**Degradation:** search failure logs a warning and the turn proceeds without
memories; add failures log and drop. `!ai` never breaks because mem0 is down.

## 4. Deletions

- `crates/core/src/ai/memory/` entirely: `store.rs`, `inject.rs` (after
  relocating recent-chat rendering), `tools.rs`, `ritual.rs` (dreamer),
  `transcript.rs`, `frontmatter.rs`, `sanitize.rs`, `types.rs`.
- Settings: `AiMemory` (byte caps) and `AiDreamer` structs; settings.ron
  schema bump with migration for existing files.
- Dashboard: `/memory` editor routes and templates, the AI · Memory and
  Dreamer settings cards.
- `$DATA_DIR/memories/` is no longer written. Existing files stay on disk
  untouched as fallback.
- Tests: `crates/core/tests/memory_v2.rs` and the unit tests inside the
  deleted module.

`prompts/system.md` and `prompts/ai_instructions.md` stay. Persona lives
wholly in `system.md`.

## 5. Dashboard replacement page

Read-only `/memory` page backed by mem0 REST: search box, per-user memory
listing, delete button per memory and per user (mod-gated). No free-text
editing, so no mtime/conflict handling.

## 6. Migration of existing memories

One-shot, sentinel-gated (`$DATA_DIR/.mem0_migrated`). On first launch with
mem0 enabled:

- each `memories/users/<id>.md` body → `add(user_id=<id>, infer=true)`
- `LORE.md` body → `add(agent_id=channel, infer=true)`
- `SOUL.md` is **not** migrated. Its content must be folded into
  `prompts/system.md` by hand before deploy (deploy checklist item).
- state files and transcripts are not migrated.

The old tree is left on disk. Migration failures log per file and do not
write the sentinel, so the next start retries.

## 7. Testing

- `TestBotBuilder` gains a fake `MemoryApi` (queue search hits, record adds
  and deletes), alongside the existing fake LLM.
- Integration tests: memories injected into the prompt; add fired after the
  reply with correct `user_id`/`agent_id`; `forget` deletes only the speaker;
  a turn where `forget` fired produces **no** add-after; graceful degradation
  when the fake returns errors; migration runs once and writes the sentinel
  only on success.

## Risks

- **Quality regression is plausible.** Today the model sees a coherent 4 KiB
  character sheet per user; afterwards it sees top-k extracted snippets.
  Accepted as part of the full-replacement decision; old files remain on disk
  as fallback.
- **mem0 v2.0 churn.** A month old with breaking changes just behind it, and
  we build the server image ourselves because upstream's prebuilt image is
  stale. Pin the mem0 source tag in the build workflow, target the post-2.0
  API only, and expect the `server/` Dockerfile to move under us on bumps.
- **New ops surface.** A Python service plus Postgres on the homelab, and
  OpenRouter spend per `!ai` turn (one extraction call plus embeddings; cents
  per day at nano-class pricing, but nonzero and per-turn). OpenRouter also
  adds a proxy hop to every turn's search and add — acceptable, since search
  only embeds the query and add is fire-and-forget.
- **`ADMIN_API_KEY` is legacy.** Upstream's recommended auth is per-user
  `m0sk_` keys, which need the dashboard or a JWT flow we don't run. The
  pinned image tag means the legacy path can't break under us silently;
  re-verify it on every mem0 version bump.

## Deploy checklist (when implementation lands)

1. Fold current `SOUL.md` content into `prompts/system.md`.
2. Run the `mem0-image.yml` workflow once so
   `ghcr.io/chronophylos/mem0-server:<mem0-tag>` exists; add `mem0` +
   `mem0-postgres` to the homelab compose with secrets; verify the OpenAPI
   explorer (`/docs`) responds on the internal network.
3. Add `[mem0] api_key` to `config.toml`; set `base_url` + `enabled` via the
   dashboard; restart the bot.
4. Confirm the one-shot migration ran (sentinel present, memories visible on
   the new `/memory` page).
