//! The single source of truth for "what state is this task's work in".
//!
//! Both the Rust gate (`commit_worktree`), the autopilot, and the UI consume
//! this one state machine, so a receipt can never mean one thing to the
//! server and another to the client. The input is evidence only — the
//! receipt, the git state, and whether a gate exists — never what the agent
//! says about its own work.

use crate::task::Verify;

/// What the last receipt says about the work in a task's worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptState {
    /// A check failed. A human (or the agent, steered by one) must look.
    Failed,
    /// Checks were green but the diff policy flagged something. Merging
    /// needs an explicit human override.
    NeedsReview,
    /// The receipt passed, but against a base that has since moved. It no
    /// longer describes what would land.
    Stale,
    /// Checks passed against the current base; nothing is flagged.
    Passed,
    /// A gate is configured but no receipt describes this worktree yet.
    Unverified,
    /// No gate is configured for this repository at all.
    Ungated,
}

impl ReceiptState {
    pub fn as_str(self) -> &'static str {
        match self {
            ReceiptState::Failed => "failed",
            ReceiptState::NeedsReview => "needs_review",
            ReceiptState::Stale => "stale",
            ReceiptState::Passed => "passed",
            ReceiptState::Unverified => "unverified",
            ReceiptState::Ungated => "ungated",
        }
    }
}

/// Classify a task's work from evidence only.
pub fn classify(verify: Option<&Verify>, behind: u64, gated: bool) -> ReceiptState {
    let Some(v) = verify else {
        return if gated {
            ReceiptState::Unverified
        } else {
            ReceiptState::Ungated
        };
    };
    match v.status.as_str() {
        "failed" => ReceiptState::Failed,
        "needs_review" => ReceiptState::NeedsReview,
        _ if behind > 0 => ReceiptState::Stale,
        _ => ReceiptState::Passed,
    }
}

/* -------------------------------------------------------------- autopilot */

/// How much the system is allowed to decide on its own after a turn ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AutopilotMode {
    /// Decide nothing; every action goes through a human.
    Off,
    /// Rebase and re-verify on its own, so receipts stay current. Merging
    /// stays a human decision.
    #[default]
    KeepFresh,
    /// Everything in `KeepFresh`, plus merge when the gate is green with no
    /// policy findings. Never merges without a gate.
    AutoMerge,
}

impl AutopilotMode {
    pub fn parse(value: &str) -> AutopilotMode {
        match value.trim() {
            "off" => AutopilotMode::Off,
            "auto-merge" => AutopilotMode::AutoMerge,
            _ => AutopilotMode::KeepFresh,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AutopilotMode::Off => "off",
            AutopilotMode::KeepFresh => "keep-fresh",
            AutopilotMode::AutoMerge => "auto-merge",
        }
    }
}

/// One step the system may take without asking. The loop in
/// `task::autopilot` re-derives this after every step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Replay the task branch onto the current base (auto-aborts on
    /// conflict, which stops the loop and surfaces the card).
    Rebase,
    /// Run the configured checks and record a receipt.
    Verify,
    /// Merge the worktree into the base branch.
    Merge,
    /// Nothing left the system may decide; the rest belongs to a human.
    Stop,
}

/// The next step the system takes on its own for a task in this state.
/// `Stop` covers everything whose remaining decision is a human judgment:
/// failed checks, policy findings, no gate, or an idle task.
pub fn next_step(state: ReceiptState, mode: AutopilotMode) -> Step {
    match mode {
        AutopilotMode::Off => Step::Stop,
        AutopilotMode::KeepFresh | AutopilotMode::AutoMerge => match state {
            ReceiptState::Stale => Step::Rebase,
            ReceiptState::Unverified => Step::Verify,
            ReceiptState::Passed if mode == AutopilotMode::AutoMerge => Step::Merge,
            _ => Step::Stop,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verify(status: &str) -> Verify {
        Verify {
            status: status.into(),
            ..Verify::default()
        }
    }

    #[test]
    fn classifies_from_evidence() {
        assert_eq!(classify(None, 0, false), ReceiptState::Ungated);
        assert_eq!(classify(None, 0, true), ReceiptState::Unverified);
        assert_eq!(
            classify(Some(&verify("failed")), 0, true),
            ReceiptState::Failed
        );
        assert_eq!(
            classify(Some(&verify("needs_review")), 3, true),
            ReceiptState::NeedsReview
        );
        // A passed receipt describes an old base once the base moves.
        assert_eq!(
            classify(Some(&verify("passed")), 1, true),
            ReceiptState::Stale
        );
        assert_eq!(
            classify(Some(&verify("passed")), 0, true),
            ReceiptState::Passed
        );
    }

    #[test]
    fn off_mode_never_acts() {
        for state in [
            ReceiptState::Failed,
            ReceiptState::NeedsReview,
            ReceiptState::Stale,
            ReceiptState::Passed,
            ReceiptState::Unverified,
            ReceiptState::Ungated,
        ] {
            assert_eq!(next_step(state, AutopilotMode::Off), Step::Stop);
        }
    }

    #[test]
    fn keep_fresh_refreshes_but_never_merges() {
        assert_eq!(
            next_step(ReceiptState::Stale, AutopilotMode::KeepFresh),
            Step::Rebase
        );
        assert_eq!(
            next_step(ReceiptState::Unverified, AutopilotMode::KeepFresh),
            Step::Verify
        );
        assert_eq!(
            next_step(ReceiptState::Passed, AutopilotMode::KeepFresh),
            Step::Stop
        );
        assert_eq!(
            next_step(ReceiptState::Failed, AutopilotMode::KeepFresh),
            Step::Stop
        );
        assert_eq!(
            next_step(ReceiptState::NeedsReview, AutopilotMode::KeepFresh),
            Step::Stop
        );
    }

    #[test]
    fn auto_merge_merges_only_a_clean_pass() {
        assert_eq!(
            next_step(ReceiptState::Passed, AutopilotMode::AutoMerge),
            Step::Merge
        );
        // No gate configured: merging would be deciding without evidence.
        assert_eq!(
            next_step(ReceiptState::Ungated, AutopilotMode::AutoMerge),
            Step::Stop
        );
        assert_eq!(
            next_step(ReceiptState::NeedsReview, AutopilotMode::AutoMerge),
            Step::Stop
        );
        assert_eq!(
            next_step(ReceiptState::Failed, AutopilotMode::AutoMerge),
            Step::Stop
        );
    }

    #[test]
    fn mode_roundtrips() {
        for mode in [
            AutopilotMode::Off,
            AutopilotMode::KeepFresh,
            AutopilotMode::AutoMerge,
        ] {
            assert_eq!(AutopilotMode::parse(mode.as_str()), mode);
        }
        // Unknown values fall back to the default, not to "off".
        assert_eq!(AutopilotMode::parse("nonsense"), AutopilotMode::KeepFresh);
        assert_eq!(AutopilotMode::parse(""), AutopilotMode::KeepFresh);
    }
}
