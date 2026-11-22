//! `run_bash`: Aurora's handle on the `!bash` sandbox.
//!
//! Same interpreter as the command ([`crate::commands::bash::build_interpreter`]:
//! hardened profile, `bash.*` limits, scratch in-memory VFS, virtual
//! identity), but the output goes to the model as structured JSON, never to
//! chat. Every call builds a fresh interpreter, so nothing survives between
//! calls, even inside one Turn.
//!
//! Admission differs from `!bash` on purpose: the tool shares the command's
//! interpreter permits but waits for one up to `bash.timeout_ms` instead of
//! rejecting at once, then answers [`BASH_BUSY`]. It never touches
//! `cooldowns.bash` or the command's per-user active set; per-user rate
//! limiting is `cooldowns.ai`, and the per-Turn cap lives in `turn.rs`.

use std::sync::atomic::Ordering;
use std::time::Duration;

use bashkit::Error as BashkitError;
use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;

use crate::commands::CommandCancellation;
use crate::commands::bash::{FailureKind, MAX_INPUT_BYTES, build_interpreter, failure_kind};
use crate::settings::SettingsHandle;

pub const RUN_BASH_TOOL_NAME: &str = "run_bash";

/// Stderr bytes handed to the model. Enough for a diagnostic, too little to
/// smuggle a second stdout through.
pub(crate) const MAX_STDERR_RESULT_BYTES: usize = 1024;

/// Every interpreter permit stayed taken for the whole wait.
pub(crate) const BASH_BUSY: &str = "bash_busy: the sandbox stayed busy for the whole wait; \
     answer without running the script or try again later";

/// The Turn was cancelled while waiting for a permit or running the script.
const CANCELLED: &str = "cancelled";

#[derive(Debug, Deserialize)]
struct RunBashArgs {
    script: String,
}

pub fn run_bash_tool() -> ToolDefinition {
    ToolDefinition {
        name: RUN_BASH_TOOL_NAME.into(),
        description: "Führt ein Bash-Skript in einer wegwerfbaren Sandbox aus (dieselbe wie \
!bash) und gibt dir das Ergebnis zurück: exit_code, stdout, stderr, Kürzungs-Flags und bei \
Abbruch error (syntax, timeout, resource_limit). Kein Netzwerk, keine Host-Dateien, nur ein \
leeres In-Memory-Dateisystem; jeder Aufruf startet frisch, nichts bleibt zwischen Aufrufen \
erhalten. Gut zum Rechnen, für Textumbau oder zum Spielen. Die Ausgabe landet nur bei dir, \
nicht im Chat."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": format!(
                        "Das komplette Bash-Skript, höchstens {MAX_INPUT_BYTES} Bytes."
                    )
                }
            },
            "required": ["script"]
        }),
    }
}

/// Run one script. `permits` is the semaphore shared with `!bash`.
pub async fn execute_run_bash(
    permits: &Semaphore,
    settings: &SettingsHandle,
    call: &ToolCall,
    cancellation: &CommandCancellation,
) -> ToolResultMessage {
    let script = match call.parse_args::<RunBashArgs>() {
        Ok(a) => a.script,
        Err(e) => {
            return ToolResultMessage::for_call(
                call,
                json!({"error": "invalid_arguments", "details": e.to_string()}).to_string(),
            );
        }
    };
    if script.trim().is_empty() {
        return ToolResultMessage::for_call(
            call,
            json!({"error": "invalid_arguments", "details": "script is empty"}).to_string(),
        );
    }
    if script.len() > MAX_INPUT_BYTES {
        // Refused before admission: an oversized script never takes a
        // permit or builds an interpreter.
        return ToolResultMessage::for_call(
            call,
            failed(
                "resource_limit",
                Some(format!("script exceeds {MAX_INPUT_BYTES} bytes")),
            ),
        );
    }

    let bash_settings = settings.load().bash.clone();
    let wait = Duration::from_millis(bash_settings.timeout_ms);
    let permit = tokio::select! {
        () = cancellation.cancelled() => {
            return ToolResultMessage::for_call(call, CANCELLED.to_owned());
        }
        acquired = tokio::time::timeout(wait, permits.acquire()) => match acquired {
            Ok(Ok(permit)) => permit,
            // The semaphore is never closed; a closed one admits nobody,
            // which is what busy means.
            Ok(Err(_)) | Err(_) => {
                tracing::debug!(wait_ms = bash_settings.timeout_ms, "run_bash found no free permit");
                return ToolResultMessage::for_call(call, BASH_BUSY.to_owned());
            }
        },
    };

    let mut bash = build_interpreter(&bash_settings);
    let stop = bash.cancellation_token();
    let result = tokio::select! {
        () = cancellation.cancelled() => {
            // The interpreter checks this token at its next command boundary;
            // the select already dropped its future.
            stop.store(true, Ordering::SeqCst);
            drop(permit);
            return ToolResultMessage::for_call(call, CANCELLED.to_owned());
        }
        result = bash.exec(&script) => result,
    };
    drop(permit);

    let content = match result {
        Ok(output) => {
            let stdout_text = output.stdout.text_lossy();
            let stderr_text = output.stderr.text_lossy();
            let (stdout, stdout_cut) = cap(&stdout_text, bash_settings.max_stdout_bytes);
            let (stderr, stderr_cut) = cap(&stderr_text, MAX_STDERR_RESULT_BYTES);
            json!({
                "exit_code": output.exit_code,
                "stdout": stdout,
                "truncated": output.stdout_truncated || stdout_cut,
                "stderr": stderr,
                "stderr_truncated": output.stderr_truncated || stderr_cut,
            })
            .to_string()
        }
        Err(error) => failure_content(&error),
    };
    tracing::debug!(tool = RUN_BASH_TOOL_NAME, "run_bash finished");
    ToolResultMessage::for_call(call, content)
}

