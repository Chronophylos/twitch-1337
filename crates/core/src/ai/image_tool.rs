//! `generate_image` chat-turn tool: OpenRouter image API + public CDN rehost.

use std::time::{Duration, Instant};

use llm::{ImageClient, ImageGenerateRequest, ToolCall, ToolDefinition, ToolResultMessage};
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use crate::ai::image_host::ImageHost;
use crate::settings::Settings;

pub const GENERATE_IMAGE_TOOL_NAME: &str = "generate_image";
pub const MAX_INPUT_IMAGE_URLS: usize = 4;
pub const MAX_DECODED_IMAGE_BYTES: usize = 10 * 1024 * 1024;

pub const IMAGE_GEN_SYSTEM_APPENDIX: &str = "\
\n\n## Image generation\n\
Only call generate_image when the user asked to create or edit an image.\n\
Pass reference image URLs in input_image_urls for edits. Put the returned\n\
public URL in your final chat reply so viewers can open it.";

/// Per-turn image generation knobs resolved from dashboard settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageGenRuntime {
    pub enabled: bool,
    pub model: String,
    pub timeout: Duration,
}

/// Resolve `!ai` vs `@grok` image model; timeout always from `ai.image_gen`.
pub fn resolve_image_gen_runtime(settings: &Settings, grok_alias: bool) -> ImageGenRuntime {
    if grok_alias {
        let g = &settings.ai.grok.image_gen;
        ImageGenRuntime {
            enabled: g.enabled && !g.model.trim().is_empty(),
            model: g.model.clone(),
            timeout: Duration::from_secs(settings.ai.image_gen.timeout_secs),
        }
    } else {
        let i = &settings.ai.image_gen;
        ImageGenRuntime {
            enabled: i.enabled && !i.model.trim().is_empty(),
            model: i.model.clone(),
            timeout: Duration::from_secs(i.timeout_secs),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GenerateImageArgs {
    prompt: String,
    #[serde(default)]
    input_image_urls: Vec<String>,
    #[serde(default)]
    aspect_ratio: Option<String>,
}

pub fn generate_image_tool() -> ToolDefinition {
    ToolDefinition {
        name: GENERATE_IMAGE_TOOL_NAME.into(),
        description: "Generate or edit an image from a text prompt. Pass reference image URLs \
            in input_image_urls when editing an existing image. Returns a public URL to include \
            in your chat reply."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "What to generate or how to edit the image."
                },
                "input_image_urls": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Public URLs of reference images for edits (max 4)."
                },
                "aspect_ratio": {
                    "type": "string",
                    "description": "Optional aspect ratio (e.g. \"16:9\", \"1:1\")."
                }
            },
            "required": ["prompt"]
        }),
    }
}

