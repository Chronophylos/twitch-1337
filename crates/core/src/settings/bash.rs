//! `!bash` sandbox runtime settings.
//!
//! Every value here shapes one throwaway Bashkit interpreter per invocation.
//! The limits are the only defense between public chat input and the bot's
//! CPU, memory, and VFS budget, so each carries a constant range the derive
//! enforces on save.

use serde::{Deserialize, Serialize};

use super::knob::Knobs;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = BashOverrides)]
pub struct BashSettings {
    /// Interpreter runs admitted at once. Read once at startup to size the
    /// command's semaphore, so a change needs a restart.
    #[knob(restart, min = 1, max = 4)]
    pub max_concurrent: usize,
    /// Wall-clock ceiling for one script, applied to both script execution
    /// and parsing.
    #[knob(min = 100, max = 10_000)]
    pub timeout_ms: u64,
    /// Bytes of stdout captured and available to the reply.
    #[knob(min = 1, max = 1_048_576)]
    pub max_stdout_bytes: usize,
    /// Total bytes the throwaway in-memory filesystem may hold (also the
    /// per-file ceiling).
    #[knob(min = 1, max = 1_048_576)]
    pub max_vfs_bytes: usize,
}

impl Default for BashSettings {
    fn default() -> Self {
        Self {
            max_concurrent: 1,
            timeout_ms: 1000,
            max_stdout_bytes: 4096,
            max_vfs_bytes: 1_048_576,
        }
    }
}
