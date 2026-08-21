//! Selects and constructs an LLM backend based on bot configuration.

use std::sync::Arc;

use eyre::Result;
use llm::{LlmClient, OllamaClient, OpenAiClient};
use secrecy::ExposeSecret as _;
use tracing::{debug, error, info};

use crate::APP_USER_AGENT;
use crate::config::AiBootstrap;
use crate::settings::Settings;

/// Build an [`LlmClient`] from the AI bootstrap + the current settings snapshot.
///
/// Returns `Ok(None)` when no AI bootstrap is provided. The connection knobs
/// (provider, base_url, model) come from the active profile in the dashboard
/// settings.
///
/// Returns `Ok(Some(client))` when the client is successfully built.
/// Returns `Err` only when the configuration is invalid (not when the backend
/// is unreachable).
pub fn build_llm_client(
    ai_bootstrap: Option<&AiBootstrap>,
    settings: &Settings,
) -> Result<Option<Arc<dyn LlmClient>>> {
    let Some(ai_boot) = ai_bootstrap else {
        debug!("AI not configured, AI command disabled");
        return Ok(None);
    };

    let conn = settings.ai.active();
    if conn.model.trim().is_empty() {
        error!("AI model is empty in settings, AI disabled (set it at /settings -> AI · Profiles)");
        return Ok(None);
    }
    let provider = conn.provider;
    let result = if provider.uses_openai_client() {
        OpenAiClient::new(
            ai_boot.api_key.expose_secret(),
            conn.base_url.as_deref(),
            APP_USER_AGENT,
            provider.is_openrouter(),
        )
        .map(|c| Arc::new(c) as Arc<dyn LlmClient>)
    } else {
        OllamaClient::new(conn.base_url.as_deref(), APP_USER_AGENT)
            .map(|c| Arc::new(c) as Arc<dyn LlmClient>)
    };

    match result {
        Ok(client) => {
            info!(provider = conn.provider.as_str(), model = %conn.model, "LLM client initialized");
            Ok(Some(client))
        }
        Err(e) => {
            error!(error = ?e, "Failed to initialize LLM client");
            Ok(None)
        }
    }
}
