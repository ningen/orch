//! What the system decides on its own after a turn ends.
//!
//! The line it must not cross: every decision here is derived from evidence
//! (the receipt, the git state, the gate) exactly like the card
//! recommendations — the autopilot only presses the buttons a human would
//! have pressed anyway. Anything whose remaining decision is a human
//! judgment (failed checks, policy findings, no gate) stops the loop and
//! surfaces as an attention card. Every decision is recorded as a note so
//! the transcript always shows why.

use super::Task;
use crate::receipt::{AutopilotMode, ReceiptState, Step, classify, next_step};
use serde_json::json;
use std::sync::Arc;

impl Task {
    /// Whether a gate is configured for this task's repository.
    async fn gated(&self) -> bool {
        !self.settings().await.checks.is_empty()
    }

    /// The authoritative receipt state for this task, also served to the UI.
    pub async fn receipt_state_with(&self, behind: u64) -> ReceiptState {
        let (meta, _) = self.snapshot().await;
        classify(meta.verify.as_ref(), behind, self.gated().await)
    }

    pub async fn receipt_state(&self) -> ReceiptState {
        self.receipt_state_with(self.behind().await).await
    }

    /// Whether the receipt describes exactly the current worktree contents.
    /// A receipt can be recent and still describe older work.
    async fn receipt_is_current(&self) -> bool {
        let (meta, _) = self.snapshot().await;
        match &meta.verify {
            Some(v) if !v.fingerprint.is_empty() => {
                let current = self.fingerprint().await;
                !current.is_empty() && v.fingerprint == current
            }
            _ => false,
        }
    }

    /// Decide what still needs doing after a turn, and do whatever does not
    /// need a human judgment: rebase onto a moved base, verify, and — only
    /// when the mode allows and the gate is green with no policy findings —
    /// merge.
    pub(super) async fn autopilot(self: &Arc<Self>) {
        let mode = AutopilotMode::parse(&self.settings().await.autopilot);
        // Bounded by construction: rebase → verify → merge is the longest
        // path, and every step either advances it or stops the loop.
        for _ in 0..4 {
            let state = self.receipt_state().await;
            // A receipt that does not describe the current contents is no
            // receipt at all, whatever its status says.
            let step = if matches!(state, ReceiptState::Passed | ReceiptState::NeedsReview)
                && !self.receipt_is_current().await
            {
                Step::Verify
            } else {
                next_step(state, mode)
            };
            match step {
                Step::Stop => break,
                Step::Rebase => {
                    let behind = self.behind().await;
                    self.note(&format!(
                        "autopilot: the base moved {behind} commit{} — rebasing",
                        if behind == 1 { "" } else { "s" }
                    ))
                    .await;
                    if let Err(error) = self.rebase_core().await {
                        // A conflict is exactly the judgment the system will
                        // not make on its own.
                        self.note(&format!("autopilot stopped: {error:#}")).await;
                        break;
                    }
                }
                Step::Verify => {
                    self.note("autopilot: verifying against the current state")
                        .await;
                    if self.run_verify().await.is_err() {
                        // A failed check already shows on the card; deciding
                        // what to do about it belongs to a human.
                        break;
                    }
                }
                Step::Merge => {
                    self.note("autopilot: checks green, no policy findings — merging")
                        .await;
                    if let Err(error) = self.merge(false).await {
                        self.note(&format!("autopilot stopped: {error:#}")).await;
                    }
                    break;
                }
            }
        }
    }

    /// Record a system decision in the transcript, so a human can audit what
    /// was decided on their behalf and why.
    async fn note(&self, text: &str) {
        self.push_item(json!({
            "kind": "note",
            "id": format!("n_{}", uuid::Uuid::new_v4().simple()),
            "at": super::now_ms(),
            "text": text,
            "tone": "info",
        }))
        .await;
    }
}
