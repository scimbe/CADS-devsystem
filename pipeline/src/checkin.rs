//! Renders a run's pending check-in as a `.plan.md` artifact for `ecc-plan-canvas`
//! (`docs/plan-stage.md`) -- the second real delivery channel for a checkpoint, not
//! just a GitHub issue comment. Pure rendering only; the actual `ecc-plan-canvas open`
//! invocation lives in the `devsystem_checkin` binary, since spawning a process isn't
//! something a hermetic `cargo test` should do.

use crate::runner::RunState;
use crate::IterationRecord;

/// Render the most recent [`IterationRecord`] in `state` as a Plan Canvas markdown
/// artifact -- the zylos envelope's `key_findings` shape (`docs/role-contracts.md`):
/// what was asked, what was found, what's proposed, and the explicit question a human
/// needs to answer. Returns `None` if the run has no history yet (nothing to check in).
pub fn render_plan_markdown(state: &RunState) -> Option<String> {
    let latest = state.history.last()?;
    Some(render_iteration(state, latest))
}

fn render_iteration(state: &RunState, record: &IterationRecord) -> String {
    let mut md = String::new();
    md.push_str(&format!("# Check-in: `{}` -- iteration {}\n\n", state.run_id, record.iteration));
    md.push_str(&format!("**Stage:** `{}`\n\n", record.stage));
    md.push_str("## What this stage found\n\n");
    md.push_str(&record.feedback);
    md.push_str("\n\n");

    if record.proposals.is_empty() {
        md.push_str("## Proposals\n\nNone this iteration.\n\n");
    } else {
        md.push_str("## Proposals\n\n");
        for p in &record.proposals {
            md.push_str(&format!("### `{}`\n\n", p.stage_id));
            md.push_str(&format!("- **Proposed by:** `{}`\n", p.proposed_by));
            md.push_str(&format!("- **Tag / units:** `{}` / {}\n", p.tag, p.units));
            let reuse = p.use_existing_service.as_deref().unwrap_or("none -- a new service must be built or provided");
            md.push_str(&format!("- **Existing service to reuse:** {reuse}\n\n"));
            md.push_str(&format!("{}\n\n", p.rationale));
        }
    }

    md.push_str("## Stages added to the live spec so far\n\n");
    if state.added_stages.is_empty() {
        md.push_str("None yet.\n\n");
    } else {
        for s in &state.added_stages {
            md.push_str(&format!("- `{s}`\n"));
        }
        md.push('\n');
    }

    md.push_str("## Decision needed\n\n");
    md.push_str("Reply `approve` to accept this iteration's proposals as-is and let the next \
        iteration proceed, or `request-changes` with your answer/direction (this canvas \
        live-reloads on `--reply`).\n");
    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IterationRecord, StageProposal};

    fn state_with_one_iteration(proposals: Vec<StageProposal>) -> RunState {
        let mut state = RunState::new("run-checkin");
        state.history.push(IterationRecord {
            run_id: "run-checkin".into(),
            stage: "devsystem.implement".into(),
            iteration: 1,
            feedback: "found a real gap: no Android/JNI path exists".into(),
            proposals,
            succeeded: true,
        });
        if let Some(p) = state.history[0].proposals.first() {
            state.added_stages.push(p.stage_id.clone());
        }
        state
    }

    #[test]
    fn renders_none_for_a_run_with_no_history() {
        let state = RunState::new("empty-run");
        assert_eq!(render_plan_markdown(&state), None);
    }

    #[test]
    fn renders_the_latest_iterations_feedback_and_proposal_details() {
        let proposal = StageProposal {
            proposed_by: "devsystem.implement".into(),
            stage_id: "devsystem.android_native_bridge".into(),
            tag: "android_native_bridge".into(),
            rationale: "reuse the audited Rust Noise_IK code instead of reimplementing it".into(),
            use_existing_service: None,
            units: 1,
        };
        let state = state_with_one_iteration(vec![proposal]);
        let md = render_plan_markdown(&state).expect("history is non-empty");

        assert!(md.contains("run-checkin"));
        assert!(md.contains("iteration 1"));
        assert!(md.contains("devsystem.implement"));
        assert!(md.contains("no Android/JNI path exists"));
        assert!(md.contains("devsystem.android_native_bridge"));
        assert!(md.contains("reuse the audited Rust Noise_IK code"));
        assert!(md.contains("none -- a new service must be built or provided"));
        assert!(md.contains("Decision needed"));
    }

    #[test]
    fn renders_cleanly_when_an_iteration_carries_no_proposals() {
        let state = state_with_one_iteration(vec![]);
        let md = render_plan_markdown(&state).unwrap();
        assert!(md.contains("None this iteration."));
    }
}
