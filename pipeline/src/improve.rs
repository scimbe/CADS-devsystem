//! `devsystem.improve`'s first real piece: analyze a run's own history to find
//! something the pipeline itself should flag, not just execute the next stage.
//! Deliberately mechanical, not speculative -- every signal here is derived directly
//! from [`RunState`], nothing invented (#382).

use crate::runner::RunState;

/// Stage ids that were proposed and added to the live spec (`state.added_stages`) but
/// have never actually had an iteration run *as* that stage. A stage can sit in the
/// spec, auction-able in principle, while nothing has filled it yet -- e.g. a proposal
/// is blocking on a human decision before implementation starts. This doesn't say
/// *why* a stage is stalled (the pipeline has no visibility into that -- a pending
/// GitHub issue reply isn't observable from here), only that it mechanically is.
pub fn stalled_stages(state: &RunState) -> Vec<String> {
    state
        .added_stages
        .iter()
        .filter(|stage_id| !state.history.iter().any(|record| &record.stage == *stage_id))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IterationRecord;

    fn iteration(stage: &str) -> IterationRecord {
        IterationRecord {
            run_id: "run-improve".into(),
            stage: stage.into(),
            iteration: 1,
            feedback: "test".into(),
            proposals: vec![],
            succeeded: true,
            requirement_indices: Vec::new(),
            ..Default::default()
        }
    }

    #[test]
    fn no_stalled_stages_when_added_stages_is_empty() {
        let state = RunState::new("run-improve");
        assert!(stalled_stages(&state).is_empty());
    }

    #[test]
    fn a_stage_with_no_matching_iteration_is_stalled() {
        let mut state = RunState::new("run-improve");
        state.added_stages.push("devsystem.android_native_bridge".into());
        state.history.push(iteration("devsystem.implement")); // proposed it, never ran as it
        assert_eq!(stalled_stages(&state), vec!["devsystem.android_native_bridge".to_string()]);
    }

    #[test]
    fn a_stage_that_has_actually_run_is_not_stalled() {
        let mut state = RunState::new("run-improve");
        state.added_stages.push("devsystem.test".into());
        state.history.push(iteration("devsystem.implement"));
        state.history.push(iteration("devsystem.test"));
        assert!(stalled_stages(&state).is_empty());
    }

    #[test]
    fn mixed_run_reports_only_the_genuinely_stalled_stage() {
        let mut state = RunState::new("run-improve");
        state.added_stages.push("devsystem.android_native_bridge".into());
        state.added_stages.push("devsystem.test".into());
        state.added_stages.push("devsystem.verify".into());
        state.history.push(iteration("devsystem.implement"));
        state.history.push(iteration("devsystem.test"));
        state.history.push(iteration("devsystem.verify"));
        assert_eq!(stalled_stages(&state), vec!["devsystem.android_native_bridge".to_string()]);
    }
}
