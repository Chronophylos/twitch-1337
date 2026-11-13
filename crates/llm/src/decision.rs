//! Client for OpenRouter's alpha Decisions API (`/api/alpha/decisions`).

use std::collections::BTreeMap;
use std::time::Duration;

use indexmap::IndexMap;
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::error::{LlmError, Result};
use crate::util::truncate_for_echo;

/// Request body for a decision call.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DecisionRequest {
    pub model: String,
    pub state: Vec<StatePart>,
    pub questions: BTreeMap<String, Question>,
}

/// One piece of the state the model decides on: plain text, named text
/// fields the question can refer to by key, or an image.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum StatePart {
    Text(String),
    Fields(BTreeMap<String, String>),
    Image(ImagePart),
}

impl StatePart {
    /// Image part from a `data:` URL.
    pub fn image(data_url: String) -> Self {
        Self::Image(ImagePart::ImageUrl {
            image_url: ImageUrl { url: data_url },
        })
    }
}

/// Wire form of an image state part: `{"type":"image_url","image_url":{..}}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImagePart {
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    Choice {
        instructions: String,
        /// Sent in insertion order: decision models lean toward the first option.
        criteria: IndexMap<String, String>,
    },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DecisionResponse {
    #[serde(default)]
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Choice {
        choice: String,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        #[serde(default)]
        confidence: Option<f64>,
    },
    Refusal,
    #[serde(other)]
    Other,
}

/// Decisions endpoint for a profile base URL: its origin plus
/// `/api/alpha/decisions`.
pub fn decisions_endpoint(base_url: &str) -> Result<String> {
    let url = Url::parse(base_url).map_err(|e| LlmError::Provider {
        status: 0,
        body: format!("invalid base url {base_url:?}: {e}"),
    })?;
    if !url.has_host() {
        return Err(LlmError::Provider {
            status: 0,
            body: format!("base url {base_url:?} has no host"),
        });
    }
    Ok(format!(
        "{}/api/alpha/decisions",
        url.origin().ascii_serialization()
    ))
}

/// POST a decision request and decode the answers.
pub async fn decide(
    http: &reqwest::Client,
    endpoint: &str,
    api_key: Option<&str>,
    req: &DecisionRequest,
    timeout: Duration,
) -> Result<DecisionResponse> {
    let mut builder = http.post(endpoint).timeout(timeout).json(req);
    if let Some(key) = api_key {
        builder = builder.bearer_auth(key);
    }
    let resp = builder.send().await.map_err(map_transport)?;
    let status = resp.status();
    let body = resp.text().await.map_err(map_transport)?;
    if !status.is_success() {
        return Err(LlmError::Provider {
            status: status.as_u16(),
            body: truncate_for_echo(&body, 2000),
        });
    }
    serde_json::from_str(&body).map_err(|source| LlmError::Decode {
        stage: "decision response",
        source,
    })
}

fn map_transport(e: reqwest::Error) -> LlmError {
    if e.is_timeout() {
        LlmError::Timeout
    } else {
        LlmError::Http(e)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn http() -> reqwest::Client {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
        reqwest::Client::new()
    }

    fn request() -> DecisionRequest {
        DecisionRequest {
            model: "nace-ai/drex-max".into(),
            state: vec![
                StatePart::Text("hello".into()),
                StatePart::image("data:image/png;base64,AAAA".into()),
            ],
            questions: BTreeMap::from([(
                "spoiler".to_string(),
                Question::Choice {
                    instructions: "classify".into(),
                    criteria: IndexMap::from([
                        ("story".into(), "story spoilers".into()),
                        ("gameplay".into(), "gameplay".into()),
                    ]),
                },
            )]),
        }
    }

    #[test]
    fn endpoint_uses_origin() {
        assert_eq!(
            decisions_endpoint("https://openrouter.ai/api/v1").unwrap(),
            "https://openrouter.ai/api/alpha/decisions"
        );
        assert_eq!(
            decisions_endpoint("http://127.0.0.1:1234/v1").unwrap(),
            "http://127.0.0.1:1234/api/alpha/decisions"
        );
        assert!(decisions_endpoint("not a url").is_err());
    }

    #[test]
    fn request_serializes_to_wire_shape() {
        assert_eq!(
            serde_json::to_value(request()).unwrap(),
            json!({
                "model": "nace-ai/drex-max",
                "state": ["hello", {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}],
                "questions": {"spoiler": {
                    "type": "choice",
                    "instructions": "classify",
                    "criteria": {"story": "story spoilers", "gameplay": "gameplay"}
                }}
            })
        );
        // `Value` sorts keys, so check criteria order on the raw string.
        let raw = serde_json::to_string(&request()).unwrap();
        assert!(raw.find("\"story\"").unwrap() < raw.find("\"gameplay\"").unwrap());
    }

    async fn server_with(resp: ResponseTemplate) -> (MockServer, String) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/alpha/decisions"))
            .and(header("authorization", "Bearer k"))
            .respond_with(resp)
            .mount(&server)
            .await;
        let endpoint = decisions_endpoint(&format!("{}/v1", server.uri())).unwrap();
        (server, endpoint)
    }

    #[tokio::test]
    async fn decodes_choice_ignoring_extra_fields() {
        let (_s, ep) = server_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "x", "provider": "DeepInfra", "model": "drex-max-20261016",
            "answers": {"spoiler": {"type": "choice", "choice": "story",
                "probabilities": {"story": 0.998, "marketing": 0.001}, "confidence": 0.997}},
            "usage": {"input_tokens": 207}
        })))
        .await;
        let r = decide(&http(), &ep, Some("k"), &request(), Duration::from_secs(5))
            .await
            .unwrap();
        match &r.answers["spoiler"] {
            Answer::Choice {
                choice,
                probabilities,
                confidence,
            } => {
                assert_eq!(choice, "story");
                assert!(probabilities["story"] > 0.99);
                assert_eq!(*confidence, Some(0.997));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn decodes_refusal_and_unknown() {
        let (_s, ep) = server_with(ResponseTemplate::new(200).set_body_json(json!({
            "answers": {"spoiler": {"type": "refusal"}, "b": {"type": "noul", "x": 1}}
        })))
        .await;
        let r = decide(&http(), &ep, Some("k"), &request(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(r.answers["spoiler"], Answer::Refusal);
        assert_eq!(r.answers["b"], Answer::Other);
    }

    #[tokio::test]
    async fn non_2xx_is_provider_error() {
        let (_s, ep) = server_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": {"code": 400, "message": "bad model"}})),
        )
        .await;
        let err = decide(&http(), &ep, Some("k"), &request(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(
            matches!(err, LlmError::Provider { status: 400, .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn slow_response_is_timeout() {
        let (_s, ep) = server_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"answers": {}}))
                .set_delay(Duration::from_millis(500)),
        )
        .await;
        let err = decide(
            &http(),
            &ep,
            Some("k"),
            &request(),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LlmError::Timeout), "{err:?}");
    }

    #[tokio::test]
    async fn garbage_body_is_decode_error() {
        let (_s, ep) = server_with(ResponseTemplate::new(200).set_body_string("nope")).await;
        let err = decide(&http(), &ep, Some("k"), &request(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::Decode { .. }), "{err:?}");
    }
}
