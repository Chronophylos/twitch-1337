use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr as _};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::llm::{
    self, Message, ToolCall, ToolCallRound, ToolChatCompletionRequest, ToolChatCompletionResponse,
    ToolDefinition, ToolResultMessage,
};
use crate::memory::scope::{is_write_allowed, seed_confidence, trust_level_for};
use crate::memory::{Scope, UserRole};

const MEMORY_FILENAME: &str = "ai_memory.ron";

/// Groups the memory store, its file path, and capacity limit.
pub struct MemoryConfig {
    pub store: Arc<RwLock<MemoryStore>>,
    pub path: PathBuf,
    pub max_memories: usize,
}
const MAX_EXTRACTION_ROUNDS: usize = 3;

const EXTRACTION_SYSTEM_PROMPT: &str = "\
You just had a conversation in a Twitch chat. Based on the exchange below, \
decide if any facts are worth remembering long-term about users, the channel, \
or the community. You can save new facts, overwrite outdated ones, or delete \
incorrect ones. Only save things that would be useful across future \
conversations. Do not save trivial or ephemeral things like greetings or \
simple questions.";

/// A single remembered fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub fact: String,
    pub scope: Scope,
    pub sources: Vec<String>,
    pub confidence: u8,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub access_count: u32,
}

impl Memory {
    pub fn new(
        fact: String,
        scope: Scope,
        source: String,
        confidence: u8,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            fact,
            scope,
            sources: vec![source],
            confidence,
            created_at: now,
            updated_at: now,
            last_accessed: now,
            access_count: 0,
        }
    }

    /// Relevance score in `[0, ~confidence]` space. Combines confidence,
    /// exponential decay on `last_accessed` (half-life `half_life_days`),
    /// and a sub-linear boost from `access_count`.
    pub fn score(&self, now: DateTime<Utc>, half_life_days: u32) -> f64 {
        // i64 → f64: precision loss only matters beyond ~285 million years;
        // age_days is bounded by clock skew in practice.
        let age_days = (now - self.last_accessed).num_seconds() as f64 / 86_400.0;
        let decay = (-(2f64.ln()) * age_days / f64::from(half_life_days)).exp();
        let hits = (1.0 + f64::from(self.access_count).ln_1p() / 5.0).max(1.0);
        (f64::from(self.confidence) / 100.0) * decay * hits
    }
}

/// A mapping from subject_id to the user's current display name, used to
/// present memories without leaking numeric user IDs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub username: String,
    pub updated_at: DateTime<Utc>,
}

/// Persistent store of AI memories, serialized to RON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStore {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub memories: HashMap<String, Memory>,
    #[serde(default)]
    pub identities: HashMap<String, Identity>,
}

fn default_version() -> u32 {
    2
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self {
            version: 2,
            memories: HashMap::new(),
            identities: HashMap::new(),
        }
    }
}

impl MemoryStore {
    /// Load from disk. Returns empty store if file doesn't exist.
    pub fn load(data_dir: &Path) -> Result<(Self, PathBuf)> {
        let path = data_dir.join(MEMORY_FILENAME);
        let store = if path.exists() {
            let data = std::fs::read_to_string(&path).wrap_err("Failed to read ai_memory.ron")?;
            ron::from_str(&data).wrap_err("Failed to parse ai_memory.ron")?
        } else {
            info!("No ai_memory.ron found, starting with empty memory store");
            Self::default()
        };

        info!(count = store.memories.len(), "Loaded AI memories");
        Ok((store, path))
    }

