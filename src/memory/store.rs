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
use crate::memory::Scope;

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

    /// Execute a single tool call against the store. Returns a result message.
    pub fn execute_tool_call(&mut self, call: &ToolCall, max_memories: usize) -> String {
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
                            // Update existing
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
                            // Create new. NOTE: this legacy dispatch is replaced
                            // by the scope-aware dispatcher in Task 9; fields
                            // below are interim placeholders so the build works.
                            self.memories.insert(
                                key.to_string(),
                                Memory::new(fact.to_string(), Scope::Lore, String::new(), 70, now),
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

/// Turn a human-readable label into a lowercase ASCII slug. Runs of
/// non-alphanumeric characters collapse into a single `-`; leading and
/// trailing dashes are trimmed. Non-ASCII input (e.g. emoji) is dropped
/// the same way.
// Dispatcher (Task 9) is the first non-test caller; allowed until then.
#[allow(dead_code)]
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
// Dispatcher (Task 9) is the first non-test caller; allowed until then.
#[allow(dead_code)]
pub(crate) fn build_key(scope: &Scope, slug: &str) -> String {
    let slug = sanitize_slug(slug);
    match scope {
        Scope::User { subject_id } => format!("user:{}:{}", subject_id, slug),
        Scope::Lore => format!("lore::{}", slug),
        Scope::Pref { subject_id } => format!("pref:{}:{}", subject_id, slug),
    }
}

pub fn memory_tool_definitions() -> Vec<ToolDefinition> {
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
                    let result = store_guard.execute_tool_call(call, max_memories);
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
    fn execute_tool_call_surfaces_parse_error() {
        let mut store = empty_store();
        let call = ToolCall {
            id: "c1".to_string(),
            name: "save_memory".to_string(),
            arguments: serde_json::Value::Null,
            arguments_parse_error: Some(llm::ToolCallArgsError {
                error: "expected `,` or `}` at line 1 column 17".to_string(),
                raw: "{\"key\":\"k\" \"fact\":\"f\"}".to_string(),
            }),
        };
        let result = store.execute_tool_call(&call, 50);
        assert!(result.contains("not valid JSON"), "got: {result}");
        assert!(result.contains("save_memory"), "got: {result}");
        assert!(result.contains("{\"key\":\"k\""), "got: {result}");
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
        let result = store.execute_tool_call(&call, 50);
        assert!(
            result.contains("requires 'key' and 'fact'"),
            "got: {result}"
        );
        assert!(!result.contains("not valid JSON"), "got: {result}");
    }
}
