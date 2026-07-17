//! OpenRouter Image API client.

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use reqwest::header::{self, HeaderValue};
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument, warn};

use crate::error::{LlmError, Result};
use crate::util::truncate_for_echo;

// --- Public types ---

/// Request to generate an image via the OpenRouter Image API.
#[derive(Debug, Clone)]
pub struct ImageGenerateRequest {
    pub model: String,
    pub prompt: String,
    pub input_image_urls: Vec<String>,
    pub aspect_ratio: Option<String>,
}

/// Decoded image bytes returned by the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

/// Provider-agnostic image generation client.
#[async_trait]
pub trait ImageClient: Send + Sync {
    async fn generate(&self, request: ImageGenerateRequest) -> Result<GeneratedImage>;
}

// --- Internal serde types ---

#[derive(Debug, Serialize)]
struct ApiImageReference {
    r#type: &'static str,
    image_url: ApiImageUrl,
}

#[derive(Debug, Serialize)]
struct ApiImageUrl {
    url: String,
}

#[derive(Debug, Serialize)]
struct ApiImageRequest {
    model: String,
    prompt: String,
    n: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    aspect_ratio: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    input_references: Vec<ApiImageReference>,
}

#[derive(Debug, Deserialize)]
struct ApiImageData {
    b64_json: String,
    #[serde(default)]
    media_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiImageResponse {
    data: Vec<ApiImageData>,
}

// --- Helpers ---

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Parse a successful `/images` JSON body into decoded bytes.
fn parse_images_response(body: &serde_json::Value) -> Result<GeneratedImage> {
    if let Some(msg) = extract_api_error(body) {
        return Err(LlmError::Provider {
            status: 200,
            body: msg,
        });
    }

    let api: ApiImageResponse =
        serde_json::from_value(body.clone()).map_err(|source| LlmError::Decode {
            stage: "images response",
            source,
        })?;

    let item = api.data.into_iter().next().ok_or(LlmError::EmptyResponse)?;

    let bytes = BASE64
        .decode(&item.b64_json)
        .map_err(|_| LlmError::Provider {
            status: 200,
            body: "failed to decode b64_json".into(),
        })?;

    let media_type = item
        .media_type
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "image/png".into());

    Ok(GeneratedImage { bytes, media_type })
}

fn extract_api_error(body: &serde_json::Value) -> Option<String> {
    let error = body.get("error")?;
    let message = error.get("message").and_then(|m| m.as_str());
    let code = error.get("code");
    let metadata = error.get("metadata");

    if message.is_none() && code.is_none() && metadata.is_none() {
        return None;
    }

    let mut parts = Vec::new();
    if let Some(message) = message {
        parts.push(message.to_owned());
    }
    if let Some(code) = code {
        parts.push(format!("code={code}"));
    }
    if let Some(metadata) = metadata {
        parts.push(format!("metadata={metadata}"));
    }
    Some(parts.join(" "))
}

async fn provider_error(response: reqwest::Response) -> LlmError {
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    let body = if looks_like_base64_payload(&body) {
        truncate_for_echo(&body, 256)
    } else {
        body
    };
    LlmError::Provider { status, body }
}

fn looks_like_base64_payload(body: &str) -> bool {
    body.contains("b64_json") && body.len() > 512
}

fn build_api_request(request: ImageGenerateRequest) -> ApiImageRequest {
    let input_references = request
        .input_image_urls
        .into_iter()
        .map(|url| ApiImageReference {
            r#type: "image_url",
            image_url: ApiImageUrl { url },
        })
        .collect();

    ApiImageRequest {
        model: request.model,
        prompt: request.prompt,
        n: 1,
        aspect_ratio: request.aspect_ratio,
        input_references,
    }
}

// --- Client ---

/// HTTP client for the OpenRouter Image API (`POST /images`).
#[derive(Debug, Clone)]
pub struct OpenRouterImageClient {
    http: reqwest::Client,
    base_url: String,
}

impl OpenRouterImageClient {
    /// Creates a new OpenRouter Image API client.
    #[instrument(skip(api_key))]
    pub fn new(api_key: &str, base_url: Option<&str>, user_agent: &str) -> Result<Self> {
        let base_url = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');

        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let mut auth_value = HeaderValue::from_str(&format!("Bearer {}", api_key))?;
        auth_value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, auth_value);

        headers.insert(
            "HTTP-Referer",
            HeaderValue::from_static("https://github.com/chronophylos/twitch-1337"),
        );
        headers.insert("X-Title", HeaderValue::from_static("twitch-1337"));

        let http = reqwest::Client::builder()
            .user_agent(user_agent)
            .default_headers(headers)
            .build()?;

        Ok(Self {
            http,
            base_url: base_url.to_string(),
        })
    }
}

#[async_trait]
impl ImageClient for OpenRouterImageClient {
    #[instrument(skip(self, request))]
    async fn generate(&self, request: ImageGenerateRequest) -> Result<GeneratedImage> {
        let url = format!("{}/images", self.base_url);
        let api_request = build_api_request(request);

        debug!(url = %url, model = %api_request.model, "image generation request");

        let response = self.http.post(&url).json(&api_request).send().await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            warn!(status, "image generation HTTP error");
            return Err(provider_error(response).await);
        }

        let body: serde_json::Value = response.json().await?;
        parse_images_response(&body)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use super::*;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    static CRYPTO: Once = Once::new();

    fn ensure_crypto_provider() {
        CRYPTO.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    #[test]
    fn parse_image_response_decodes_b64() {
        let json = serde_json::json!({
            "created": 1,
            "data": [{"b64_json": BASE64.encode(b"PNGDATA"), "media_type": "image/png"}]
        });
        let img = parse_images_response(&json).unwrap();
        assert_eq!(img.bytes, b"PNGDATA");
        assert_eq!(img.media_type, "image/png");
    }

    #[test]
    fn parse_image_response_rejects_empty_data() {
        let json = serde_json::json!({"data": []});
        assert!(parse_images_response(&json).is_err());
    }

    #[tokio::test]
    async fn generate_posts_to_images_endpoint() {
        ensure_crypto_provider();
        let mock = MockServer::start().await;
        let b64 = BASE64.encode(b"PNGDATA");
        let response_body = serde_json::json!({
            "created": 1,
            "data": [{"b64_json": b64, "media_type": "image/png"}]
        });

        Mock::given(method("POST"))
            .and(path("/api/v1/images"))
            .and(body_json(serde_json::json!({
                "model": "test-model",
                "prompt": "a cat",
                "n": 1,
                "input_references": [
                    {"type": "image_url", "image_url": {"url": "https://example.com/in.png"}}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .mount(&mock)
            .await;

        let client = OpenRouterImageClient::new(
            "test-key",
            Some(&format!("{}/api/v1", mock.uri())),
            "test-agent",
        )
        .unwrap();

        let img = client
            .generate(ImageGenerateRequest {
                model: "test-model".into(),
                prompt: "a cat".into(),
                input_image_urls: vec!["https://example.com/in.png".into()],
                aspect_ratio: None,
            })
            .await
            .unwrap();

        assert_eq!(img.bytes, b"PNGDATA");
        assert_eq!(img.media_type, "image/png");
    }
}