    /// Write current state to disk using write+rename for atomicity.
    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp_path = path.with_extension("ron.tmp");
        let data = ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())
            .wrap_err("Failed to serialize AI memories")?;
        std::fs::write(&tmp_path, &data).wrap_err("Failed to write ai_memory.ron.tmp")?;
        std::fs::rename(&tmp_path, path)
            .wrap_err("Failed to rename ai_memory.ron.tmp to ai_memory.ron")?;
        debug!("Saved AI memories to disk");
        Ok(())
    }

    /// Format memories for injection into the system prompt.
    /// Returns None if there are no memories.
    pub fn format_for_prompt(&self) -> Option<String> {
        if self.memories.is_empty() {
            return None;
        }
        let mut lines: Vec<String> = self
            .memories
            .iter()
            .map(|(key, mem)| format!("- {}: {}", key, mem.fact))
            .collect();
        lines.sort(); // Deterministic ordering
        Some(format!("\n\n## Known facts\n{}", lines.join("\n")))
    }

    /// Format memories for the extraction prompt (key: fact list).
    pub fn format_for_extraction(&self) -> String {
        if self.memories.is_empty() {
            return "(none)".to_string();
        }
        let mut lines: Vec<String> = self
            .memories
            .iter()
            .map(|(key, mem)| format!("- {}: {}", key, mem.fact))
            .collect();
        lines.sort();
        lines.join("\n")
    }

    /// Evict the lowest-scoring memory whose scope matches `tag` when the
    /// scope is already at capacity. Returns the evicted key, if any.
    pub fn evict_lowest_in_scope(
        &mut self,
        tag: &str,
        now: DateTime<Utc>,
        half_life_days: u32,
    ) -> Option<String> {
        let candidates: Vec<(String, f64, DateTime<Utc>)> = self
            .memories
            .iter()
            .filter(|(_, m)| m.scope.tag() == tag)
            .map(|(k, m)| (k.clone(), m.score(now, half_life_days), m.last_accessed))
            .collect();
        let (key, _, _) = candidates.into_iter().min_by(|a, b| {
            // score() is finite (no NaN-producing paths); unwrap_or is a defensive fallback.
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.2.cmp(&b.2)) // older last_accessed wins the tie
        })?;
        self.memories.remove(&key);
        Some(key)
    }

    /// Insert or refresh the `(user_id -> username)` mapping. Username
    /// changes overwrite; timestamp is bumped on every call so Identity
    /// entries reflect when the mapping was last observed.
    pub fn upsert_identity(&mut self, user_id: &str, username: &str, now: DateTime<Utc>) {
        self.identities.insert(
            user_id.to_string(),
            Identity {
                username: username.to_string(),
                updated_at: now,
            },
        );
    }

    /// Execute a single extractor tool call against the store. Routes
    /// `save_memory` through the permission matrix and `get_memories` for
    /// read-only inspection. Other tool names return an error string.
    pub fn execute_tool_call(&mut self, call: &ToolCall, ctx: &DispatchContext<'_>) -> String {
        if let Some(err) = &call.arguments_parse_error {
            return format!(
                "Error: tool '{name}' arguments were not valid JSON ({error}). \
                 Raw text: {raw}. Resend with a valid JSON object.",
                name = call.name,
                error = err.error,
                raw = err.raw,
            );
        }
        match call.name.as_str() {
            "save_memory" => self.handle_save_memory(call, ctx),
            "get_memories" => self.handle_get_memories(call, ctx),
            other => format!("Unknown tool: {other}"),
        }
    }

    fn handle_save_memory(&mut self, call: &ToolCall, ctx: &DispatchContext<'_>) -> String {
        let args = &call.arguments;
        let scope_str = args.get("scope").and_then(|v| v.as_str()).unwrap_or("");
        let slug = args.get("slug").and_then(|v| v.as_str()).unwrap_or("");
        let fact = args.get("fact").and_then(|v| v.as_str()).unwrap_or("");
        let subject_id = args
            .get("subject_id")
            .and_then(|v| v.as_str())
            .map(String::from);

        if slug.is_empty() || fact.is_empty() {
            return "Error: save_memory requires non-empty 'slug' and 'fact'".into();
        }
        let scope = match (scope_str, subject_id) {
            ("user", Some(s)) => Scope::User { subject_id: s },
            ("pref", Some(s)) => Scope::Pref { subject_id: s },
            ("lore", None) => Scope::Lore,
            ("user" | "pref", None) => {
                return "Error: save_memory requires 'subject_id' for user/pref scope".into();
            }
            ("lore", Some(_)) => {
                return "Error: save_memory must NOT include 'subject_id' for lore scope".into();
            }
            _ => return format!("Error: unknown scope '{scope_str}' (expected user|lore|pref)"),
        };
        if !is_write_allowed(ctx.speaker_role, &scope, ctx.speaker_id) {
            return format!(
                "Error: not authorized to save {} for subject={:?} — speaker role is {:?}. \
                 Regular users may write User/Pref only with subject_id == speaker_id. \
                 Prefs are always self-only. Lore is moderator/broadcaster-only.",
                scope.tag(),
                scope.subject_id(),
                ctx.speaker_role
            );
        }

        let key = build_key(&scope, slug);
        let level = trust_level_for(ctx.speaker_role, &scope, ctx.speaker_id);
        let seed_conf = seed_confidence(level);
        let now = ctx.now;

        if let Some(existing) = self.memories.get_mut(&key) {
            existing.fact = fact.to_string();
            existing.updated_at = now;
            if !existing.sources.iter().any(|s| s == ctx.speaker_username) {
                existing.sources.push(ctx.speaker_username.to_string());
            }
            return format!("Updated memory '{key}'");
        }

        let cap = match &scope {
            Scope::User { .. } => ctx.caps.max_user,
            Scope::Lore => ctx.caps.max_lore,
            Scope::Pref { .. } => ctx.caps.max_pref,
        };
        let count = self.count_scope(scope.tag());
        if count >= cap {
            if let Some(evicted) = self.evict_lowest_in_scope(scope.tag(), now, ctx.half_life_days)
            {
                info!(%evicted, "Evicted to make room");
            } else {
                return format!(
                    "Memory full ({count}/{cap}) and no evictable entry in scope {}",
                    scope.tag()
                );
            }
        }

        self.memories.insert(
            key.clone(),
            Memory::new(
                fact.to_string(),
                scope,
                ctx.speaker_username.to_string(),
                seed_conf,
                now,
            ),
        );
        format!("Saved memory '{key}' (confidence {seed_conf})")
    }

    fn count_scope(&self, tag: &str) -> usize {
        self.memories
            .values()
            .filter(|m| m.scope.tag() == tag)
            .count()
    }

    fn handle_get_memories(&self, call: &ToolCall, _ctx: &DispatchContext<'_>) -> String {
        let scope_str = call
            .arguments
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let subject_id = call.arguments.get("subject_id").and_then(|v| v.as_str());
        let mut out: Vec<String> = self
            .memories
            .iter()
            .filter(|(_, m)| {
                m.scope.tag() == scope_str
                    && match subject_id {
                        Some(s) => m.scope.subject_id() == Some(s),
                        None => true,
                    }
            })
            .map(|(k, m)| {
                format!(
                    "- {}: {} (confidence={}, sources={:?})",
                    k, m.fact, m.confidence, m.sources
                )
            })
            .collect();
        out.sort();
        if out.is_empty() {
            "(none)".into()
        } else {
            out.join("\n")
        }
    }

    /// Legacy dispatcher kept alive for the in-file `run_memory_extraction`
    /// path until Phase E rewrites that module. Preserves pre-Phase-C behavior
    /// so existing extraction wiring keeps compiling.
    #[allow(dead_code)] // Removed in Phase E once run_memory_extraction is rewritten.
    fn legacy_execute_tool_call(&mut self, call: &ToolCall, max_memories: usize) -> String {
        if let Some(err) = &call.arguments_parse_error {
            return format!(
                "Error: tool '{name}' arguments were not valid JSON ({error}). \
                 Raw text: {raw}. Resend with a valid JSON object.",
                name = call.name,
                error = err.error,
                raw = err.raw,
            );
        }
        match call.name.as_str() {
            "save_memory" => {
                let key = call.arguments.get("key").and_then(|v| v.as_str());
                let fact = call.arguments.get("fact").and_then(|v| v.as_str());
                match (key, fact) {
                    (Some(key), Some(fact)) => {
                        let now = chrono::Utc::now();
                        if self.memories.contains_key(key) {
                            let mem = self.memories.get_mut(key).unwrap();
                            mem.fact = fact.to_string();
                            mem.updated_at = now;
                            format!("Updated memory '{}'", key)
                        } else if self.memories.len() >= max_memories {
                            format!(
                                "Memory full ({}/{}) — delete a memory first",
                                self.memories.len(),
                                max_memories
                            )
                        } else {
                            self.memories.insert(
                                key.to_string(),
                                Memory::new(
                                    fact.to_string(),
                                    Scope::Lore,
                                    "legacy".to_string(),
                                    70,
                                    now,
                                ),
                            );
                            format!("Saved memory '{}'", key)
                        }
                    }
                    _ => "Error: save_memory requires 'key' and 'fact' parameters".to_string(),
                }
            }
            "delete_memory" => {
                let key = call.arguments.get("key").and_then(|v| v.as_str());
                match key {
                    Some(key) => {
                        if self.memories.remove(key).is_some() {
                            format!("Deleted memory '{}'", key)
                        } else {
                            format!("No memory with key '{}'", key)
                        }
                    }
                    None => "Error: delete_memory requires 'key' parameter".to_string(),
                }
            }
            other => format!("Unknown tool: {}", other),
        }
    }
}

