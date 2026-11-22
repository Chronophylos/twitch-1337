//! Per-provider capability declaration.
//!
//! Providers behind the same wire protocol still disagree on which knobs
//! exist: OpenRouter takes `reasoning.effort` including `xhigh` plus a
//! service tier, stock OpenAI takes a flat `reasoning_effort` and no tier,
//! a random OpenAI-compatible endpoint takes whatever its vendor documents,
//! and Ollama takes none of it. Declaring that here means the form, the
//! save-validation and the wire serializer all read one table instead of
//! sniffing the base URL.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AiProvider {
    OpenRouter,
    OpenAi,
    OpenAiCompatible,
    Ollama,
}

/// What `reasoning_effort` values a provider accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Efforts {
    /// Exactly these values; the form renders them as segments.
    Fixed(&'static [&'static str]),
    /// Anything non-blank; the form renders a text input.
    FreeText,
    /// The knob does not exist; the form hides it and the client strips it.
    None,
}

/// Wire protocol used by the optional provider model listing endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelListing {
    OpenRouter,
    OpenAiStyle,
    OllamaTags,
    /// Reserved for providers that cannot enumerate models.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCaps {
    pub efforts: Efforts,
    pub service_tier: bool,
    /// Used when the profile leaves `base_url` empty. `None` = required.
    pub default_base_url: Option<&'static str>,
    pub model_listing: ModelListing,
}

impl ProviderCaps {
    pub fn allows_effort(&self, value: &str) -> bool {
        match self.efforts {
            Efforts::Fixed(list) => list.contains(&value),
            Efforts::FreeText => !value.trim().is_empty(),
            Efforts::None => false,
        }
    }

    /// "fixed" | "free" | "none" — mirrors `Efforts` for callers (dashboard
    /// templates in particular) that can't match the enum itself and only
    /// need to pick which control shape to render.
    pub const fn effort_kind(&self) -> &'static str {
        match self.efforts {
            Efforts::Fixed(_) => "fixed",
            Efforts::FreeText => "free",
            Efforts::None => "none",
        }
    }

    pub const fn supports_model_listing(&self) -> bool {
        !matches!(self.model_listing, ModelListing::None)
    }
}

impl AiProvider {
    pub const ALL: [Self; 4] = [
        Self::OpenRouter,
        Self::OpenAi,
        Self::OpenAiCompatible,
        Self::Ollama,
    ];

    /// Stable snake_case form — matches `serde(rename_all)` output and is
    /// what the dashboard `<input value=…>` and POST parser exchange.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenRouter => "open_router",
            Self::OpenAi => "open_ai",
            Self::OpenAiCompatible => "open_ai_compatible",
            Self::Ollama => "ollama",
        }
    }

    /// Human label for the dashboard.
    pub const fn label(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter",
            Self::OpenAi => "OpenAI",
            Self::OpenAiCompatible => "OpenAI-compatible",
            Self::Ollama => "Ollama",
        }
    }

    /// True when this provider speaks OpenRouter's reasoning/tier dialect.
    pub const fn is_openrouter(self) -> bool {
        matches!(self, Self::OpenRouter)
    }

    /// True when this provider is served by `OpenAiClient` (vs `OllamaClient`).
    pub const fn uses_openai_client(self) -> bool {
        !matches!(self, Self::Ollama)
    }

    /// Base URL to use when talking to this provider over the OpenAI-compatible
    /// `/chat/completions` API, which is not always the same host shape the
    /// provider's native client wants: Ollama serves its own API at
    /// `{host}/api/chat` but its OpenAI-compat shim at `{host}/v1/...`.
    pub const fn openai_compat_base_url(self) -> Option<&'static str> {
        match self {
            Self::Ollama => Some("http://localhost:11434/v1"),
            _ => self.caps().default_base_url,
        }
    }

    pub const fn caps(self) -> ProviderCaps {
        match self {
            Self::OpenRouter => ProviderCaps {
                efforts: Efforts::Fixed(&["minimal", "medium", "high", "xhigh"]),
                service_tier: true,
                default_base_url: Some("https://openrouter.ai/api/v1"),
                model_listing: ModelListing::OpenRouter,
            },
            Self::OpenAi => ProviderCaps {
                efforts: Efforts::Fixed(&["minimal", "medium", "high"]),
                service_tier: false,
                default_base_url: Some("https://api.openai.com/v1"),
                model_listing: ModelListing::OpenAiStyle,
            },
            Self::OpenAiCompatible => ProviderCaps {
                efforts: Efforts::FreeText,
                // OpenAI-wire aggregators like NanoGPT accept `service_tier`;
                // profiles pointed at servers that don't support it simply
                // leave it unset, and the field is then omitted from the
                // request body.
                service_tier: true,
                default_base_url: None,
                model_listing: ModelListing::OpenAiStyle,
            },
            Self::Ollama => ProviderCaps {
                efforts: Efforts::None,
                service_tier: false,
                default_base_url: Some("http://localhost:11434"),
                model_listing: ModelListing::OllamaTags,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_gate_effort_and_tier() {
        for (provider, effort, effort_ok, tier) in [
            (AiProvider::OpenRouter, "xhigh", true, true),
            (AiProvider::OpenAi, "high", true, false),
            (AiProvider::OpenAi, "xhigh", false, false),
            (
                AiProvider::OpenAiCompatible,
                "whatever-the-vendor-calls-it",
                true,
                true,
            ),
            (AiProvider::Ollama, "medium", false, false),
        ] {
            let caps = provider.caps();
            assert_eq!(
                caps.allows_effort(effort),
                effort_ok,
                "{provider:?} effort {effort:?}"
            );
            assert_eq!(caps.service_tier, tier, "{provider:?} service tier");
        }
    }

    #[test]
    fn caps_declare_model_listing_wire_formats() {
        for (provider, listing) in [
            (AiProvider::OpenRouter, ModelListing::OpenRouter),
            (AiProvider::OpenAi, ModelListing::OpenAiStyle),
            (AiProvider::OpenAiCompatible, ModelListing::OpenAiStyle),
            (AiProvider::Ollama, ModelListing::OllamaTags),
        ] {
            let caps = provider.caps();
            assert_eq!(caps.model_listing, listing, "{provider:?}");
            assert!(caps.supports_model_listing(), "{provider:?}");
        }
        let caps = ProviderCaps {
            efforts: Efforts::None,
            service_tier: false,
            default_base_url: None,
            model_listing: ModelListing::None,
        };
        assert!(!caps.supports_model_listing());
    }

    #[test]
    fn as_str_roundtrips_through_serde() {
        for p in AiProvider::ALL {
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(json, format!("\"{}\"", p.as_str()));
        }
    }

    /// `MediaClient` posts to `{base}/chat/completions`, so Ollama's compat
    /// base has to carry the `/v1` its native `{host}/api/chat` client must
    /// not have. Every other provider speaks OpenAI-compat natively.
    #[test]
    fn ollama_compat_base_url_carries_v1_while_native_does_not() {
        let native = AiProvider::Ollama.caps().default_base_url;
        let compat = AiProvider::Ollama.openai_compat_base_url();
        assert_eq!(native, Some("http://localhost:11434"));
        assert_eq!(compat, Some("http://localhost:11434/v1"));
        assert!(!native.unwrap().ends_with("/v1"));
        assert!(compat.unwrap().ends_with("/v1"));
    }
}
