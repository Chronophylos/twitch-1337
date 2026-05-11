//! Error type for the `llm` crate.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, LlmError>;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP transport error")]
    Http(#[from] reqwest::Error),

    #[error("invalid header value")]
    Header(#[from] reqwest::header::InvalidHeaderValue),

    #[error("provider returned status {status}: {body}")]
    Provider { status: u16, body: String },

    #[error("failed to decode response ({stage})")]
    Decode {
        stage: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("provider returned an empty response")]
    EmptyResponse,
}
