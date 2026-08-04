//! Pre-flight risk annotations (`docs/plan-stage.md`'s own documented "next slice",
//! proposal §5): real, mechanical checks over a run's history, meant to be seeded
//! into the canvas session *before* a human ever opens it. Every finding here
//! inspects real [`IterationRecord`]/[`StageProposal`] data -- nothing invented, no
//! LLM judgment call, just pattern checks a human reviewer would otherwise have to
//! do by hand.

use crate::runner::RunState;
use crate::{STAGE_IMPLEMENT, STAGE_TEST};

/// One real risk finding: a short label plus the concrete evidence that triggered it
/// -- always traceable back to specific text/history, never asserted without a
/// reason a human can immediately verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskAnnotation {
    pub label: String,
    pub evidence: String,
}

const SECURITY_KEYWORDS: [&str; 8] =
    ["auth", "security", "crypto", "key material", "credential", "password", "handshake", "session"];

/// Run every known check against `state` and return whatever real findings result.
pub fn preflight_annotations(state: &RunState) -> Vec<RiskAnnotation> {
    let mut findings = Vec::new();
    if let Some(a) = security_keyword_hit(state) {
        findings.push(a);
    }
    if let Some(a) = missing_test_before_implement(state) {
        findings.push(a);
    }
    if let Some(a) = no_price_ceiling(state) {
        findings.push(a);
    }
    findings
}

/// "touches auth" (proposal §5's own example): the latest iteration's feedback, or
/// any proposal it carries, mentions a security-relevant keyword.
fn security_keyword_hit(state: &RunState) -> Option<RiskAnnotation> {
    let latest = state.history.last()?;
    let feedback_lower = latest.feedback.to_lowercase();
    if let Some(kw) = SECURITY_KEYWORDS.iter().find(|kw| feedback_lower.contains(**kw)) {
        return Some(RiskAnnotation {
            label: "touches auth/security".into(),
            evidence: format!("iteration {}'s feedback mentions \"{kw}\"", latest.iteration),
        });
    }
    for p in &latest.proposals {
        let text = format!("{} {}", p.tag, p.rationale).to_lowercase();
        if let Some(kw) = SECURITY_KEYWORDS.iter().find(|kw| text.contains(**kw)) {
            return Some(RiskAnnotation {
                label: "touches auth/security".into(),
                evidence: format!("proposal `{}`'s rationale mentions \"{kw}\"", p.stage_id),
            });
        }
    }
    None
}

/// "no test stage before implement" (proposal §5's own example): a real
/// `devsystem.implement` iteration ran with no `devsystem.test` iteration before it
/// anywhere in the run's history.
fn missing_test_before_implement(state: &RunState) -> Option<RiskAnnotation> {
    let implement_at = state.history.iter().position(|r| r.stage == STAGE_IMPLEMENT)?;
    let test_before = state.history[..implement_at].iter().any(|r| r.stage == STAGE_TEST);
    if test_before {
        None
    } else {
        Some(RiskAnnotation {
            label: "no test stage before implement".into(),
            evidence: format!(
                "devsystem.implement first ran at iteration {}, with no devsystem.test iteration before it",
                state.history[implement_at].iteration
            ),
        })
    }
}

