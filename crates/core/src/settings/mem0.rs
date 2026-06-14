//! mem0 memory settings. `api_key` stays in config.toml as a secret; the
//! dashboard manages the rest. `base_url` is restart-required (client built
//! once at startup).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Mem0Settings {
    pub enabled: bool,
    pub base_url: String,
    pub timeout_secs: u64,
    pub search_limit: usize,
}

impl Default for Mem0Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "http://mem0:8888".to_owned(),
            timeout_secs: 10,
            search_limit: 8,
        }
    }
}