pub async fn execute_generate_image(
    client: &dyn ImageClient,
    host: &dyn ImageHost,
    runtime: &ImageGenRuntime,
    call: &ToolCall,
) -> ToolResultMessage {
    let started = Instant::now();
    let args = match call.parse_args::<GenerateImageArgs>() {
        Ok(a) => a,
        Err(e) => {
            return ToolResultMessage::for_call(
                call,
                json!({
                    "error": "invalid_arguments",
                    "details": e.to_string(),
                })
                .to_string(),
            );
        }
    };

    let prompt = args.prompt.trim();
    if prompt.is_empty() {
        return ToolResultMessage::for_call(call, json!({"error": "empty_prompt"}).to_string());
    }

    if args.input_image_urls.len() > MAX_INPUT_IMAGE_URLS {
        return ToolResultMessage::for_call(
            call,
            json!({
                "error": "too_many_input_images",
                "max": MAX_INPUT_IMAGE_URLS,
                "got": args.input_image_urls.len(),
            })
            .to_string(),
        );
    }

    let aspect_ratio = args
        .aspect_ratio
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let request = ImageGenerateRequest {
        model: runtime.model.clone(),
        prompt: prompt.to_string(),
        input_image_urls: args.input_image_urls,
        aspect_ratio,
    };

    let generated = match tokio::time::timeout(runtime.timeout, client.generate(request)).await {
        Ok(Ok(img)) => img,
        Ok(Err(e)) => {
            warn!(error = ?e, model = %runtime.model, "generate_image provider error");
            return ToolResultMessage::for_call(
                call,
                json!({
                    "error": "generation_failed",
                    "details": e.to_string(),
                })
                .to_string(),
            );
        }
        Err(_) => {
            warn!(
                model = %runtime.model,
                timeout_secs = runtime.timeout.as_secs(),
                "generate_image timed out"
            );
            return ToolResultMessage::for_call(call, json!({"error": "timeout"}).to_string());
        }
    };

    if generated.bytes.len() > MAX_DECODED_IMAGE_BYTES {
        return ToolResultMessage::for_call(
            call,
            json!({
                "error": "image_too_large",
                "max_bytes": MAX_DECODED_IMAGE_BYTES,
                "got_bytes": generated.bytes.len(),
            })
            .to_string(),
        );
    }

    let url = match host.upload(&generated.bytes, &generated.media_type).await {
        Ok(u) => u,
        Err(e) => {
            warn!(error = ?e, model = %runtime.model, "generate_image upload failed");
            return ToolResultMessage::for_call(
                call,
                json!({
                    "error": "upload_failed",
                    "details": e.to_string(),
                })
                .to_string(),
            );
        }
    };

    let latency_ms = started.elapsed().as_millis();
    info!(
        model = %runtime.model,
        latency_ms,
        hoster = "imgdrip",
        success = true,
        "generate_image completed"
    );

    ToolResultMessage::for_call(
        call,
        json!({
            "url": url,
            "media_type": generated.media_type,
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use llm::GeneratedImage;

    use super::*;
    use crate::settings::{Settings, overrides};

    struct FakeClient {
        bytes: Vec<u8>,
        media_type: String,
    }

    #[async_trait]
    impl ImageClient for FakeClient {
        async fn generate(&self, _request: ImageGenerateRequest) -> llm::Result<GeneratedImage> {
            Ok(GeneratedImage {
                bytes: self.bytes.clone(),
                media_type: self.media_type.clone(),
            })
        }
    }

    struct FakeHost {
        url: String,
    }

    #[async_trait]
    impl ImageHost for FakeHost {
        async fn upload(&self, _bytes: &[u8], _media_type: &str) -> eyre::Result<String> {
            Ok(self.url.clone())
        }
    }

    fn test_settings() -> Settings {
        let defaults = Settings::compiled_defaults();
        let mut ovr = overrides::SettingsOverrides::default();
        ovr.ai.image_gen.enabled = Some(true);
        ovr.ai.image_gen.model = Some("ai-model".into());
        ovr.ai.image_gen.timeout_secs = Some(120);
        ovr.ai.grok.image_gen.enabled = Some(true);
        ovr.ai.grok.image_gen.model = Some("grok-model".into());
        Settings::resolve(&defaults, &ovr)
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_img".into(),
            name: GENERATE_IMAGE_TOOL_NAME.into(),
            arguments: args,
            arguments_parse_error: None,
        }
    }

    fn runtime(model: &str) -> ImageGenRuntime {
        ImageGenRuntime {
            enabled: true,
            model: model.to_string(),
            timeout: Duration::from_secs(30),
        }
    }

    #[test]
    fn resolve_uses_grok_model_not_ai_model() {
        let settings = test_settings();

        let grok_rt = resolve_image_gen_runtime(&settings, true);
        assert!(grok_rt.enabled);
        assert_eq!(grok_rt.model, "grok-model");
        assert_eq!(grok_rt.timeout, Duration::from_secs(120));

        let ai_rt = resolve_image_gen_runtime(&settings, false);
        assert!(ai_rt.enabled);
        assert_eq!(ai_rt.model, "ai-model");
        assert_eq!(ai_rt.timeout, Duration::from_secs(120));
    }

    #[test]
    fn resolve_disabled_when_model_empty() {
        let mut settings = test_settings();
        settings.ai.image_gen.model.clear();
        let rt = resolve_image_gen_runtime(&settings, false);
        assert!(!rt.enabled);
    }

    #[tokio::test]
    async fn execute_rejects_too_many_urls() {
        let client = Arc::new(FakeClient {
            bytes: b"x".to_vec(),
            media_type: "image/png".into(),
        }) as Arc<dyn ImageClient>;
        let host = Arc::new(FakeHost {
            url: "https://cdn.test/a.png".into(),
        }) as Arc<dyn ImageHost>;
        let urls: Vec<String> = (0..5)
            .map(|i| format!("https://example.com/{i}.png"))
            .collect();
        let msg = execute_generate_image(
            client.as_ref(),
            host.as_ref(),
            &runtime("test-model"),
            &call(json!({"prompt": "draw", "input_image_urls": urls})),
        )
        .await;
        assert!(
            msg.content.contains("too_many_input_images"),
            "{}",
            msg.content
        );
    }

    #[tokio::test]
    async fn execute_happy_path_returns_url() {
        let client = Arc::new(FakeClient {
            bytes: b"PNGDATA".to_vec(),
            media_type: "image/png".into(),
        }) as Arc<dyn ImageClient>;
        let host = Arc::new(FakeHost {
            url: "https://cdn.test/a.png".into(),
        }) as Arc<dyn ImageHost>;
        let msg = execute_generate_image(
            client.as_ref(),
            host.as_ref(),
            &runtime("test-model"),
            &call(json!({"prompt": "a cat"})),
        )
        .await;
        assert!(
            msg.content.contains("\"url\":\"https://cdn.test/a.png\""),
            "{}",
            msg.content
        );
        assert!(
            msg.content.contains("\"media_type\":\"image/png\""),
            "{}",
            msg.content
        );
    }
}
