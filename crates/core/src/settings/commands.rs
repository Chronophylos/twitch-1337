//! Settings for the generic command layer.
//!
//! `execution` caps the background command execution manager: how many
//! accepted invocations run at once and how many wait in its FIFO queue,
//! globally, per user, and per command. Every admission and start decision
//! reads the live snapshot, so edits apply without a restart. Lowering a
//! limit never cancels a running invocation or drops a queued one; it only
//! gates new starts and new admissions.

use serde::{Deserialize, Serialize};

use super::knob::Knobs;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = CommandsOverrides)]
pub struct CommandsSettings {
    #[knob(nested)]
    pub execution: CommandExecutionSettings,
}

/// Admission limits of the background command execution manager. A pending
/// limit of `0` disables queueing at that level: an invocation that cannot
/// start right away is rejected with the capacity reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Knobs)]
#[knobs(overrides = CommandExecutionOverrides)]
pub struct CommandExecutionSettings {
    /// Background invocations running at once, across all users and commands.
    #[knob(min = 1, max = 128)]
    pub max_active: usize,
    /// Accepted invocations waiting for a free slot, across all users and
    /// commands.
    #[knob(min = 0, max = 1024)]
    pub max_pending: usize,
    /// Invocations one user may have running at once.
    #[knob(min = 1, max = 32)]
    pub max_active_per_user: usize,
    /// Invocations one user may have waiting at once.
    #[knob(min = 0, max = 64)]
    pub max_pending_per_user: usize,
    /// Invocations of one command running at once.
    #[knob(min = 1, max = 64)]
    pub max_active_per_command: usize,
    /// Invocations of one command waiting at once.
    #[knob(min = 0, max = 64)]
    pub max_pending_per_command: usize,
}

impl Default for CommandExecutionSettings {
    fn default() -> Self {
        Self {
            max_active: 32,
            max_pending: 128,
            max_active_per_user: 3,
            max_pending_per_user: 8,
            max_active_per_command: 8,
            max_pending_per_command: 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Settings, SettingsOverrides};

    /// A `settings.ron` without a `commands` section resolves to the limits
    /// the manager hard-coded before they became knobs.
    #[test]
    fn missing_section_resolves_to_previous_hard_coded_limits() {
        let overrides: SettingsOverrides = ron::from_str("()").expect("parse");
        let resolved = Settings::resolve(&Settings::compiled_defaults(), &overrides);
        assert_eq!(
            resolved.commands.execution,
            CommandExecutionSettings {
                max_active: 32,
                max_pending: 128,
                max_active_per_user: 3,
                max_pending_per_user: 8,
                max_active_per_command: 8,
                max_pending_per_command: 8,
            }
        );
    }
}