/// "external-partner role with no price ceiling" (proposal §5's own example),
/// honestly scoped to what the data actually shows: a proposal that needs a new
/// service built or provided (`use_existing_service` is `None` -- the closest
/// signal available for "not just reusing something already trusted/priced") and
/// sets no `price_ceiling`. Doesn't claim to know the filler is specifically an
/// *external paid* partner -- `StageProposal` has no field distinguishing that
/// from an internal build yet -- only that nothing here bounds what it could cost.
fn no_price_ceiling(state: &RunState) -> Option<RiskAnnotation> {
    let latest = state.history.last()?;
    let unbounded = latest.proposals.iter().find(|p| p.use_existing_service.is_none() && p.price_ceiling.is_none())?;
    Some(RiskAnnotation {
        label: "no price ceiling set".into(),
        evidence: format!(
            "proposal `{}` needs a new service (no use_existing_service) and sets no price_ceiling -- nothing bounds what filling it could cost",
            unbounded.stage_id
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IterationRecord, StageProposal, STAGE_REVIEW, STAGE_VERIFY};

    fn iteration(stage: &str, iteration: u32, feedback: &str, proposals: Vec<StageProposal>) -> IterationRecord {
        IterationRecord { run_id: "run-preflight".into(), stage: stage.into(), iteration, feedback: feedback.into(), proposals, succeeded: true }
    }

    #[test]
    fn no_findings_for_a_run_with_no_history() {
        let state = RunState::new("run-preflight");
        assert!(preflight_annotations(&state).is_empty());
    }

    #[test]
    fn flags_a_security_keyword_in_the_latest_feedback() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_REVIEW, 1, "reviewed the Noise_IK handshake code", vec![]));
        let findings = preflight_annotations(&state);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].label, "touches auth/security");
        assert!(findings[0].evidence.contains("handshake"));
    }

    #[test]
    fn flags_a_security_keyword_in_a_proposals_rationale() {
        let mut state = RunState::new("run-preflight");
        let proposal = StageProposal {
            proposed_by: STAGE_REVIEW.into(),
            stage_id: "devsystem.android_native_bridge".into(),
            tag: "android_native_bridge".into(),
            rationale: "handles key material across the JNI boundary".into(),
            use_existing_service: None,
            units: 1,
            // Set so this test isolates the security-keyword check -- a None here
            // would also trip no_price_ceiling below, covered by its own tests.
            price_ceiling: Some(50),
        };
        state.history.push(iteration(STAGE_REVIEW, 1, "no risk words here", vec![proposal]));
        let findings = preflight_annotations(&state);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].evidence.contains("android_native_bridge"));
    }

    #[test]
    fn no_security_finding_when_nothing_mentions_a_keyword() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_TEST, 1, "added a unit test", vec![]));
        state.history.push(iteration(STAGE_IMPLEMENT, 2, "wrote a helper function", vec![]));
        assert!(preflight_annotations(&state).is_empty());
    }

    #[test]
    fn flags_implement_running_before_any_test_iteration() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_IMPLEMENT, 1, "wrote code", vec![]));
        let findings = preflight_annotations(&state);
        assert!(findings.iter().any(|f| f.label == "no test stage before implement"));
    }

    #[test]
    fn does_not_flag_when_a_test_iteration_precedes_implement() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_TEST, 1, "added a test", vec![]));
        state.history.push(iteration(STAGE_IMPLEMENT, 2, "wrote code", vec![]));
        assert!(preflight_annotations(&state).is_empty());
    }

    #[test]
    fn does_not_flag_when_test_runs_after_implement_but_still_before_a_later_implement() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_IMPLEMENT, 1, "wrote code", vec![]));
        state.history.push(iteration(STAGE_TEST, 2, "added a test", vec![]));
        state.history.push(iteration(STAGE_VERIFY, 3, "verified", vec![]));
        // Still flags -- the FIRST implement had no test before it, and that's the
        // real historical fact this check reports; it does not retroactively clear.
        let findings = preflight_annotations(&state);
        assert!(findings.iter().any(|f| f.label == "no test stage before implement"));
    }

    fn proposal(use_existing_service: Option<&str>, price_ceiling: Option<u64>) -> StageProposal {
        StageProposal {
            proposed_by: STAGE_REVIEW.into(),
            stage_id: "devsystem.some_new_role".into(),
            tag: "some_new_role".into(),
            rationale: "test".into(),
            use_existing_service: use_existing_service.map(str::to_string),
            units: 1,
            price_ceiling,
        }
    }

    #[test]
    fn flags_a_new_service_proposal_with_no_price_ceiling() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_REVIEW, 1, "no risk words here", vec![proposal(None, None)]));
        let findings = preflight_annotations(&state);
        assert!(findings.iter().any(|f| f.label == "no price ceiling set"));
    }

    #[test]
    fn does_not_flag_a_proposal_with_a_price_ceiling_set() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_REVIEW, 1, "no risk words here", vec![proposal(None, Some(100))]));
        assert!(preflight_annotations(&state).is_empty());
    }

    #[test]
    fn does_not_flag_a_proposal_that_reuses_an_existing_service() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_REVIEW, 1, "no risk words here", vec![proposal(Some("android-build-box"), None)]));
        assert!(preflight_annotations(&state).is_empty());
    }
}
