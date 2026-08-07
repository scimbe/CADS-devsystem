//! `devsystem.improve`'s first real piece: analyze a run's own history to find
//! something the pipeline itself should flag, not just execute the next stage.
//! Deliberately mechanical, not speculative -- every signal here is derived directly
//! from [`RunState`], nothing invented (#382).

use crate::runner::RunState;

/// Stage ids that were proposed and added to the live spec (`state.added_stages`) but
/// have never actually had a **successful** iteration run *as* that stage. A stage can
/// sit in the spec, auction-able in principle, while nothing has delivered as it yet --
/// e.g. a proposal is blocking on a human decision before implementation starts. This
/// doesn't say *why* a stage is stalled (the pipeline has no visibility into that -- a
/// pending GitHub issue reply isn't observable from here), only that it mechanically is.
///
/// Real evaluator finding, issue #53: this used to clear on the mere *existence* of a
/// matching iteration record, regardless of `succeeded` -- a one-way latch, since a
/// single `succeeded: false` attempt (including one whose own feedback says it did
/// nothing) permanently silenced the badge for the rest of the run's life, no re-arming
/// possible. Live-confirmed on the actual flagship `webconference-android` run: three of
/// its five added stages (`devsystem.document_extraction`, `devsystem.android_emulator_test`,
/// `devsystem.android_native_build_ci` -- exactly the ones blocked on real external infra,
/// tracked as issues #12/#13/#14) have never once produced a successful iteration, and
/// `stalled_stages` reported none of them. Fixed to key on "no *succeeded* iteration has
/// ever run as this stage," matching the panel's own stated copy ("no iteration has ever
/// run as these stages") without rewording it, and making the signal re-armable in the
/// only direction that matters: a real success clears it, a failed attempt does not.
pub fn stalled_stages(state: &RunState) -> Vec<String> {
    state
        .added_stages
        .iter()
        .filter(|stage_id| !state.history.iter().any(|record| &record.stage == *stage_id && record.succeeded))
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

    fn failed_iteration(stage: &str) -> IterationRecord {
        IterationRecord { succeeded: false, ..iteration(stage) }
    }

    #[test]
    /// Real evaluator finding, issue #53, live-reproduced on a fresh probe run and
    /// confirmed already true of the actual flagship webconference-android run: a
    /// `succeeded: false` iteration used to clear this exactly like a real success would
    /// -- including one whose own feedback plainly says it did nothing. This is the
    /// single most important case in the whole module: the badge exists specifically to
    /// catch a role that has never delivered, and a failed attempt is evidence FOR that,
    /// not against it.
    fn a_failed_iteration_does_not_clear_stalled() {
        let mut state = RunState::new("run-improve");
        state.added_stages.push("devsystem.document_extraction".into());
        state.history.push(failed_iteration("devsystem.document_extraction"));
        assert_eq!(
            stalled_stages(&state),
            vec!["devsystem.document_extraction".to_string()],
            "a failed attempt is not delivered work -- the stage is still genuinely stalled"
        );
    }

    #[test]
    fn a_later_real_success_after_earlier_failures_does_clear_stalled() {
        let mut state = RunState::new("run-improve");
        state.added_stages.push("devsystem.document_extraction".into());
        state.history.push(failed_iteration("devsystem.document_extraction"));
        state.history.push(failed_iteration("devsystem.document_extraction"));
        state.history.push(iteration("devsystem.document_extraction"));
        assert!(stalled_stages(&state).is_empty(), "a real, later success re-arms the signal off, same as it always could");
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
