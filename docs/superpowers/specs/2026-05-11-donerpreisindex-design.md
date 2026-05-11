# Dönerpreisindex command + AI tool

Issue: [#178](https://github.com/Chronophylos/twitch-1337/issues/178)

## Goal

Expose the public dönerpreisindex dataset (https://xn--dnerindex-07a.com) to chat
as `!dpi [city]` and to the AI as a `doener_index(city?)` tool. Single
implementation, two surfaces.

## Upstream API

Two endpoints used. Both return `application/json`, no auth, public site.

### `GET /api/stats.php`

Global aggregate. Example response:

```json
{
  "ok": true,
  "total_locations": 6092,
  "total_cities": 2202,
  "min_price": 5.5,
  "max_price": 9,
  "avg_price": 6.1,
  "locations_no_price": 5304,
  "locations_no_price_pct": 87.1
}
```

### `GET /api/cities.php?q=<prefix>`

Fuzzy/prefix city search. Returns matched cities sorted (empirically by
`location_count` descending). Example for `q=Han`:

```json
{
  "ok": true,
  "query": "Han",
  "count": 5,
  "cities": [
    {"city": "Hannover",  "zip": "30459", "location_count": 51, "min_price": "6.00", "max_price": "6.00", "avg_price": "6.00"},
    {"city": "Hanau",     "zip": "63456", "location_count": 3,  "min_price": "6.00", "max_price": "6.00", "avg_price": "6.00"},
    {"city": "Handewitt", "zip": "24983", "location_count": 1,  "min_price": null,   "max_price": null,   "avg_price": null}
  ]
}
```

Prices are quoted strings; `null` when no location in the city has a price.
`zip` is one representative postcode (not a list).

### Not used

- `/api/city.php?q=<exact>` — would return the full location array (324 entries
  for Berlin). Same aggregate fields are already in `cities.php`. No reason to
  call it.
- `/api/locations.php` — full dump. Too large.

## Module layout

```
crates/core/src/doener/
  mod.rs        # re-exports
  client.rs     # DoenerClient
  types.rs      # GlobalStats, CityHit, CitiesResponse
  format.rs     # chat formatters
```

`DoenerClient` is constructed once in `Services` (`crates/core/src/lib.rs` /
wherever services are wired) and shared as `Arc<DoenerClient>` between the chat
command and the AI tool executor.

### `client.rs`

```rust
pub struct DoenerClient {
    http: reqwest::Client,
    base_url: String, // injected for tests; defaults to BASE_URL const
}

impl DoenerClient {
    pub fn new() -> Result<Self>;                       // 5s timeout, UA "twitch-1337/<pkg_version>"
    pub fn with_base_url(http: reqwest::Client, base_url: impl Into<String>) -> Self; // tests
    pub async fn stats(&self) -> Result<GlobalStats>;
    pub async fn search_cities(&self, q: &str) -> Result<Vec<CityHit>>;
}
```

Constants in `client.rs`:

```rust
const BASE_URL: &str = "https://xn--dnerindex-07a.com";
const TIMEOUT: Duration = Duration::from_secs(5);
```

Errors bubble up as `eyre::Result`. Caller logs with `?error` and falls back to
a user-visible "API down" message.

### `types.rs`

`Deserialize`-only structs. Subset of upstream fields — drop anything the bot
does not display.

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct GlobalStats {
    pub total_locations: u32,
    pub total_cities: u32,
    pub min_price: f64,
    pub max_price: f64,
    pub avg_price: f64,
    pub locations_no_price_pct: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CityHit {
    pub city: String,
    pub location_count: u32,
    #[serde(deserialize_with = "deser_optional_f64_str")]
    pub min_price: Option<f64>,
    #[serde(deserialize_with = "deser_optional_f64_str")]
    pub max_price: Option<f64>,
    #[serde(deserialize_with = "deser_optional_f64_str")]
    pub avg_price: Option<f64>,
}
```

Prices are strings upstream (`"6.00"`) or `null`. Custom deserializer parses
both forms; unparseable strings become `None`.

`CitiesResponse` is a thin envelope with `cities: Vec<CityHit>`. The client
returns `Vec<CityHit>` to callers — envelope stays internal.

### `format.rs`

Pure functions, return `String`. No I/O. Easy unit tests against fixtures.

```rust
pub fn format_global(s: &GlobalStats) -> String;
pub fn format_city(c: &CityHit) -> String;             // for single best match
pub fn format_did_you_mean(hits: &[CityHit]) -> String; // 2..N matches, top 3
pub fn format_not_found(query: &str) -> String;
```

Output (German, matches `news.rs` tone, no Markdown):

| Case | Output |
|---|---|
| global | `Döner-Index DE: 6092 Buden in 2202 Städten, ⌀ 6.10€ (5.50–9.00€). 87% ohne Preis.` |
| city with price | `Hannover: 51 Buden, ⌀ 6.00€ (6.00–6.00€).` |
| city no price | `Handewitt: 1 Bude, noch keine Preise.` |
| 2–3 hits | `Meintest du: Hannover (51), Hanau (3), Handewitt (1)?` |
| 0 hits | `FeelsDankMan keine Stadt für 'xyz' gefunden.` |
| API down | `FeelsDankMan döner-index API down` |

Numbers: avg/min/max formatted as `{:.2}€`. `location_count == 1` → `"Bude"`,
else `"Buden"`.

## Chat command

`crates/core/src/commands/doener.rs` — implements the `Command` trait used by
the existing dispatcher in `crates/core/src/commands/mod.rs`. Registered next
to the other commands in the command-handler wiring.

Trigger: `!dpi`.

Behavior:

1. Strip trigger, trim arg.
2. Empty arg → `client.stats()` → `format_global`.
3. Non-empty arg → `client.search_cities(arg)`.
   - 0 hits → `format_not_found`.
   - exactly 1 hit → `format_city`.
   - 2+ hits, but the first hit's city name equals the query case-insensitively
     → treat as 1 hit (`format_city` on the first). Avoids `!dpi Berlin`
     returning "Meintest du: Berlin (324), Berlingerode (1)?".
   - otherwise → `format_did_you_mean` (top 3).

Cooldown: per-user, default 30s, via existing `PerUserCooldown` like `!up`.
Add `pub doener: u64` to `CooldownsConfig` in `crates/core/src/config.rs`
with `default_doener_cooldown() -> 30` and update the `Default` impl and the
TOML example.

`config.toml.example` `[cooldowns]` block gains:

```toml
# doener = 30   # !dpi command (default: 30)
```

No `[doener]` config block. Base URL and timeout are consts.

## AI tool

Registered in `crates/core/src/ai/content/tools.rs::ai_tools()` (always
present when `[ai]` is configured — not gated by `[ai.web]`, since this is a
first-party data tool, not generic web access).

```rust
ToolDefinition {
    name: "doener_index".into(),
    description: "Look up the German Döner price index from dönerindex.com. \
        Without `city`, returns the country-wide aggregate (location count, \
        avg/min/max price). With `city` (free-form), returns the top matching \
        cities and their per-city aggregate. Use this for any question about \
        Döner prices, kebab prices, or how expensive Döner is in a German city.".into(),
    parameters: serde_json::json!({
        "type": "object",
        "properties": {
            "city": {
                "type": "string",
                "description": "Optional city name or prefix. German spelling preferred (e.g. 'Köln', 'München')."
            }
        }
    }),
}
```

Dispatch added to `crates/core/src/ai/content/executor.rs::execute_tool_call`:

```rust
"doener_index" => match call.parse_args::<DoenerIndexArgs>() {
    Ok(args) => self.execute_doener_index(args).await,
    Err(err) => format_args_error(...),
},
```

`DoenerIndexArgs { city: Option<String> }`.

`execute_doener_index` returns JSON (not formatted chat strings — the model
phrases it):

```jsonc
// no city
{"scope": "global", "stats": { /* GlobalStats */ }}

// city given, 0 hits
{"scope": "city", "query": "xyz", "hits": []}

// city given, N hits (top 5, raw aggregate)
{"scope": "city", "query": "Han", "hits": [
  {"city": "Hannover", "location_count": 51, "avg_price": 6.0, "min_price": 6.0, "max_price": 6.0},
  ...
]}

// API failure
{"error": "doener_index API unavailable"}
```

The `Executor` gains an `Arc<DoenerClient>` field; constructed in
`Services` alongside the other shared clients.

`is_web_tool` does not include `doener_index`. The tool is allowed even when
`[ai.web]` is disabled.

## Wiring

`Services` (the struct holding shared deps in `crates/core/src/lib.rs`) grows:

```rust
pub doener: Arc<DoenerClient>,
```

Constructed once at startup with `DoenerClient::new()`. If construction fails
(reqwest builder), bot startup fails — same posture as the aviation client's
historical behavior wasn't quite this, but `DoenerClient::new` only fails on
an invalid TLS root store / OS-level reqwest error, which is fatal anyway.

The doener client is consumed by:
- the `!dpi` command handler (`crates/core/src/commands/doener.rs`)
- the AI executor (`crates/core/src/ai/content/executor.rs`)

## Error handling

Single best-effort HTTP call per invocation. No retries, no fallback host.

- reqwest error / timeout / non-2xx → `Err(eyre!(...))`, logged with `?error`
  and `endpoint = "stats"|"cities"`, query (if present).
- `ok: false` in response body (defensive — never observed) → treated as
  failure.
- JSON parse failure → failure.

Chat: `"FeelsDankMan döner-index API down"`. AI tool: `{"error": "..."}` — the
model decides how to surface this; existing executor pattern.

## Tests

### Unit

- `format.rs` against hand-written `GlobalStats` / `CityHit` fixtures, all
  branches (global, single, plural-hits, no-hits, missing prices, singular vs
  plural "Bude/Buden").
- `types.rs` deserializer: stringy `"6.00"`, `null`, and a malformed string
  all yield expected `Option<f64>`.

### Client (wiremock)

`crates/core/src/doener/client.rs#[cfg(test)] mod tests`:

- `stats` 200 → struct populated correctly.
- `stats` 500 → `Err`.
- `cities` 200 with three hits → struct populated, order preserved.
- `cities` 200 with empty `cities` → `Ok(vec![])`.
- timeout (delayed response > 5s) → `Err`. Use a shorter test timeout via
  `with_base_url` + a low-timeout test-only `reqwest::Client` to keep the
  test fast.

### Executor

`crates/core/src/ai/content/executor.rs` — add tests beside existing ones:

- `doener_index` with no city → JSON contains `"scope":"global"`.
- `doener_index` with city → JSON contains `"scope":"city"` and hits.
- Args parse error path covered (model passes a non-string `city`).

Use wiremock with a base URL injected into the test `DoenerClient`.

### Command

`crates/core/src/commands/doener.rs` — wiremock for the upstream, fake IRC
client, assert the bot says the expected string for each branch.

### `ai_tools_surface_contains_search_and_read`

Existing test in `tools.rs` asserts exact tool order. Update to include
`doener_index`. Add a separate assertion that `doener_index` is **not** in
`WEB_TOOL_NAMES`.

## Out of scope

- Submitting prices (`/api/submit.php` exists, would need auth/captcha).
- Map / location-level detail (would need `city.php` + truncation logic).
- Caching / TTL. Upstream is cheap and small; revisit if rate-limited.
- Localization. Output is German; bot is German.
- A `[doener]` config block. Not needed today.

## Files touched

New:
- `crates/core/src/doener/mod.rs`
- `crates/core/src/doener/client.rs`
- `crates/core/src/doener/types.rs`
- `crates/core/src/doener/format.rs`
- `crates/core/src/commands/doener.rs`
- `docs/superpowers/specs/2026-05-11-donerpreisindex-design.md` (this file)

Modified:
- `crates/core/src/lib.rs` — add `pub mod doener;`, `Services.doener`, wire.
- `crates/core/src/commands/mod.rs` — register `!dpi`.
- `crates/core/src/config.rs` — `CooldownsConfig.doener` (default 30).
- `crates/core/src/ai/content/tools.rs` — register `doener_index` tool def.
- `crates/core/src/ai/content/executor.rs` — `execute_doener_index`, dispatch
  arm, `Executor.doener` field, executor constructor sites.
- `crates/twitch-1337/config.toml.example` — `# doener = 30` line.

Cargo: no new dependencies (reqwest, serde, eyre, wiremock, async-trait
already in tree).