/// Per-scope maximum memory counts. Enforced by the extractor dispatcher.
#[derive(Debug, Clone)]
pub struct Caps {
    pub max_user: usize,
    pub max_lore: usize,
    pub max_pref: usize,
}

/// Context threaded through the extractor dispatcher: who's speaking, their
/// role, the caps that apply, and the clock.
pub struct DispatchContext<'a> {
    pub speaker_id: &'a str,
    pub speaker_username: &'a str,
    pub speaker_role: UserRole,
    pub caps: Caps,
    pub half_life_days: u32,
    pub now: DateTime<Utc>,
}

/// Turn a human-readable label into a lowercase ASCII slug. Runs of
/// non-alphanumeric characters collapse into a single `-`; leading and
/// trailing dashes are trimmed. Non-ASCII input (e.g. emoji) is dropped
/// the same way.
pub(crate) fn sanitize_slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = true; // suppress leading dashes
    for c in s.chars() {
        let norm = c.to_ascii_lowercase();
        if norm.is_ascii_alphanumeric() {
            out.push(norm);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Compose the canonical key for a memory: `user:<uid>:<slug>`, `lore::<slug>`,
/// or `pref:<uid>:<slug>`. The slug is sanitized before composition.
pub(crate) fn build_key(scope: &Scope, slug: &str) -> String {
    let slug = sanitize_slug(slug);
    match scope {
        Scope::User { subject_id } => format!("user:{}:{}", subject_id, slug),
        Scope::Lore => format!("lore::{}", slug),
        Scope::Pref { subject_id } => format!("pref:{}:{}", subject_id, slug),
    }
}

// Legacy per-turn tool set. Kept module-private to feed `run_memory_extraction`
// until Phase E rewrites that path against `extractor_tools()` in `tools.rs`.
fn memory_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "save_memory".to_string(),
            description: "Save or update a fact. Use a short descriptive slug as the key (e.g. 'chrono-favorite-game'). If the key already exists, the fact is overwritten.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Short slug identifier for this memory"
                    },
                    "fact": {
                        "type": "string",
                        "description": "The fact to remember"
                    }
                },
                "required": ["key", "fact"]
            }),
        },
        ToolDefinition {
            name: "delete_memory".to_string(),
            description: "Delete a stored memory by its key.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "The key of the memory to delete"
                    }
                },
                "required": ["key"]
            }),
        },
    ]
}

