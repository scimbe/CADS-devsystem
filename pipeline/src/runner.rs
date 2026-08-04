//! Drives one real run's "super loop": apply an [`IterationRecord`]'s proposals to the
//! live spec, track consecutive failures, and decide whether the run continues, must
//! pause for a human check-in, or aborts -- the actual glue between the primitives in
//! `lib.rs` and a persisted, resumable run (#382).

use crate::{apply_proposal, plan_only_spec, should_abort, should_checkin, AbortCriteria, IterationRecord, ProposalOutcome};
use ct_common::pipeline::PipelineSpec;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::path::Path;

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
    /// This run's own bounded-loop criteria -- starts at [`AbortCriteria::default`] but
    /// a human can tune it per run (e.g. a run that's earned trust doesn't need a
    /// check-in every 5 iterations). `#[serde(default)]` so `state.json` files written
    /// before this field existed still load, falling back to the same defaults every
    /// run used to be hardcoded to.
    #[serde(default)]
    pub criteria: AbortCriteria,
    /// Explicit human "stop, let me correct something" -- operator feedback: "ich
    /// weiss nicht... wie ich es anhalten kann um es zu korrigieren." Distinct from
    /// `CheckinDue`/`Abort` (which the run reaches on its own bounded-loop cadence):
    /// this is set/cleared only by a human action (the GUI's Pause/Resume button or
    /// the equivalent API calls), never by `run_iteration` itself. While `true`,
    /// `iterate_run` refuses new iterations with a clear error instead of silently
    /// accepting them. `#[serde(default)]` so pre-existing `state.json` files (none
    /// paused, obviously) still load.
    #[serde(default)]
    pub paused: bool,
}

impl RunState {
    pub fn new(run_id: impl Into<String>) -> Self {
        RunState {
            run_id: run_id.into(),
            consecutive_failures: 0,
            history: Vec::new(),
            added_stages: Vec::new(),
            criteria: AbortCriteria::default(),
            paused: false,
        }
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

/// Load a run's persisted `spec.json`/`state.json` from `run_dir`, or start fresh
/// (a new `plan_only_spec` + empty `RunState`) if this is the run's first iteration.
/// The actual load-or-init logic behind `devsystem_iterate` -- pulled out here so
/// it's unit-testable directly, without spawning the binary as a subprocess.
pub fn load_or_init_run(run_dir: &Path, run_id: &str) -> Result<(PipelineSpec, RunState), Box<dyn Error>> {
    let spec_path = run_dir.join("spec.json");
    let state_path = run_dir.join("state.json");

    let spec = if spec_path.exists() {
        serde_json::from_str(&fs::read_to_string(&spec_path)?)?
    } else {
        plan_only_spec(run_id, None)
    };
    let state = if state_path.exists() {
        serde_json::from_str(&fs::read_to_string(&state_path)?)?
    } else {
        RunState::new(run_id.to_string())
    };
    Ok((spec, state))
}

/// Persist a run's spec + state to `run_dir`, creating it if needed. The write side
/// of the same load/persist round-trip `devsystem_iterate` performs every real
/// invocation.
pub fn persist_run(run_dir: &Path, spec: &PipelineSpec, state: &RunState) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(run_dir)?;
    fs::write(run_dir.join("spec.json"), serde_json::to_string_pretty(spec)?)?;
    fs::write(run_dir.join("state.json"), serde_json::to_string_pretty(state)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::STAGE_IMPLEMENT;

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
            price_ceiling: None,
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

    fn temp_run_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("devsystem-runner-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn load_or_init_starts_fresh_when_no_files_exist_yet() {
        let dir = temp_run_dir("fresh");
        let (spec, state) = load_or_init_run(&dir, "a-new-run").unwrap();
        assert_eq!(spec.roles.len(), 1, "plan_only_spec's single starting role");
        assert_eq!(state.run_id, "a-new-run");
        assert!(state.history.is_empty());
        assert!(!dir.exists(), "load_or_init never creates the directory itself -- persist_run does");
    }

    #[test]
    fn persist_then_load_round_trips_a_real_spec_and_state() {
        let dir = temp_run_dir("roundtrip");
        let mut spec = plan_only_spec("roundtrip-run", None);
        let mut state = RunState::new("roundtrip-run");
        let criteria = AbortCriteria::default();
        run_iteration(&mut spec, &mut state, record(1, true, vec![]), &criteria);

        persist_run(&dir, &spec, &state).unwrap();
        assert!(dir.join("spec.json").exists());
        assert!(dir.join("state.json").exists());

        let (loaded_spec, loaded_state) = load_or_init_run(&dir, "roundtrip-run").unwrap();
        assert_eq!(loaded_spec, spec);
        assert_eq!(loaded_state.run_id, state.run_id);
        assert_eq!(loaded_state.history.len(), 1);

        fs::remove_dir_all(&dir).unwrap();
    }
}
