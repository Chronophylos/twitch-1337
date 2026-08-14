//! Prepare a user-facing agent chat line.
//!
//! After the tool loop, the final text is sanitized, rewritten once if it
//! exceeds [`AGENT_REPLY_CHAR_LIMIT`], and clamped to that limit only as a
//! fallback. The 480-byte IRC clamp must not run on this path.

use std::time::Duration;

use llm::{ChatCompletionRequest, LlmClient, Message, TraceIds};
use tracing::{debug, warn};

use crate::twitch::chat_sender::collapse_control_and_whitespace;

/// Character ceiling for Aurora and Grok chat replies. Over this, one
/// no-tools rewrite is attempted; anything still longer is clamped here.
pub const AGENT_REPLY_CHAR_LIMIT: usize = 490;

pub struct RewordOpts {
    pub model: String,
    pub timeout: Duration,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub trace: TraceIds,
}

/// Sanitize `draft`, optionally rewrite it once if it is over
/// [`AGENT_REPLY_CHAR_LIMIT`] characters, then clamp to that limit.
pub async fn prepare_agent_reply(llm: &dyn LlmClient, draft: &str, opts: &RewordOpts) -> String {
    let cleaned = collapse_control_and_whitespace(clean_user_facing_ai_response(draft));
    if cleaned.chars().count() <= AGENT_REPLY_CHAR_LIMIT {
        return cleaned;
    }

    debug!(
        chars = cleaned.chars().count(),
        limit = AGENT_REPLY_CHAR_LIMIT,
        "agent reply over limit, requesting one rewrite"
    );

    let request = ChatCompletionRequest {
        model: opts.model.clone(),
        messages: vec![
            Message::system(format!(
                "Rewrite the following Twitch chat message so it is at most {AGENT_REPLY_CHAR_LIMIT} characters. Keep the meaning and language. Reply with only the rewritten message, as a single line."
            )),
            Message::user(cleaned.clone()),
        ],
        reasoning_effort: opts.reasoning_effort.clone(),
        service_tier: opts.service_tier.clone(),
        trace: opts.trace.clone(),
    };

    let rewritten = match tokio::time::timeout(opts.timeout, llm.chat_completion(request)).await {
        Ok(Ok(text)) => {
            let line = collapse_control_and_whitespace(clean_user_facing_ai_response(&text));
            if line.trim().is_empty() {
                warn!("agent rewrite returned empty, clamping original");
                None
            } else {
                Some(line)
            }
        }
        Ok(Err(error)) => {
            warn!(?error, "agent rewrite failed, clamping original");
            None
        }
        Err(_) => {
            warn!(
                timeout_s = opts.timeout.as_secs(),
                "agent rewrite timed out, clamping original"
            );
            None
        }
    };

    let to_send = rewritten.unwrap_or(cleaned);
    if to_send.chars().count() <= AGENT_REPLY_CHAR_LIMIT {
        to_send
    } else {
        clamp_chars(&to_send, AGENT_REPLY_CHAR_LIMIT)
    }
}

fn clean_user_facing_ai_response(text: &str) -> &str {
    let trimmed = text.trim_start();
    for marker in ["thought", "analysis", "final"] {
        let Some(prefix) = trimmed.get(..marker.len()) else {
            continue;
        };
        if !prefix.eq_ignore_ascii_case(marker) {
            continue;
        }

        let rest = &trimmed[marker.len()..];
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }

        if let Some((_, message)) = rest.trim_start().split_once('|') {
            return message.trim_start();
        }
    }

    text
}

