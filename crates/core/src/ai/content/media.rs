//! Multimodal sub-agent client for `read_url`. Posts an OpenAI-compatible
//! chat completion with content parts; returns the assistant's text answer.
//!
//! Why isolated here: the multimodal request schema (content parts, image_url
//! data URLs) is provider-coupled. Keeping it out of the shared `llm` trait
//! avoids forcing every provider implementation to grow content-part variants
//! for a feature only this consumer needs.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use llm::TraceIds;
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Value, json};
use tracing::{Span, field::Empty, instrument};

use crate::ai::content::client::Payload;
use crate::util::http::SendTraced as _;

/// Why a media analysis call failed. `Display` is for logs only.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("media chat-completions request timed out")]
    Timeout(#[source] reqwest::Error),
    #[error("media chat-completions request failed")]
    Upstream(#[source] reqwest::Error),
    #[error("media response missing assistant content")]
    EmptyAnswer,
}

impl From<reqwest::Error> for MediaError {
    fn from(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout(error)
        } else {
            Self::Upstream(error)
        }
    }
}

impl MediaError {
    /// Variant name, recorded as `error.type`.
    fn variant_name(&self) -> &'static str {
        match self {
            Self::Timeout(_) => "Timeout",
            Self::Upstream(_) => "Upstream",
            Self::EmptyAnswer => "EmptyAnswer",
        }
    }
}

// German: the answer is fed straight back into the bot's German turn, so an
// English one would force a translation round the model does not always make.
const SYSTEM_PROMPT: &str = "Du analysierst URLs für einen Twitch-Chatbot. \
Beantworte die Anweisung ausschließlich aus dem gelieferten Inhalt. Fass dich kurz. \
Ohne Anweisung beschreibst du den Inhalt. Antworte auf Deutsch.";

pub struct MediaClient {
    http: reqwest::Client,
    /// `AiProvider` name of the profile, recorded as `gen_ai.provider.name`.
    provider_name: &'static str,
    base_url: String,
    api_key: Option<SecretString>,
    model: String,
    timeout: Duration,
}

impl MediaClient {
    pub fn new(
        http: reqwest::Client,
        provider_name: &'static str,
        base_url: String,
        api_key: Option<SecretString>,
        model: String,
        timeout: Duration,
    ) -> Self {
        Self {
            http,
            provider_name,
            base_url,
            api_key,
            model,
            timeout,
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[instrument(
        skip_all,
        name = "chat",
        fields(
            gen_ai.operation.name = "chat",
            gen_ai.provider.name = self.provider_name,
            gen_ai.request.model = %self.model,
            gen_ai.usage.input_tokens = Empty,
            gen_ai.usage.output_tokens = Empty,
            error.type = Empty,
            otel.status_code = Empty,
        ),
    )]
    pub async fn analyze(
        &self,
        content_type: &str,
        payload: &Payload,
        instruction: Option<&str>,
        trace: &TraceIds,
    ) -> Result<String, MediaError> {
        let result = self
            .request_answer(content_type, payload, instruction, trace)
            .await;
        if let Err(error) = &result {
            let span = Span::current();
            span.record("error.type", error.variant_name());
            span.record("otel.status_code", "ERROR");
        }
        result
    }

    async fn request_answer(
        &self,
        content_type: &str,
        payload: &Payload,
        instruction: Option<&str>,
        trace: &TraceIds,
    ) -> Result<String, MediaError> {
        let body = self.build_request(content_type, payload, instruction, trace);

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut req = self.http.post(&url).timeout(self.timeout).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key.expose_secret());
        }
        let response = req.send_traced().await?.error_for_status()?;

        let value: Value = response.json().await?;

        if let Some(usage) = value.get("usage") {
            let span = Span::current();
            if let Some(n) = usage.get("prompt_tokens").and_then(Value::as_u64) {
                span.record("gen_ai.usage.input_tokens", n);
            }
            if let Some(n) = usage.get("completion_tokens").and_then(Value::as_u64) {
                span.record("gen_ai.usage.output_tokens", n);
            }
        }

        let answer = value
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(str::to_string);

        match answer {
            Some(s) if !s.trim().is_empty() => Ok(s),
            _ => Err(MediaError::EmptyAnswer),
        }
    }