/// Spawn a fire-and-forget task that asks the LLM to extract memories.
/// Errors are logged and swallowed — never affects the user-facing response.
pub fn spawn_memory_extraction(
    llm_client: Arc<dyn llm::LlmClient>,
    model: String,
    mem: &MemoryConfig,
    username: String,
    user_message: String,
    ai_response: String,
    timeout: std::time::Duration,
) {
    let store = mem.store.clone();
    let store_path = mem.path.clone();
    let max_memories = mem.max_memories;
    tokio::spawn(async move {
        if let Err(e) = run_memory_extraction(
            &*llm_client,
            &model,
            &store,
            &store_path,
            max_memories,
            (&username, &user_message, &ai_response),
            timeout,
        )
        .await
        {
            debug!("Memory extraction failed (non-critical): {:#}", e);
        }
    });
}

async fn run_memory_extraction(
    llm_client: &dyn llm::LlmClient,
    model: &str,
    store: &RwLock<MemoryStore>,
    store_path: &Path,
    max_memories: usize,
    conversation: (&str, &str, &str), // (username, user_message, ai_response)
    timeout: std::time::Duration,
) -> Result<()> {
    let (username, user_message, ai_response) = conversation;

    let current_memories = {
        let store_guard = store.read().await;
        store_guard.format_for_extraction()
    };

    let user_content = format!(
        "Current memories:\n{}\n\nConversation:\nUser ({}): {}\nAssistant: {}",
        current_memories, username, user_message, ai_response
    );

    let tools = memory_tool_definitions();
    let messages = vec![
        Message {
            role: "system".to_string(),
            content: EXTRACTION_SYSTEM_PROMPT.to_string(),
        },
        Message {
            role: "user".to_string(),
            content: user_content,
        },
    ];
    let mut prior_rounds: Vec<ToolCallRound> = Vec::new();

    for round in 0..MAX_EXTRACTION_ROUNDS {
        let request = ToolChatCompletionRequest {
            model: model.to_string(),
            messages: messages.clone(),
            tools: tools.clone(),
            prior_rounds: prior_rounds.clone(),
        };

        let response =
            tokio::time::timeout(timeout, llm_client.chat_completion_with_tools(request))
                .await
                .wrap_err("Memory extraction timed out")?
                .wrap_err("Memory extraction LLM call failed")?;

        match response {
            ToolChatCompletionResponse::Message(_) => {
                debug!(round, "Memory extraction finished (text response)");
                break;
            }
            ToolChatCompletionResponse::ToolCalls(calls) => {
                debug!(
                    round,
                    count = calls.len(),
                    "Memory extraction: processing tool calls"
                );
                let mut store_guard = store.write().await;
                let mut results: Vec<ToolResultMessage> = Vec::with_capacity(calls.len());
                for call in &calls {
                    let result = store_guard.legacy_execute_tool_call(call, max_memories);
                    info!(tool = %call.name, key = %call.arguments.get("key").and_then(|v| v.as_str()).unwrap_or("?"), result = %result, "Memory tool executed");
                    results.push(ToolResultMessage {
                        tool_call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        content: result,
                    });
                }
                // Persist after each round of tool calls
                store_guard.save(store_path)?;
                prior_rounds.push(ToolCallRound { calls, results });
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_store() -> MemoryStore {
        MemoryStore::default()
    }

    #[test]
    fn memory_new_seeds_all_fields() {
        use chrono::Utc;
        let mem = Memory::new(
            "alice plays Tarkov".to_string(),
            Scope::User {
                subject_id: "1".to_string(),
            },
            "alice".to_string(),
            70,
            Utc::now(),
        );
        assert_eq!(mem.confidence, 70);
        assert_eq!(mem.sources, vec!["alice".to_string()]);
        assert_eq!(mem.access_count, 0);
        assert_eq!(mem.created_at, mem.updated_at);
        assert_eq!(mem.created_at, mem.last_accessed);
    }

    fn mem_at(confidence: u8, last_accessed_days_ago: i64, access_count: u32) -> Memory {
        use chrono::{Duration, Utc};
        let now = Utc::now();
        Memory {
            fact: "f".to_string(),
            scope: Scope::Lore,
            sources: vec!["x".to_string()],
            confidence,
            created_at: now,
            updated_at: now,
            last_accessed: now - Duration::days(last_accessed_days_ago),
            access_count,
        }
    }

    #[test]
    fn score_monotone_in_confidence() {
        use chrono::Utc;
        let a = mem_at(90, 0, 0);
        let b = mem_at(50, 0, 0);
        assert!(a.score(Utc::now(), 30) > b.score(Utc::now(), 30));
    }

    #[test]
    fn score_decays_with_age() {
        use chrono::Utc;
        let fresh = mem_at(70, 0, 0);
        let stale = mem_at(70, 60, 0);
        assert!(fresh.score(Utc::now(), 30) > stale.score(Utc::now(), 30));
    }

    #[test]
    fn score_boosts_with_access_count() {
        use chrono::Utc;
        let cold = mem_at(70, 0, 0);
        let hot = mem_at(70, 0, 20);
        assert!(hot.score(Utc::now(), 30) > cold.score(Utc::now(), 30));
    }

    #[test]
    fn sanitize_slug_basic() {
        assert_eq!(sanitize_slug("Favorite Game"), "favorite-game");
        assert_eq!(sanitize_slug("alice's cat!!"), "alice-s-cat");
        assert_eq!(sanitize_slug("---weird---"), "weird");
        assert_eq!(sanitize_slug("emoji 🙂 drop"), "emoji-drop");
    }

    #[test]
    fn build_key_per_scope() {
        assert_eq!(
            build_key(
                &Scope::User {
                    subject_id: "42".into()
                },
                "plays-tarkov"
            ),
            "user:42:plays-tarkov"
        );
        assert_eq!(
            build_key(&Scope::Lore, "channel-emote"),
            "lore::channel-emote"
        );
        assert_eq!(
            build_key(
                &Scope::Pref {
                    subject_id: "42".into()
                },
                "speaks-german"
            ),
            "pref:42:speaks-german"
        );
    }

    #[test]
    fn store_default_is_v2_and_empty() {
        let s = MemoryStore::default();
        assert_eq!(s.version, 2);
        assert!(s.memories.is_empty());
        assert!(s.identities.is_empty());
    }

    #[test]
    fn store_save_load_round_trip() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ai_memory.ron");
        let s = MemoryStore {
            version: 2,
            memories: HashMap::new(),
            identities: HashMap::new(),
        };
        s.save(&path).unwrap();
        let (loaded, _) = MemoryStore::load(dir.path()).unwrap();
        assert_eq!(loaded.version, 2);
    }

    #[test]
    fn identity_round_trip_ron() {
        use chrono::Utc;
        let id = Identity {
            username: "alice".to_string(),
            updated_at: Utc::now(),
        };
        let s = ron::ser::to_string_pretty(&id, ron::ser::PrettyConfig::default()).unwrap();
        let back: Identity = ron::from_str(&s).unwrap();
        assert_eq!(back.username, id.username);
    }

    #[test]
    fn upsert_identity_new() {
        let mut s = MemoryStore::default();
        let now = Utc::now();
        s.upsert_identity("42", "alice", now);
        assert_eq!(s.identities.get("42").unwrap().username, "alice");
        assert_eq!(s.identities.get("42").unwrap().updated_at, now);
    }

    #[test]
    fn upsert_identity_rename_overwrites() {
        let mut s = MemoryStore::default();
        let now = Utc::now();
        s.upsert_identity("42", "alice", now);
        s.upsert_identity("42", "alicette", now);
        assert_eq!(s.identities.get("42").unwrap().username, "alicette");
        assert_eq!(s.identities.len(), 1);
    }

    #[test]
    fn evict_tie_breaks_by_older_last_accessed() {
        use chrono::{Duration, Utc};
        let now = Utc::now();
        let mut s = MemoryStore::default();
        // identical score inputs, different last_accessed
        let mut m_old = mem_at(70, 5, 0);
        m_old.last_accessed = now - Duration::days(5);
        let mut m_new = mem_at(70, 5, 0);
        m_new.last_accessed = now - Duration::days(3);
        s.memories.insert("lore::old".into(), m_old);
        s.memories.insert("lore::new".into(), m_new);
        let evicted = s.evict_lowest_in_scope("lore", now, 30).unwrap();
        assert_eq!(evicted, "lore::old");
    }

    fn ctx_for(speaker_id: &'static str, role: UserRole) -> DispatchContext<'static> {
        // Used by dispatcher tests that don't care about speaker_username or
        // cap exhaustion; defaults are spacious.
        DispatchContext {
            speaker_id,
            speaker_username: speaker_id,
            speaker_role: role,
            caps: Caps {
                max_user: 50,
                max_lore: 50,
                max_pref: 50,
            },
            half_life_days: 30,
            now: Utc::now(),
        }
    }

    #[test]
    fn execute_tool_call_surfaces_parse_error() {
        let mut store = empty_store();
        let call = ToolCall {
            id: "c1".to_string(),
            name: "save_memory".to_string(),
            arguments: serde_json::Value::Null,
            arguments_parse_error: Some(llm::ToolCallArgsError {
                error: "expected `,` or `}` at line 1 column 17".to_string(),
                raw: "{\"slug\":\"k\" \"fact\":\"f\"}".to_string(),
            }),
        };
        let ctx = ctx_for("42", UserRole::Regular);
        let result = store.execute_tool_call(&call, &ctx);
        assert!(result.contains("not valid JSON"), "got: {result}");
        assert!(result.contains("save_memory"), "got: {result}");
        assert!(result.contains("{\"slug\":\"k\""), "got: {result}");
        assert!(store.memories.is_empty());
    }

    #[test]
    fn execute_tool_call_missing_args_is_distinct_from_parse_error() {
        let mut store = empty_store();
        let call = ToolCall {
            id: "c2".to_string(),
            name: "save_memory".to_string(),
            arguments: serde_json::Value::Null,
            arguments_parse_error: None,
        };
        let ctx = ctx_for("42", UserRole::Regular);
        let result = store.execute_tool_call(&call, &ctx);
        assert!(
            result.contains("requires non-empty 'slug' and 'fact'"),
            "got: {result}"
        );
        assert!(!result.contains("not valid JSON"), "got: {result}");
    }

    #[test]
    fn save_memory_self_claim_creates_user_scope() {
        let mut store = MemoryStore::default();
        let call = ToolCall {
            id: "c1".into(),
            name: "save_memory".into(),
            arguments: serde_json::json!({
                "scope": "user",
                "subject_id": "42",
                "slug": "plays-tarkov",
                "fact": "alice plays tarkov",
            }),
            arguments_parse_error: None,
        };
        let ctx = DispatchContext {
            speaker_id: "42",
            speaker_username: "alice",
            speaker_role: UserRole::Regular,
            caps: Caps {
                max_user: 50,
                max_lore: 50,
                max_pref: 50,
            },
            half_life_days: 30,
            now: Utc::now(),
        };
        let out = store.execute_tool_call(&call, &ctx);
        assert!(out.contains("Saved"), "got: {out}");
        assert!(store.memories.contains_key("user:42:plays-tarkov"));
    }

    #[test]
    fn save_memory_third_party_rejected() {
        let mut store = MemoryStore::default();
        let call = ToolCall {
            id: "c1".into(),
            name: "save_memory".into(),
            arguments: serde_json::json!({
                "scope": "user",
                "subject_id": "99",
                "slug": "drinks-coffee",
                "fact": "bob drinks coffee",
            }),
            arguments_parse_error: None,
        };
        let ctx = DispatchContext {
            speaker_id: "42", // != subject_id 99
            speaker_username: "alice",
            speaker_role: UserRole::Regular,
            caps: Caps {
                max_user: 50,
                max_lore: 50,
                max_pref: 50,
            },
            half_life_days: 30,
            now: Utc::now(),
        };
        let out = store.execute_tool_call(&call, &ctx);
        assert!(out.contains("not authorized"), "got: {out}");
        assert!(store.memories.is_empty());
    }

    #[test]
    fn save_memory_pref_self_only_even_for_mod() {
        let mut store = MemoryStore::default();
        let call = ToolCall {
            id: "c1".into(),
            name: "save_memory".into(),
            arguments: serde_json::json!({
                "scope": "pref",
                "subject_id": "99",
                "slug": "language",
                "fact": "speaks German",
            }),
            arguments_parse_error: None,
        };
        let ctx = DispatchContext {
            speaker_id: "42",
            speaker_username: "modguy",
            speaker_role: UserRole::Moderator,
            caps: Caps {
                max_user: 50,
                max_lore: 50,
                max_pref: 50,
            },
            half_life_days: 30,
            now: Utc::now(),
        };
        let out = store.execute_tool_call(&call, &ctx);
        assert!(out.contains("not authorized"), "got: {out}");
    }

    #[test]
    fn save_memory_collision_same_speaker_keeps_single_source() {
        // Invariant under test: re-saving the same (scope, subject_id, slug)
        // from the same speaker must not duplicate `sources`. The plan's
        // original test also covered a second-speaker append, but that half
        // requires Task 10's identity wiring; kept simple and focused here.
        use chrono::Duration;
        let now = Utc::now();
        let mut store = MemoryStore::default();
        store.memories.insert(
            "user:42:plays-tarkov".into(),
            Memory::new(
                "alice plays tarkov".into(),
                Scope::User {
                    subject_id: "42".into(),
                },
                "alice".into(),
                70,
                now - Duration::minutes(5),
            ),
        );
        let call = ToolCall {
            id: "c1".into(),
            name: "save_memory".into(),
            arguments: serde_json::json!({
                "scope": "user",
                "subject_id": "42",
                "slug": "plays-tarkov",
                "fact": "alice plays tarkov on wipes",
            }),
            arguments_parse_error: None,
        };
        let ctx = DispatchContext {
            speaker_id: "42",
            speaker_username: "alice",
            speaker_role: UserRole::Regular,
            caps: Caps {
                max_user: 50,
                max_lore: 50,
                max_pref: 50,
            },
            half_life_days: 30,
            now,
        };
        let _ = store.execute_tool_call(&call, &ctx);
        let mem = store.memories.get("user:42:plays-tarkov").unwrap();
        assert_eq!(mem.sources, vec!["alice".to_string()]);
        assert_eq!(mem.fact, "alice plays tarkov on wipes");
    }

    #[test]
    fn save_memory_collision_appends_new_speaker_source() {
        // Companion to the single-speaker test: a second, distinct speaker
        // corroborating the same key must append to `sources`.
        let now = Utc::now();
        let mut store = MemoryStore::default();
        store.memories.insert(
            "user:42:plays-tarkov".into(),
            Memory::new(
                "alice plays tarkov".into(),
                Scope::User {
                    subject_id: "42".into(),
                },
                "alice".into(),
                70,
                now,
            ),
        );
        let call = ToolCall {
            id: "c2".into(),
            name: "save_memory".into(),
            arguments: serde_json::json!({
                "scope": "user",
                "subject_id": "42",
                "slug": "plays-tarkov",
                "fact": "alice plays tarkov",
            }),
            arguments_parse_error: None,
        };
        let ctx = DispatchContext {
            speaker_id: "100",
            speaker_username: "modguy",
            speaker_role: UserRole::Moderator,
            caps: Caps {
                max_user: 50,
                max_lore: 50,
                max_pref: 50,
            },
            half_life_days: 30,
            now,
        };
        let _ = store.execute_tool_call(&call, &ctx);
        let mem = store.memories.get("user:42:plays-tarkov").unwrap();
        assert_eq!(mem.sources, vec!["alice".to_string(), "modguy".to_string()]);
    }

    #[test]
    fn get_memories_filters_by_scope_and_subject() {
        let now = Utc::now();
        let mut s = MemoryStore::default();
        s.memories.insert(
            "user:1:a".into(),
            Memory::new(
                "alice fact".into(),
                Scope::User {
                    subject_id: "1".into(),
                },
                "alice".into(),
                70,
                now,
            ),
        );
        s.memories.insert(
            "user:2:b".into(),
            Memory::new(
                "bob fact".into(),
                Scope::User {
                    subject_id: "2".into(),
                },
                "bob".into(),
                70,
                now,
            ),
        );
        s.memories.insert(
            "lore::c".into(),
            Memory::new("channel fact".into(), Scope::Lore, "mod".into(), 90, now),
        );
        let call = ToolCall {
            id: "g".into(),
            name: "get_memories".into(),
            arguments: serde_json::json!({"scope": "user", "subject_id": "1"}),
            arguments_parse_error: None,
        };
        let ctx = ctx_for("1", UserRole::Regular);
        let out = s.execute_tool_call(&call, &ctx);
        assert!(out.contains("user:1:a"), "got: {out}");
        assert!(!out.contains("user:2:b"), "got: {out}");
        assert!(!out.contains("lore::c"), "got: {out}");
    }
}
