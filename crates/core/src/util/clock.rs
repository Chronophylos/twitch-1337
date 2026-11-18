//! Wall-clock abstraction for time-dependent handlers.
//!
//! Production code uses [`SystemClock`], which delegates to
//! [`jiff::Timestamp::now`] and [`tokio::time::sleep`]. Integration tests
//! substitute a fake clock that the test body advances manually, paired
//! with `tokio::time::pause` so tokio-native sleeps also advance
//! deterministically.

use async_trait::async_trait;
use jiff::Timestamp;

#[async_trait]
pub trait Clock: Send + Sync {
    fn now_utc(&self) -> Timestamp;
    async fn sleep_until(&self, target: Timestamp);
}

#[derive(Debug, Default)]
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now_utc(&self) -> Timestamp {
        Timestamp::now()
    }

    async fn sleep_until(&self, target: Timestamp) {
        let delta = std::time::Duration::try_from(target - Timestamp::now())
            .unwrap_or(std::time::Duration::ZERO);
        tokio::time::sleep(delta).await;
    }
}
