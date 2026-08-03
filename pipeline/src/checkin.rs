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

    // A periodic check-in is about the whole run so far, not just the iteration that
    // happened to trigger it -- a human pausing here needs "how's this run going",
    // not just the latest slice. Every prior iteration gets one summary line;
    // the triggering iteration gets full detail below.
    if state.history.len() > 1 {
        md.push_str("## Run summary\n\n");
        md.push_str(&format!(
            "{} iteration(s) so far, {} role(s) currently in the live spec.\n\n",
            state.history.len(),
            state.added_stages.len() + 1, // +1 for the always-present plan role
        ));
        for r in &state.history {
            let status = if r.succeeded { "ok" } else { "FAILED" };
            let first_line = r.feedback.lines().next().unwrap_or("");
            let truncated = if first_line.chars().count() > 140 {
                let head: String = first_line.chars().take(140).collect();
                format!("{head}...")
            } else {
                first_line.to_string()
            };
            md.push_str(&format!("- iteration {} (`{}`, {status}): {truncated}\n", r.iteration, r.stage));
        }
        md.push('\n');
    }

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

    #[test]
    fn a_single_iteration_run_has_no_run_summary_section() {
        // Nothing to summarize yet when this is the only iteration -- avoid a
        // redundant "1 iteration so far" section duplicating the detail below it.
        let state = state_with_one_iteration(vec![]);
        let md = render_plan_markdown(&state).unwrap();
        assert!(!md.contains("## Run summary"));
    }

    #[test]
    fn a_multi_iteration_run_summarizes_every_prior_iteration_not_just_the_latest() {
        let mut state = RunState::new("run-multi");
        state.history.push(IterationRecord {
            run_id: "run-multi".into(),
            stage: "devsystem.implement".into(),
            iteration: 1,
            feedback: "found the JNI gap".into(),
            proposals: vec![],
            succeeded: true,
        });
        state.history.push(IterationRecord {
            run_id: "run-multi".into(),
            stage: "devsystem.test".into(),
            iteration: 2,
            feedback: "added a Robolectric test".into(),
            proposals: vec![],
            succeeded: true,
        });
        state.added_stages.push("devsystem.test".into());

        let md = render_plan_markdown(&state).unwrap();
        assert!(md.contains("## Run summary"));
        assert!(md.contains("2 iteration(s) so far"));
        assert!(md.contains("found the JNI gap"), "the non-latest iteration must still be summarized");
        assert!(md.contains("added a Robolectric test"));
    }
}
