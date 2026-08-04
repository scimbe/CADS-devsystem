//! Renders a run's pending check-in as a `.plan.md` artifact for `ecc-plan-canvas`
//! (`docs/plan-stage.md`) -- the second real delivery channel for a checkpoint, not
//! just a GitHub issue comment. Pure rendering only; the actual `ecc-plan-canvas open`
//! invocation lives in the `devsystem_checkin` binary, since spawning a process isn't
//! something a hermetic `cargo test` should do.

use crate::improve::stalled_stages;
use crate::preflight::preflight_annotations;
use crate::runner::RunState;
use crate::IterationRecord;

/// Derive a canvas session's `(key, origin)` from the `url` field of
/// `ecc-plan-canvas open`'s JSON output (e.g.
/// `"http://127.0.0.1:4517/canvas/3f51bca85e2f"` -> `("3f51bca85e2f",
/// "http://127.0.0.1:4517")`). `open`'s JSON carries no separate session-key field
/// -- pulled out here, tested directly, after a real bug shipped in this exact
/// logic once already (an earlier version assumed a "key" field existed; it
/// doesn't -- caught during live verification, not by a test, which is the whole
/// reason this is a real function now instead of staying inline in `main()`).
pub fn parse_session_key_and_origin(url: &str) -> Option<(String, String)> {
    let segments: Vec<&str> = url.split('/').collect();
    // A bare origin ("http://host:port") splits into exactly 3 segments
    // ("http:", "", "host:port") -- anything at or below that has no path
    // component to treat as a key, unlike a naive rsplit('/').next() would assume.
    if segments.len() <= 3 {
        return None;
    }
    let key = segments.last().filter(|s| !s.is_empty())?;
    Some((key.to_string(), segments[..3].join("/")))
}

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

    // Real gap found+fixed 2026-08-05: requirement_indices lives ON this exact
    // IterationRecord (same as `proposals`, already shown below), but had no
    // rendering anywhere in the mandatory check-in artifact -- a human
    // approving/requesting-changes on this iteration had no way to see which
    // requirements it actually claims to address, unlike the GUI's History/
    // Requirements panels which already show it (#47 traceability slice).
    if !record.requirement_indices.is_empty() {
        md.push_str("## Requirements addressed this iteration\n\n");
        for &i in &record.requirement_indices {
            match state.requirements.get(i) {
                Some(r) => md.push_str(&format!("- {}\n", r.statement)),
                None => md.push_str(&format!("- requirement #{i} (no longer exists)\n")),
            }
        }
        md.push('\n');
    }

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

    let stalled = stalled_stages(state);
    if !stalled.is_empty() {
        md.push_str("## Stalled stages (devsystem.improve)\n\n");
        md.push_str("Proposed and live in the spec, but no iteration has run *as* these \
            stages yet -- likely blocked on a pending human decision:\n\n");
        for s in &stalled {
            md.push_str(&format!("- `{s}`\n"));
        }
        md.push('\n');
    }

    let risks = preflight_annotations(state);
    if !risks.is_empty() {
        md.push_str("## Risk annotations\n\n");
        md.push_str("Mechanical checks over this run's history -- not an LLM judgment call, \
            just patterns a human reviewer would otherwise have to spot by hand:\n\n");
        for r in &risks {
            md.push_str(&format!("- **{}**: {}\n", r.label, r.evidence));
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
    use crate::runner::Requirement;
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
            requirement_indices: Vec::new(),
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
            price_ceiling: None,
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
    fn renders_which_real_requirements_this_iteration_claims_to_address() {
        let mut state = RunState::new("run-req-checkin");
        state.requirements.push(Requirement {
            statement: "WHEN a user sends a text message over an established channel, THE SYSTEM SHALL persist it locally before confirming delivery to the UI".into(),
            acceptance_criteria: vec!["message survives an app restart".into()],
            verified: false,
        });
        state.history.push(IterationRecord {
            run_id: "run-req-checkin".into(),
            stage: "devsystem.implement".into(),
            iteration: 1,
            feedback: "wired local persistence before the UI confirms delivery".into(),
            proposals: vec![],
            succeeded: true,
            requirement_indices: vec![0],
        });

        let md = render_plan_markdown(&state).expect("history is non-empty");
        assert!(md.contains("## Requirements addressed this iteration"));
        assert!(md.contains("WHEN a user sends a text message over an established channel"));
    }

    #[test]
    fn omits_the_requirements_section_when_this_iteration_claims_none() {
        let state = state_with_one_iteration(vec![]);
        let md = render_plan_markdown(&state).unwrap();
        assert!(!md.contains("## Requirements addressed this iteration"));
    }

    #[test]
    fn a_proposed_stage_with_no_iteration_of_its_own_is_reported_as_stalled() {
        let proposal = StageProposal {
            proposed_by: "devsystem.implement".into(),
            stage_id: "devsystem.android_native_bridge".into(),
            tag: "android_native_bridge".into(),
            rationale: "blocked on a real architecture decision".into(),
            use_existing_service: None,
            units: 1,
            price_ceiling: None,
        };
        let state = state_with_one_iteration(vec![proposal]);
        let md = render_plan_markdown(&state).unwrap();
        assert!(md.contains("## Stalled stages (devsystem.improve)"));
        assert!(md.contains("`devsystem.android_native_bridge`"));
    }

    #[test]
    fn surfaces_a_real_preflight_finding_in_the_rendered_markdown() {
        // preflight::preflight_annotations is real, tested governance logic that
        // used to be computed but never rendered anywhere a human would see it --
        // this proves the check-in markdown (the actual human-review gate) now
        // carries it, not just the underlying pure function in isolation.
        let mut state = RunState::new("run-security");
        state.history.push(IterationRecord {
            run_id: "run-security".into(),
            stage: "devsystem.implement".into(),
            iteration: 1,
            feedback: "wired the real Noise_IK handshake and session key material".into(),
            proposals: vec![],
            succeeded: true,
            requirement_indices: Vec::new(),
        });
        let md = render_plan_markdown(&state).unwrap();
        assert!(md.contains("## Risk annotations"));
        assert!(md.contains("touches auth/security"));
    }

    #[test]
    fn omits_the_risk_annotations_section_when_nothing_is_flagged() {
        let mut state = RunState::new("run-clean");
        state.history.push(IterationRecord {
            run_id: "run-clean".into(),
            stage: "devsystem.test".into(),
            iteration: 1,
            feedback: "added a Robolectric test, no proposals".into(),
            proposals: vec![],
            succeeded: true,
            requirement_indices: Vec::new(),
        });
        let md = render_plan_markdown(&state).unwrap();
        assert!(!md.contains("## Risk annotations"));
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
            requirement_indices: Vec::new(),
        });
        state.history.push(IterationRecord {
            run_id: "run-multi".into(),
            stage: "devsystem.test".into(),
            iteration: 2,
            feedback: "added a Robolectric test".into(),
            proposals: vec![],
            succeeded: true,
            requirement_indices: Vec::new(),
        });
        state.added_stages.push("devsystem.test".into());

        let md = render_plan_markdown(&state).unwrap();
        assert!(md.contains("## Run summary"));
        assert!(md.contains("2 iteration(s) so far"));
        assert!(md.contains("found the JNI gap"), "the non-latest iteration must still be summarized");
        assert!(md.contains("added a Robolectric test"));
    }

    #[test]
    fn parses_the_real_url_shape_ecc_plan_canvas_open_actually_returns() {
        let (key, origin) = parse_session_key_and_origin("http://127.0.0.1:4517/canvas/3f51bca85e2f").unwrap();
        assert_eq!(key, "3f51bca85e2f");
        assert_eq!(origin, "http://127.0.0.1:4517");
    }

    #[test]
    fn parses_a_url_with_a_different_host_and_port() {
        let (key, origin) = parse_session_key_and_origin("http://localhost:9000/canvas/abc123").unwrap();
        assert_eq!(key, "abc123");
        assert_eq!(origin, "http://localhost:9000");
    }

    #[test]
    fn returns_none_for_a_trailing_slash_with_no_key_segment() {
        assert_eq!(parse_session_key_and_origin("http://127.0.0.1:4517/canvas/"), None);
    }

    #[test]
    fn returns_none_for_a_url_with_no_path_at_all() {
        assert_eq!(parse_session_key_and_origin("http://127.0.0.1:4517"), None);
    }

    #[test]
    fn returns_none_for_an_empty_string() {
        assert_eq!(parse_session_key_and_origin(""), None);
    }
}
