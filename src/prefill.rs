use serde::{Deserialize, Serialize};

fn default_base_url() -> String {
    "https://logs.zonian.dev".to_string()
}

fn default_threshold() -> f64 {
    0.5
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HistoryPrefillConfig {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_threshold")]
    pub threshold: f64,
}
