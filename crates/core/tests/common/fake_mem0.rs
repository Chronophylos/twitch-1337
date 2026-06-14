//! A fake [`MemoryApi`] that returns pre-queued search results and records
//! `add` / `delete_user` calls.
//!
//! Tests queue search results with [`FakeMem0::push_search`] before the turn,
//! then inspect [`FakeMem0::adds`] / [`FakeMem0::deletes`] afterwards. The
//! `add` is `tokio::spawn`ed fire-and-forget by the production path, so tests
//! must poll `adds()` rather than asserting immediately.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use eyre::{Result, eyre};
use twitch_1337_core::ai::mem0::{MemoryApi, MemoryHit, Scope, TurnMessage};

#[derive(Default)]
pub struct FakeMem0 {
    /// Queued search results: each `search` pops one front (or returns empty).
    searches: Mutex<VecDeque<Vec<MemoryHit>>>,
    /// Recorded `add` calls: `(messages, user_id, agent_id)`.
    adds: Mutex<Vec<(Vec<TurnMessage>, String, String)>>,
    /// Recorded `delete_user` calls (the user_id argument).
    deletes: Mutex<Vec<String>>,
    /// When set, every `search` returns `Err` (degradation knob).
    search_errors: AtomicBool,
}

impl FakeMem0 {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one search result set, served on the next `search` call.
    pub fn push_search(&self, hits: Vec<MemoryHit>) {
        self.searches.lock().unwrap().push_back(hits);
    }

    /// Make every `search` return an error (used to test degradation).
    pub fn set_search_errors(&self, on: bool) {
        self.search_errors.store(on, Ordering::SeqCst);
    }

    pub fn adds(&self) -> Vec<(Vec<TurnMessage>, String, String)> {
        self.adds.lock().unwrap().clone()
    }

    pub fn deletes(&self) -> Vec<String> {
        self.deletes.lock().unwrap().clone()
    }
}

#[async_trait]
impl MemoryApi for FakeMem0 {
    async fn search(&self, _query: &str, _scope: Scope<'_>) -> Result<Vec<MemoryHit>> {
        if self.search_errors.load(Ordering::SeqCst) {
            return Err(eyre!("fake mem0 search error"));
        }
        Ok(self
            .searches
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default())
    }

    async fn add(
        &self,
        messages: &[TurnMessage],
        user_id: &str,
        agent_id: &str,
        _metadata: serde_json::Value,
    ) -> Result<()> {
        self.adds.lock().unwrap().push((
            messages.to_vec(),
            user_id.to_owned(),
            agent_id.to_owned(),
        ));
        Ok(())
    }

    async fn delete_user(&self, user_id: &str) -> Result<()> {
        self.deletes.lock().unwrap().push(user_id.to_owned());
        Ok(())
    }

    async fn delete_one(&self, _memory_id: &str) -> Result<()> {
        Ok(())
    }

    async fn list_user(&self, _user_id: &str) -> Result<Vec<MemoryHit>> {
        Ok(Vec::new())
    }
}