    fn build_request(
        &self,
        content_type: &str,
        payload: &Payload,
        instruction: Option<&str>,
        trace: &TraceIds,
    ) -> Value {
        let prompt = instruction
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Describe the contents.");

        let media_part = match payload {
            Payload::Text(t) => json!({ "type": "text", "text": t }),
            Payload::Bytes(b) => {
                let prefix = "data:";
                let mid = ";base64,";
                let mut data_url = String::with_capacity(
                    prefix.len() + content_type.len() + mid.len() + (b.len() * 4 / 3) + 4,
                );
                data_url.push_str(prefix);
                data_url.push_str(content_type);
                data_url.push_str(mid);
                BASE64.encode_string(b, &mut data_url);
                json!({ "type": "image_url", "image_url": { "url": data_url } })
            }
        };

        let mut body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": SYSTEM_PROMPT },
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": prompt },
                        media_part,
                    ],
                },
            ],
        });
        if let Some(user) = &trace.user {
            body["user"] = Value::String(user.clone());
        }
        if let Some(session_id) = &trace.session_id {
            body["session_id"] = Value::String(session_id.clone());
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> MediaClient {
        crate::install_crypto_provider();
        MediaClient::new(
            reqwest::Client::new(),
            "open_ai_compatible",
            "https://example.invalid/v1".to_string(),
            None,
            "test-model".to_string(),
            Duration::from_secs(1),
        )
    }

    #[test]
    fn build_request_image_uses_data_url() {
        let c = client();
        let req = c.build_request(
            "image/png",
            &Payload::Bytes(vec![1, 2, 3]),
            Some("what is shown?"),
            &TraceIds::default(),
        );
        let parts = &req["messages"][1]["content"];
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "what is shown?");
        assert_eq!(parts[1]["type"], "image_url");
        let url = parts[1]["image_url"]["url"].as_str().expect("url");
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
    }

    #[test]
    fn build_request_text_uses_inline_text_part() {
        let c = client();
        let req = c.build_request(
            "text/html",
            &Payload::Text("Hello".into()),
            None,
            &TraceIds::default(),
        );
        assert_eq!(req["messages"][0]["role"], "system");
        let parts = &req["messages"][1]["content"];
        assert_eq!(parts[0]["text"], "Describe the contents.");
        assert_eq!(parts[1]["type"], "text");
        assert_eq!(parts[1]["text"], "Hello");
    }

    #[test]
    fn build_request_emits_trace_ids_when_set() {
        let c = client();
        let trace = TraceIds {
            user: Some("nikolai".into()),
            session_id: Some("turn-9".into()),
        };
        let req = c.build_request("text/plain", &Payload::Text("x".into()), None, &trace);
        assert_eq!(req["user"], "nikolai");
        assert_eq!(req["session_id"], "turn-9");
    }

    #[test]
    fn build_request_omits_trace_ids_when_default() {
        let c = client();
        let req = c.build_request(
            "text/plain",
            &Payload::Text("x".into()),
            None,
            &TraceIds::default(),
        );
        assert!(req.get("user").is_none());
        assert!(req.get("session_id").is_none());
    }

    #[tokio::test]
    async fn analyze_extracts_choices_message_content() {
        crate::install_crypto_provider();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": { "role": "assistant", "content": "It is a cat." }
                    }]
                })),
            )
            .mount(&server)
            .await;

        let c = MediaClient::new(
            reqwest::Client::new(),
            "open_ai_compatible",
            format!("{}/v1", server.uri()),
            None,
            "x".into(),
            Duration::from_secs(2),
        );
        let answer = c
            .analyze(
                "image/png",
                &Payload::Bytes(vec![1, 2]),
                Some("what?"),
                &TraceIds::default(),
            )
            .await
            .expect("ok");
        assert_eq!(answer, "It is a cat.");
    }

    #[tokio::test]
    async fn analyze_surfaces_provider_error() {
        crate::install_crypto_provider();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let c = MediaClient::new(
            reqwest::Client::new(),
            "open_ai_compatible",
            format!("{}/v1", server.uri()),
            None,
            "x".into(),
            Duration::from_secs(2),
        );
        let result = c
            .analyze(
                "text/plain",
                &Payload::Text("x".into()),
                None,
                &TraceIds::default(),
            )
            .await;
        assert!(result.is_err(), "a 5xx must surface as an error");
    }
}
