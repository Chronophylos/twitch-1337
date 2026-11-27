use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::settings::{Settings, SettingsHandle};

/// Per-user cooldown tracker.
///
/// Stores the last usage timestamp per user and checks whether the cooldown
/// period has elapsed. Thread-safe via internal `Mutex`. The duration is read
/// from settings on every check, so dashboard edits apply to the next
/// invocation without a restart.
pub struct PerUserCooldown {
    settings: SettingsHandle,
    getter: fn(&Settings) -> Duration,
    last_use: Mutex<HashMap<String, Instant>>,
}

impl PerUserCooldown {
    /// Create a cooldown tracker whose duration is read live from `settings`
    /// via `getter` on every check. The per-user timestamp map persists across
    /// turns; only the configured duration is re-read each call.
    pub fn live(settings: SettingsHandle, getter: fn(&Settings) -> Duration) -> Self {
        Self {
            settings,
            getter,
            last_use: Mutex::new(HashMap::new()),
        }
    }
}

impl PerUserCooldown {
    /// Atomically checks the cooldown and, if it has elapsed, charges it.
    ///
    /// Check and charge share one lock, so concurrent invocations from one
    /// user admit exactly one. Returns the charge stamp on success (only
    /// needed for [`Self::refund`]) or the remaining wait while still on
    /// cooldown.
    pub async fn try_acquire(&self, user: &str) -> Result<Instant, Duration> {
        let duration = (self.getter)(&self.settings.load());
        let mut guard = self.last_use.lock().await;
        let now = Instant::now();
        match guard.get_mut(user) {
            Some(last) => {
                let elapsed = now.saturating_duration_since(*last);
                if elapsed < duration {
                    return Err(duration - elapsed);
                }
                *last = now;
            }
            None => {
                guard.insert(user.to_owned(), now);
            }
        }
        Ok(now)
    }

    /// Gives back a charge from [`Self::try_acquire`] for an answer that turned
    /// out to be free. A no-op once a later charge has replaced `stamp`.
    pub async fn refund(&self, user: &str, stamp: Instant) {
        let mut guard = self.last_use.lock().await;
        if guard.get(user) == Some(&stamp) {
            guard.remove(user);
        }
    }
}

/// Formats a duration as compact hours+minutes (e.g., "1h12m", "45m").
/// Ignores seconds. Returns "0m" for durations under one minute.
pub fn format_duration_hm(d: Duration) -> String {
    let total_mins = d.as_secs() / 60;
    let hours = total_mins / 60;
    let mins = total_mins % 60;
    if hours > 0 {
        format!("{hours}h{mins}m")
    } else {
        format!("{mins}m")
    }
}

pub fn format_cooldown_remaining(remaining: Duration) -> String {
    let total_secs = remaining.as_secs();

    // Sub-second or zero: clamp to display value
    if total_secs == 0 {
        return if remaining.is_zero() {
            "0s".to_string()
        } else {
            "1s".to_string()
        };
    }

    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if hours > 0 {
        if minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else if minutes > 0 {
        if seconds > 0 {
            format!("{minutes}m {seconds}s")
        } else {
            format!("{minutes}m")
        }
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_remaining_cooldown() {
        let cases: &[(Duration, &str)] = &[
            (Duration::ZERO, "0s"),
            // Sub-second remainders round up so the reply never says "0s".
            (Duration::from_millis(100), "1s"),
            (Duration::from_millis(500), "1s"),
            (Duration::from_secs(1), "1s"),
            (Duration::from_secs(30), "30s"),
            (Duration::from_secs(59), "59s"),
            (Duration::from_secs(60), "1m"),
            (Duration::from_secs(63), "1m 3s"),
            (Duration::from_secs(120), "2m"),
            (Duration::from_secs(243), "4m 3s"),
            (Duration::from_secs(300), "5m"),
            (Duration::from_secs(3599), "59m 59s"),
            (Duration::from_secs(3600), "1h"),
            (Duration::from_secs(3900), "1h 5m"),
            (Duration::from_secs(7200), "2h"),
        ];
        for &(input, expected) in cases {
            assert_eq!(format_cooldown_remaining(input), expected, "{input:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_acquires_admit_one() {
        let cooldown = std::sync::Arc::new(PerUserCooldown::live(
            SettingsHandle::from_settings(Settings::compiled_defaults()),
            |_| Duration::from_secs(60),
        ));
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let cooldown = cooldown.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    cooldown.try_acquire("alice").await.is_ok()
                })
            })
            .collect();
        let mut admitted = 0;
        for task in tasks {
            if task.await.unwrap() {
                admitted += 1;
            }
        }
        assert_eq!(admitted, 1);
    }
}
