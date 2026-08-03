//! Drives one real run's "super loop": apply an [`IterationRecord`]'s proposals to the
//! live spec, track consecutive failures, and decide whether the run continues, must
//! pause for a human check-in, or aborts -- the actual glue between the primitives in
//! `lib.rs` and a persisted, resumable run (#382).

use crate::{apply_proposal, should_abort, should_checkin, AbortCriteria, IterationRecord, ProposalOutcome};
use ct_common::pipeline::PipelineSpec;
use serde::{Deserialize, Serialize};

/// Persisted state for one run -- serialized to `runs/<run_id>/state.json` in the
/// coordination repo so a run survives across separate loop firings (each firing is a
/// fresh process; nothing here is in-memory-only).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunState {
    pub run_id: String,
    pub consecutive_failures: u32,
    pub history: Vec<IterationRecord>,
    /// `stage_id`s of every proposal that actually got added to the live spec, in the
    /// order they were added -- the run's own record of how the pipeline grew itself.
    pub added_stages: Vec<String>,
}

impl RunState {
    pub fn new(run_id: impl Into<String>) -> Self {
        RunState { run_id: run_id.into(), consecutive_failures: 0, history: Vec::new(), added_stages: Vec::new() }
    }
}

/// What the runner decided after folding in one [`IterationRecord`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// Bounds not hit -- the next iteration can run without human input.
    Continue,
    /// A check-in is due (cadence or ceiling) -- the run must pause here for a real
    /// human response (ecc-plan-canvas / a GitHub issue comment) before continuing.
    CheckinDue,
    /// Consecutive-failure or hard-ceiling bound was hit -- the run stops.
    Abort,
}

/// Fold one real iteration into `state`, mutating `spec` with any proposals it carries,
/// and return what should happen next. This is the only place `apply_proposal` +
/// `should_checkin` + `should_abort` are wired together end to end.
pub fn run_iteration(
    spec: &mut PipelineSpec,
    state: &mut RunState,
    record: IterationRecord,
    criteria: &AbortCriteria,
) -> RunOutcome {
    for proposal in &record.proposals {
        if apply_proposal(spec, proposal) == ProposalOutcome::Added {
            state.added_stages.push(proposal.stage_id.clone());
        }
    }

    if record.succeeded {
        state.consecutive_failures = 0;
    } else {
        state.consecutive_failures += 1;
    }
    let iteration = record.iteration;
    let checkin_check = should_checkin(&record, criteria);
    state.history.push(record);

    if should_abort(state.consecutive_failures, iteration, criteria) {
        RunOutcome::Abort
    } else if checkin_check {
        RunOutcome::CheckinDue
    } else {
        RunOutcome::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{plan_only_spec, STAGE_IMPLEMENT};

    fn record(iteration: u32, succeeded: bool, proposals: Vec<crate::StageProposal>) -> IterationRecord {
        IterationRecord {
            run_id: "run-x".into(),
            stage: STAGE_IMPLEMENT.into(),
            iteration,
            feedback: "test feedback".into(),
            proposals,
            succeeded,
        }
    }

    #[test]
    fn a_proposal_carried_by_an_iteration_is_applied_and_tracked() {
        let mut spec = plan_only_spec("run-x", None);
        let mut state = RunState::new("run-x");
        let criteria = AbortCriteria::default();

        let proposal = crate::StageProposal {
            proposed_by: STAGE_IMPLEMENT.into(),
            stage_id: "devsystem.android_jni_bridge".into(),
            tag: "android_jni_bridge".into(),
            rationale: "test".into(),
            use_existing_service: None,
            units: 1,
        };
        let outcome = run_iteration(&mut spec, &mut state, record(1, true, vec![proposal]), &criteria);

        assert_eq!(outcome, RunOutcome::Continue);
        assert_eq!(spec.roles.len(), 2, "the proposed role was actually added to the live spec");
        assert_eq!(state.added_stages, vec!["devsystem.android_jni_bridge".to_string()]);
        assert_eq!(state.history.len(), 1);
    }

    #[test]
    fn checkin_cadence_pauses_the_run_without_aborting_it() {
        let mut spec = plan_only_spec("run-x", None);
        let mut state = RunState::new("run-x");
        let criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 3, checkin_every: 5 };

        let outcome = run_iteration(&mut spec, &mut state, record(5, true, vec![]), &criteria);
        assert_eq!(outcome, RunOutcome::CheckinDue);
    }

    #[test]
    fn consecutive_failures_abort_the_run() {
        let mut spec = plan_only_spec("run-x", None);
        let mut state = RunState::new("run-x");
        let criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 2, checkin_every: 5 };

        assert_eq!(run_iteration(&mut spec, &mut state, record(1, false, vec![]), &criteria), RunOutcome::Continue);
        assert_eq!(run_iteration(&mut spec, &mut state, record(2, false, vec![]), &criteria), RunOutcome::Abort);
        assert_eq!(state.consecutive_failures, 2);
    }

    #[test]
    fn a_success_after_failures_resets_the_consecutive_counter() {
        let mut spec = plan_only_spec("run-x", None);
        let mut state = RunState::new("run-x");
        let criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 3, checkin_every: 5 };

        run_iteration(&mut spec, &mut state, record(1, false, vec![]), &criteria);
        run_iteration(&mut spec, &mut state, record(2, true, vec![]), &criteria);
        assert_eq!(state.consecutive_failures, 0);
    }
}