fn failure_content(error: &BashkitError) -> String {
    match failure_kind(error) {
        FailureKind::Syntax => failed("syntax", None),
        FailureKind::Timeout => failed("timeout", None),
        FailureKind::ResourceLimit => failed("resource_limit", None),
        FailureKind::Cancelled => CANCELLED.to_owned(),
        FailureKind::Unexpected => {
            crate::log_handled!(warn, error, "run_bash interpreter failed");
            // The interpreter aborted without an exit status. `timeout` is
            // the least misleading public kind: retrying may help, whereas
            // `syntax` and `resource_limit` would wrongly blame the script.
            // The warn log retains Bashkit's actual error classification.
            failed("timeout", None)
        }
    }
}

/// Result for a run that produced no `ExecResult`.
fn failed(kind: &str, details: Option<String>) -> String {
    let mut value = json!({
        "exit_code": null,
        "stdout": "",
        "truncated": false,
        "stderr": "",
        "stderr_truncated": false,
        "error": kind,
    });
    if let Some(details) = details {
        value["details"] = json!(details);
    }
    value.to_string()
}

/// `text` cut to at most `max` bytes on a char boundary, and whether it was cut.
fn cap(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use serde_json::Value;

    use super::*;
    use crate::settings::Settings;

    fn settings(edit: impl FnOnce(&mut crate::settings::BashSettings)) -> SettingsHandle {
        let mut s = Settings::compiled_defaults();
        edit(&mut s.bash);
        SettingsHandle::from_settings(s)
    }

    fn call(script: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: RUN_BASH_TOOL_NAME.into(),
            arguments: json!({ "script": script }),
            arguments_parse_error: None,
        }
    }

    async fn run_with(
        permits: &Semaphore,
        settings: &SettingsHandle,
        script: &str,
        cancellation: &CommandCancellation,
    ) -> String {
        execute_run_bash(permits, settings, &call(script), cancellation)
            .await
            .content
    }

    async fn run(settings: &SettingsHandle, script: &str) -> Value {
        let permits = Semaphore::new(1);
        let content = run_with(&permits, settings, script, &CommandCancellation::new()).await;
        serde_json::from_str(&content).unwrap_or_else(|e| panic!("not JSON ({e}): {content}"))
    }

    #[tokio::test]
    async fn completed_run_reports_every_field() {
        let v = run(&settings(|_| {}), "printf hi; printf oops >&2; exit 3").await;
        assert_eq!(v["exit_code"], 3);
        assert_eq!(v["stdout"], "hi");
        assert_eq!(v["truncated"], false);
        assert_eq!(v["stderr"], "oops");
        assert_eq!(v["stderr_truncated"], false);
        assert!(v.get("error").is_none(), "no error on a completed run: {v}");
    }

    #[tokio::test]
    async fn stdout_is_capped_at_the_setting() {
        let v = run(
            &settings(|b| b.max_stdout_bytes = 16),
            "printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
        )
        .await;
        assert_eq!(v["stdout"], "a".repeat(16));
        assert_eq!(v["truncated"], true);
    }

    #[tokio::test]
    async fn stderr_is_capped_at_one_kib() {
        let v = run(&settings(|_| {}), "printf '%02000d' 0 >&2").await;
        assert_eq!(
            v["stderr"].as_str().expect("stderr").len(),
            MAX_STDERR_RESULT_BYTES
        );
        assert_eq!(v["stderr_truncated"], true);
        assert_eq!(v["truncated"], false);
    }

    #[tokio::test]
    async fn failures_carry_a_machine_readable_kind() {
        // Mirrors the `!bash` integration tests: only the timeout case runs
        // under a short deadline, so the recursion hits its depth limit first.
        for (script, timeout_ms, kind) in [
            ("if then fi", 1000, "syntax"),
            ("sleep 1", 100, "timeout"),
            ("f(){ f; }; f", 1000, "resource_limit"),
        ] {
            let v = run(&settings(|b| b.timeout_ms = timeout_ms), script).await;
            assert_eq!(v["error"], kind, "{script}: {v}");
            assert!(v["exit_code"].is_null(), "{script}: {v}");
            assert_eq!(v["truncated"], false);
            assert_eq!(v["stderr_truncated"], false);
        }
    }

    #[test]
    fn unexpected_failures_stay_within_the_documented_kinds() {
        let error = BashkitError::io_sanitized(std::io::Error::other("injected"));
        assert_eq!(failure_kind(&error), FailureKind::Unexpected);

        let content = failure_content(&error);
        let v: Value = serde_json::from_str(&content).expect("json");
        let kind = v["error"].as_str().expect("error kind");
        assert!(
            matches!(kind, "syntax" | "timeout" | "resource_limit"),
            "unexpected failure used undocumented kind {kind:?}: {v}"
        );
        assert_eq!(kind, "timeout");
        assert!(v["exit_code"].is_null());
    }

    #[tokio::test]
    async fn oversized_script_is_refused_without_a_permit() {
        // No permit at all: reaching admission would hang until busy.
        let permits = Semaphore::new(0);
        let script = format!("printf ran; {}", "#".repeat(MAX_INPUT_BYTES));
        let start = Instant::now();
        let content = run_with(
            &permits,
            &settings(|b| b.timeout_ms = 5_000),
            &script,
            &CommandCancellation::new(),
        )
        .await;
        assert!(start.elapsed() < Duration::from_secs(1));
        let v: Value = serde_json::from_str(&content).expect("json");
        assert_eq!(v["error"], "resource_limit");
        assert_eq!(v["stdout"], "");
    }

    #[tokio::test]
    async fn calls_do_not_share_the_filesystem() {
        let s = settings(|_| {});
        let script = "cat /tmp/state 2>/dev/null; printf leaked > /tmp/state";
        let first = run(&s, script).await;
        let second = run(&s, script).await;
        assert_eq!(first["stdout"], "");
        assert_eq!(
            second["stdout"], "",
            "second call saw the first call's file"
        );
    }

    #[tokio::test]
    async fn held_permit_answers_busy_after_the_timeout() {
        let permits = Semaphore::new(1);
        let _held = permits.try_acquire().expect("free permit");
        let start = Instant::now();
        let content = run_with(
            &permits,
            &settings(|b| b.timeout_ms = 200),
            "printf hi",
            &CommandCancellation::new(),
        )
        .await;
        let waited = start.elapsed();
        assert_eq!(content, BASH_BUSY);
        assert!(
            waited >= Duration::from_millis(200),
            "gave up early: {waited:?}"
        );
        assert!(waited < Duration::from_secs(2), "kept waiting: {waited:?}");
    }

    #[tokio::test]
    async fn permit_freed_within_the_window_runs_the_script() {
        let permits = Arc::new(Semaphore::new(1));
        let held = Arc::clone(&permits)
            .try_acquire_owned()
            .expect("free permit");
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(held);
        });
        let content = run_with(
            &permits,
            &settings(|b| b.timeout_ms = 2_000),
            "printf hi",
            &CommandCancellation::new(),
        )
        .await;
        let v: Value = serde_json::from_str(&content).expect("json");
        assert_eq!(v["stdout"], "hi");
        assert_eq!(v["exit_code"], 0);
    }

    #[tokio::test]
    async fn cancellation_stops_a_pending_wait() {
        let permits = Semaphore::new(1);
        let _held = permits.try_acquire().expect("free permit");
        let cancellation = CommandCancellation::new();
        let canceller = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            canceller.cancel();
        });
        let start = Instant::now();
        let content = run_with(
            &permits,
            &settings(|b| b.timeout_ms = 10_000),
            "printf hi",
            &cancellation,
        )
        .await;
        assert_eq!(content, CANCELLED);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn cancellation_stops_an_in_flight_run_and_frees_the_permit() {
        let permits = Semaphore::new(1);
        let cancellation = CommandCancellation::new();
        let canceller = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            canceller.cancel();
        });
        let start = Instant::now();
        let content = run_with(
            &permits,
            &settings(|b| b.timeout_ms = 10_000),
            "sleep 10",
            &cancellation,
        )
        .await;
        assert_eq!(content, CANCELLED);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(permits.available_permits(), 1);
    }

    #[test]
    fn cap_respects_char_boundaries() {
        assert_eq!(cap("äb", 1), ("", true));
        assert_eq!(cap("äb", 2), ("ä", true));
        assert_eq!(cap("äb", 3), ("äb", false));
    }
}
