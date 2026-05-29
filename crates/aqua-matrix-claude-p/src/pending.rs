//! The shared `ask_user` primitive — a pending-reply router that lets a run
//! pause and ask the authenticated user a question over the same Matrix
//! channel, then resolves on their next DM.
//!
//! This is the core capability reused by Phases A, B, and C of the
//! chat-confirmations plan; only *what triggers a question* and *what an answer
//! authorises* differ between phases. The router itself is phase-agnostic.
//!
//! ## Design
//!
//! The relay's [`MessageHandler::handle_message`] fires once per inbound DM
//! from `target`. Normally every DM starts a fresh `claude` run. While a run is
//! awaiting an answer we want the inverse: the user's *next* DM is the answer,
//! not a new prompt. [`PendingMap`] is the shared state that makes this work:
//!
//! - [`PendingMap::ask`] registers a one-shot keyed by `target`, sends the
//!   question, and awaits the answer with a timeout — **default-DENY** on
//!   timeout so a silent user can never wedge the `claude` process.
//! - [`PendingMap::try_resolve`] is called first thing in `handle_message`; if
//!   a question is pending for `target` it consumes the DM as the answer
//!   (returning `true`) instead of letting it start a new run.
//!
//! Exactly one question per `target` is open at a time (the relay already runs
//! ~one conversation per user, and [`PendingMap::ask`] overwrites/aborts any
//! stale entry), so there is no question/answer correlation ambiguity.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aqua_matrix_relay::AgentClient;
use tokio::sync::oneshot;

/// Shared pending-question state, embedded in the handler and cloned into each
/// run task. Cheap to clone (`Arc`).
#[derive(Clone, Default)]
pub struct PendingMap {
    // did (target MXID) -> sender that resolves that target's open question.
    // A std `Mutex` is fine: every critical section is a HashMap insert/remove
    // with no `.await` held across the lock.
    inner: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
}

impl PendingMap {
    /// Is a question currently open for `target`? Part of the `ask_user` router
    /// surface reused by Phases B/C; Phase A's flow uses [`Self::try_resolve`]
    /// instead, hence `allow(dead_code)` for now.
    #[allow(dead_code)]
    pub fn is_pending(&self, target: &str) -> bool {
        self.inner
            .lock()
            .map(|m| m.contains_key(target))
            .unwrap_or(false)
    }

    /// If a question is pending for `target`, resolve it with `body` and return
    /// `true` (the DM was the answer). Otherwise return `false` (the caller
    /// should treat the DM as a fresh prompt). Called first in
    /// `handle_message`.
    pub fn try_resolve(&self, target: &str, body: &str) -> bool {
        let sender = {
            let Ok(mut map) = self.inner.lock() else {
                // A poisoned lock means a previous holder panicked; fail closed
                // (treat as no pending question) rather than risk acting on
                // corrupt state.
                return false;
            };
            map.remove(target)
        };
        match sender {
            Some(tx) => {
                // Receiver may have already dropped on timeout; ignore the err.
                let _ = tx.send(body.to_string());
                true
            }
            None => false,
        }
    }

    /// Ask `target` `question` and await their next DM as the answer, up to
    /// `timeout`. Returns `Some(answer)` or `None` on timeout / send failure /
    /// dropped channel — callers MUST treat `None` as a **deny** (fail closed).
    ///
    /// Registering the one-shot BEFORE sending the question closes the race
    /// where a very fast reply arrives before the entry exists.
    pub async fn ask(
        &self,
        agent: &AgentClient,
        target: &str,
        question: &str,
        timeout: Duration,
    ) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        {
            let Ok(mut map) = self.inner.lock() else {
                tracing::warn!("ask_user: pending map lock poisoned; denying");
                return None;
            };
            // Overwrite any stale entry: only one question per target is valid.
            // Dropping the previous sender resolves its receiver to Err → deny.
            if map.insert(target.to_string(), tx).is_some() {
                tracing::warn!("ask_user: replaced a stale pending question for {target}");
            }
        }

        if let Err(e) = agent.send_dm(target, question).await {
            tracing::warn!("ask_user: failed to send question to {target}: {e:#}");
            self.clear(target);
            return None;
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) => Some(answer),
            Ok(Err(_)) => {
                // Sender dropped (e.g. overwritten by a newer ask). Deny.
                tracing::warn!("ask_user: pending channel for {target} dropped; denying");
                None
            }
            Err(_) => {
                // Timed out — remove our entry so a late reply isn't mistaken
                // for an answer, and deny.
                tracing::info!("ask_user: question to {target} timed out; denying");
                self.clear(target);
                None
            }
        }
    }

    /// Drop any pending entry for `target` without resolving it.
    fn clear(&self, target: &str) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The Matrix-touching `ask` path needs a live AgentClient, so the unit
    // tests cover the pure routing logic — the part with the subtle races.

    #[tokio::test]
    async fn resolve_delivers_answer_to_waiter() {
        let map = PendingMap::default();
        let (tx, rx) = oneshot::channel();
        map.inner
            .lock()
            .unwrap()
            .insert("@u:hs".into(), tx);

        assert!(map.is_pending("@u:hs"));
        assert!(map.try_resolve("@u:hs", "yes"));
        assert_eq!(rx.await.unwrap(), "yes");
        // Entry consumed: a second resolve is a no-op (fresh prompt).
        assert!(!map.try_resolve("@u:hs", "again"));
        assert!(!map.is_pending("@u:hs"));
    }

    #[tokio::test]
    async fn no_pending_means_fresh_prompt() {
        let map = PendingMap::default();
        // Nothing registered → the DM is NOT consumed as an answer.
        assert!(!map.try_resolve("@u:hs", "delete everything"));
        assert!(!map.is_pending("@u:hs"));
    }

    #[tokio::test]
    async fn questions_are_per_target() {
        let map = PendingMap::default();
        let (tx_a, rx_a) = oneshot::channel();
        let (tx_b, rx_b) = oneshot::channel();
        map.inner.lock().unwrap().insert("@a:hs".into(), tx_a);
        map.inner.lock().unwrap().insert("@b:hs".into(), tx_b);

        // Answering A must not touch B's pending question.
        assert!(map.try_resolve("@a:hs", "yes"));
        assert_eq!(rx_a.await.unwrap(), "yes");
        assert!(map.is_pending("@b:hs"));
        assert!(map.try_resolve("@b:hs", "no"));
        assert_eq!(rx_b.await.unwrap(), "no");
    }

    #[tokio::test]
    async fn overwriting_a_pending_question_denies_the_old_waiter() {
        // Simulates `ask` replacing a stale entry: the old receiver resolves to
        // Err, which the caller maps to a deny.
        let map = PendingMap::default();
        let (tx_old, rx_old) = oneshot::channel::<String>();
        map.inner.lock().unwrap().insert("@u:hs".into(), tx_old);

        // New ask overwrites — drop the old sender.
        let (tx_new, _rx_new) = oneshot::channel::<String>();
        map.inner.lock().unwrap().insert("@u:hs".into(), tx_new);

        // Old waiter now sees a closed channel → deny.
        assert!(rx_old.await.is_err());
    }
}
