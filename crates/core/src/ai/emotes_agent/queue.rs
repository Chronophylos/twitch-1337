use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};

const QUEUE_CAPACITY: usize = 256;

/// One background generation request. The name is captured with the add/edit
/// action so a disabled or renamed catalog row cannot change the request's
/// meaning before the worker reaches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EmoteAgentRequest {
    Emote { emote_id: String, name: String },
    RunNow,
}

/// Non-blocking producer shared with 7TV ingest and the dashboard. The single
/// worker owns the receiver through a mutex so construction does not need a
/// second runtime-only field in `Services`.
#[derive(Clone)]
pub struct EmotesAgentHandle {
    tx: mpsc::Sender<EmoteAgentRequest>,
    rx: Arc<Mutex<mpsc::Receiver<EmoteAgentRequest>>>,
}

impl Default for EmotesAgentHandle {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        Self {
            tx,
            rx: Arc::new(Mutex::new(rx)),
        }
    }
}

impl EmotesAgentHandle {
    /// Queue one emote without waiting for the worker. `false` means the
    /// bounded queue is full or the worker has stopped; nightly tiers recover
    /// missed add/manual work where applicable.
    pub fn regenerate(&self, emote_id: &str, name: &str) -> bool {
        self.tx
            .try_send(EmoteAgentRequest::Emote {
                emote_id: emote_id.to_owned(),
                name: name.to_owned(),
            })
            .is_ok()
    }

    /// Ask the worker to run one nightly-priority batch immediately.
    pub fn run_now(&self) -> bool {
        self.tx.try_send(EmoteAgentRequest::RunNow).is_ok()
    }

    pub(crate) async fn recv(&self) -> Option<EmoteAgentRequest> {
        self.rx.lock().await.recv().await
    }
}