fn clamp_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    const ELLIPSIS: char = '…';
    let keep = max_chars.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push(ELLIPSIS);
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use llm::{
        ChatCompletionRequest, LlmClient, LlmError, ToolChatCompletionRequest,
        ToolChatCompletionResponse, TraceIds,
    };

    use super::*;

    enum Scripted {
        Text(String),
        Error,
        Sleep(Duration),
    }

    struct ScriptedClient {
        queue: Mutex<Vec<Scripted>>,
        chat_calls: Mutex<Vec<ChatCompletionRequest>>,
        tool_calls: Mutex<usize>,
    }

    impl ScriptedClient {
        fn new(steps: Vec<Scripted>) -> Self {
            Self {
                queue: Mutex::new(steps),
                chat_calls: Mutex::new(Vec::new()),
                tool_calls: Mutex::new(0),
            }
        }

        fn chat_calls(&self) -> Vec<ChatCompletionRequest> {
            self.chat_calls.lock().unwrap().clone()
        }

        fn tool_call_count(&self) -> usize {
            *self.tool_calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn chat_completion(&self, request: ChatCompletionRequest) -> llm::Result<String> {
            self.chat_calls.lock().unwrap().push(request);
            let step = self.queue.lock().unwrap().pop();
            match step {
                Some(Scripted::Text(s)) => Ok(s),
                Some(Scripted::Error) => Err(LlmError::EmptyResponse),
                Some(Scripted::Sleep(d)) => {
                    tokio::time::sleep(d).await;
                    Ok("late".into())
                }
                None => Err(LlmError::Provider {
                    status: 0,
                    body: "no chat response queued".into(),
                }),
            }
        }

        async fn chat_completion_with_tools(
            &self,
            _request: ToolChatCompletionRequest,
        ) -> llm::Result<ToolChatCompletionResponse> {
            *self.tool_calls.lock().unwrap() += 1;
            Err(LlmError::EmptyResponse)
        }
    }

    fn opts() -> RewordOpts {
        RewordOpts {
            model: "test-model".into(),
            timeout: Duration::from_secs(5),
            reasoning_effort: Some("low".into()),
            service_tier: None,
            trace: TraceIds {
                user: Some("alice".into()),
                session_id: Some("sess".into()),
            },
        }
    }

    #[tokio::test]
    async fn short_reply_is_returned_without_a_rewrite_call() {
        let llm = ScriptedClient::new(vec![]);
        let out = prepare_agent_reply(&llm, "hello chat", &opts()).await;
        assert_eq!(out, "hello chat");
        assert!(llm.chat_calls().is_empty());
        assert_eq!(llm.tool_call_count(), 0);
    }

    #[tokio::test]
    async fn exactly_limit_does_not_reword() {
        let draft = "x".repeat(AGENT_REPLY_CHAR_LIMIT);
        let llm = ScriptedClient::new(vec![]);
        let out = prepare_agent_reply(&llm, &draft, &opts()).await;
        assert_eq!(out, draft);
        assert!(llm.chat_calls().is_empty());
    }

    #[tokio::test]
    async fn overlong_reply_is_replaced_by_one_rewrite() {
        let draft = "x".repeat(AGENT_REPLY_CHAR_LIMIT + 1);
        let llm = ScriptedClient::new(vec![Scripted::Text("short rewrite".into())]);
        let out = prepare_agent_reply(&llm, &draft, &opts()).await;
        assert_eq!(out, "short rewrite");
        assert_eq!(llm.chat_calls().len(), 1);
        assert_eq!(llm.tool_call_count(), 0);
        let call = &llm.chat_calls()[0];
        assert_eq!(call.model, "test-model");
        assert_eq!(call.reasoning_effort.as_deref(), Some("low"));
        assert!(
            call.messages
                .iter()
                .any(|m| m.content.contains(&draft) || m.content == draft),
            "overlong draft must be sent back for rewrite: {call:?}"
        );
    }

    #[tokio::test]
    async fn rewrite_error_clamps_original() {
        let draft = "y".repeat(AGENT_REPLY_CHAR_LIMIT + 20);
        let llm = ScriptedClient::new(vec![Scripted::Error]);
        let out = prepare_agent_reply(&llm, &draft, &opts()).await;
        assert_eq!(out.chars().count(), AGENT_REPLY_CHAR_LIMIT);
        assert!(out.ends_with('…'));
        assert!(out.starts_with('y'));
        assert_eq!(llm.chat_calls().len(), 1);
    }

    #[tokio::test]
    async fn empty_rewrite_clamps_original() {
        let draft = "z".repeat(AGENT_REPLY_CHAR_LIMIT + 5);
        let llm = ScriptedClient::new(vec![Scripted::Text(String::new())]);
        let out = prepare_agent_reply(&llm, &draft, &opts()).await;
        assert_eq!(out.chars().count(), AGENT_REPLY_CHAR_LIMIT);
        assert!(out.starts_with('z'));
        assert!(out.ends_with('…'));
    }

    #[tokio::test]
    async fn whitespace_only_rewrite_clamps_original() {
        let draft = "w".repeat(AGENT_REPLY_CHAR_LIMIT + 5);
        let llm = ScriptedClient::new(vec![Scripted::Text("  \n\t  ".into())]);
        let out = prepare_agent_reply(&llm, &draft, &opts()).await;
        assert!(out.starts_with('w'));
        assert_eq!(out.chars().count(), AGENT_REPLY_CHAR_LIMIT);
    }

    #[tokio::test]
    async fn rewrite_that_is_still_overlong_is_clamped() {
        let original = "a".repeat(AGENT_REPLY_CHAR_LIMIT + 50);
        let rewrite = "b".repeat(AGENT_REPLY_CHAR_LIMIT + 10);
        let llm = ScriptedClient::new(vec![Scripted::Text(rewrite)]);
        let out = prepare_agent_reply(&llm, &original, &opts()).await;
        assert_eq!(out.chars().count(), AGENT_REPLY_CHAR_LIMIT);
        assert!(out.starts_with('b'), "clamp the rewrite, not the original");
        assert!(out.ends_with('…'));
        assert!(!out.contains('a'));
    }

    #[tokio::test]
    async fn timeout_clamps_original() {
        let draft = "t".repeat(AGENT_REPLY_CHAR_LIMIT + 5);
        let llm = ScriptedClient::new(vec![Scripted::Sleep(Duration::from_secs(5))]);
        let opts = RewordOpts {
            timeout: Duration::from_millis(20),
            ..opts()
        };
        let out = prepare_agent_reply(&llm, &draft, &opts).await;
        assert!(out.starts_with('t'));
        assert_eq!(out.chars().count(), AGENT_REPLY_CHAR_LIMIT);
        assert!(out.ends_with('…'));
    }

    #[tokio::test]
    async fn control_chars_are_collapsed_before_the_length_check() {
        // Raw length is well over the limit; after collapsing newlines it
        // is a short line and must not trigger a rewrite.
        let draft = format!("hello{}\nworld", "\n".repeat(AGENT_REPLY_CHAR_LIMIT));
        let llm = ScriptedClient::new(vec![]);
        let out = prepare_agent_reply(&llm, &draft, &opts()).await;
        assert_eq!(out, "hello world");
        assert!(llm.chat_calls().is_empty());
    }

    #[tokio::test]
    async fn thought_marker_prefix_is_stripped() {
        let llm = ScriptedClient::new(vec![]);
        let out = prepare_agent_reply(&llm, "thought scratch | hello there", &opts()).await;
        assert_eq!(out, "hello there");
        assert!(llm.chat_calls().is_empty());
    }

    #[tokio::test]
    async fn clamp_lands_on_a_char_boundary() {
        let draft = "ä".repeat(AGENT_REPLY_CHAR_LIMIT + 10);
        let llm = ScriptedClient::new(vec![Scripted::Error]);
        let out = prepare_agent_reply(&llm, &draft, &opts()).await;
        assert_eq!(out.chars().count(), AGENT_REPLY_CHAR_LIMIT);
        assert!(out.ends_with('…'));
        assert!(out.is_char_boundary(out.len()));
    }
}
