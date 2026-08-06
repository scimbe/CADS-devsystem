//! Pre-flight risk annotations (`docs/plan-stage.md`'s own documented "next slice",
//! proposal §5): real, mechanical checks over a run's history, meant to be seeded
//! into the canvas session *before* a human ever opens it. Every finding here
//! inspects real [`IterationRecord`]/[`StageProposal`] data -- nothing invented, no
//! LLM judgment call, just pattern checks a human reviewer would otherwise have to
//! do by hand.

use crate::runner::{distinct_word_count, RunState};
use crate::{STAGE_IMPLEMENT, STAGE_TEST};
use ct_common::pipeline::PipelineSpec;

/// Same real, mechanical substance bars `runner.rs`'s review gate uses
/// (`MIN_REVIEW_FEEDBACK_LEN`/`MIN_REVIEW_DISTINCT_WORDS`), named separately
/// here rather than shared -- these are two conceptually distinct gates (one
/// about a review's real scrutiny, this one about a test stage's real
/// substance) that could reasonably diverge later, even though they start at
/// the same values.
const MIN_TEST_FEEDBACK_LEN: usize = 25;
const MIN_TEST_DISTINCT_WORDS: usize = 8;

/// One real risk finding: a short label plus the concrete evidence that triggered it
/// -- always traceable back to specific text/history, never asserted without a
/// reason a human can immediately verify.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RiskAnnotation {
    pub label: String,
    pub evidence: String,
}

const SECURITY_KEYWORDS: [&str; 8] =
    ["auth", "security", "crypto", "key material", "credential", "password", "handshake", "session"];

/// Real gap named directly in the goal doc's own §5 quality-bar table
/// ("Vertragsgemäße / Sachmangelfreie Leistung ... nothing blocks marking work
/// 'done' with open, known defects"), live-verified before this check existed:
/// `{"stage":"devsystem.implement","feedback":"Shipped the feature. Known
/// issue: crashes on a null id, not fixed yet, workaround needed.",
/// "succeeded":true}` produced zero risk findings -- nothing caught an
/// iteration contradicting its own `succeeded:true` in its own feedback text.
/// Multi-word phrases specifically, not single common words like "broken"
/// alone (which would false-positive on "fixes the previously broken X") --
/// same crude-but-explainable proxy discipline as `SECURITY_KEYWORDS`, still
/// beatable by phrasing a real defect without these exact words (an honest,
/// named limitation, not claimed comprehensive).
const DEFECT_ADMISSION_PHRASES: [&str; 6] =
    ["known issue", "known bug", "not fixed", "not implemented", "workaround needed", "still broken"];

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
    if let Some(a) = succeeded_iteration_admits_a_defect(state) {
        findings.push(a);
    }
    if let Some(a) = checkin_cadence_effectively_disabled(state) {
        findings.push(a);
    }
    if let Some(a) = vague_acceptance_criteria(state) {
        findings.push(a);
    }
    findings
}

/// A criterion at or above this many distinct words is treated as specific
/// enough to leave a real, checkable constraint -- deliberately much lower
/// than `MIN_REVIEW_DISTINCT_WORDS` (a whole review's worth of prose, not one
/// short EARS-style clause). Crude, honestly-scoped proxy, same convention as
/// every other mechanical check here: a genuinely specific-but-terse criterion
/// ("file exists") can still false-positive, and a genuinely vague-but-wordy
/// one ("the feature behaves as expected across all reasonable cases") can
/// still slip through -- not claimed comprehensive, same as
/// `DEFECT_ADMISSION_PHRASES`/`SECURITY_KEYWORDS` above.
const MIN_ACCEPTANCE_CRITERION_DISTINCT_WORDS: usize = 3;

/// Real gap named directly in the goal doc's own §4.3 -- the *second* explicit
/// worked example of what a real `devsystem.process_improve` check should
/// catch: "this role's acceptance criteria are too vague to be deterministic"
/// (§1's own commitment: "acceptance criteria specific enough to leave no real
/// decision to the LLM"). `add_requirement`'s own `MIN_ACCEPTANCE_CRITERION_ALNUM_CHARS`
/// gate (`web/src/main.rs`) already rejects the worst cases at add-time
/// ("ok", ".", an invisible character) -- but a criterion like "works" or "is
/// fast" clears that 5-alphanumeric-character bar while still leaving a real
/// decision to the LLM, exactly what §1 says this project is trying to avoid.
/// This is the complementary, existing-requirement-scanning half: not a hard
/// block (some real criteria are legitimately short), an advisory risk a
/// human reviewing this run's own Risks panel would want to see.
fn vague_acceptance_criteria(state: &RunState) -> Option<RiskAnnotation> {
    for (i, r) in state.requirements.iter().enumerate() {
        for (ci, c) in r.acceptance_criteria.iter().enumerate() {
            if distinct_word_count(c) < MIN_ACCEPTANCE_CRITERION_DISTINCT_WORDS {
                return Some(RiskAnnotation {
                    label: "acceptance criteria too vague to be deterministic".into(),
                    evidence: format!(
                        "requirement #{i}'s acceptance criterion #{ci} (\"{c}\") has fewer than \
                         {MIN_ACCEPTANCE_CRITERION_DISTINCT_WORDS} distinct words -- goal doc §1's own \
                         commitment (\"acceptance criteria specific enough to leave no real decision to \
                         the LLM\") needs more than a short label to actually constrain what a \
                         role-filler builds"
                    ),
                });
            }
        }
    }
    None
}

/// Real gap named directly in the goal doc's own §4.3 -- an explicit worked
/// example of what a real `devsystem.process_improve` check should catch:
/// "this run's check-ins are too sparse". Live-verified before this check
/// existed: `checkin_every: 0` (accepted by `update_criteria` with zero
/// validation -- unlike `max_iterations`/`max_consecutive_failures`, which
/// already reject `0`) produced zero risk findings, even though
/// `should_checkin`'s own real fallback for it means the mandatory cadence
/// (`AbortCriteria::checkin_every`'s whole documented purpose: "fires at
/// least this often, even when every iteration is succeeding") is
/// effectively disabled -- only the hard `max_iterations` ceiling still
/// forces a check-in. `checkin_every >= max_iterations` is the same real
/// problem in a less obvious shape: the cadence can never fire on its own
/// before the ceiling does either, so it's functionally disabled too, not
/// just large.
fn checkin_cadence_effectively_disabled(state: &RunState) -> Option<RiskAnnotation> {
    let c = state.criteria;
    if c.checkin_every == 0 {
        return Some(RiskAnnotation {
            label: "mandatory check-in cadence effectively disabled".into(),
            evidence: format!(
                "checkin_every is 0 -- the mandatory \"check in at least this often\" cadence never \
                 fires on its own; only the hard max_iterations ceiling ({}) still forces a check-in",
                c.max_iterations
            ),
        });
    }
    if c.checkin_every >= c.max_iterations {
        return Some(RiskAnnotation {
            label: "mandatory check-in cadence effectively disabled".into(),
            evidence: format!(
                "checkin_every ({}) is at or past max_iterations ({}) -- the cadence can never fire \
                 on its own before the hard ceiling does either, so it never provides an actual \
                 mid-run check-in",
                c.checkin_every, c.max_iterations
            ),
        });
    }
    None
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

/// "succeeded iteration admits a known defect" -- some `succeeded: true`
/// iteration's own feedback contains a real defect-admission phrase. See
/// [`DEFECT_ADMISSION_PHRASES`]'s own doc comment for the live-verified gap
/// this closes and its honest limitation.
///
/// **Real gap found live by the stress test, 2026-08-06, same bug shape as
/// `no_price_ceiling`'s own fix**: this used to only look at the LATEST
/// iteration. Live-verified: a real, unfixed, admitted defect got correctly
/// flagged, then silently vanished the moment one completely unrelated
/// iteration followed it -- the defect was never actually fixed, only
/// unmentioned. Unlike `no_price_ceiling`, there's no structural "was this
/// resolved" signal to check (a `price_ceiling` getting set is a real,
/// checkable field change; a defect getting fixed is free text with no
/// equivalent marker) -- so the honest fix, matching
/// `no_review_role_despite_real_progress`'s own established pattern, is to
/// scan all of history and keep flagging as long as ANY successful iteration
/// ever admitted a defect. A false "still open" nag on an actually-fixed
/// defect is a real, named cost of this -- but it's a far smaller one than
/// silently hiding a defect nobody ever said was fixed.
fn succeeded_iteration_admits_a_defect(state: &RunState) -> Option<RiskAnnotation> {
    let hit = state.history.iter().rev().find(|h| {
        h.succeeded && {
            let feedback_lower = h.feedback.to_lowercase();
            DEFECT_ADMISSION_PHRASES.iter().any(|p| feedback_lower.contains(*p))
        }
    })?;
    let feedback_lower = hit.feedback.to_lowercase();
    let phrase = DEFECT_ADMISSION_PHRASES.iter().find(|p| feedback_lower.contains(**p))?;
    Some(RiskAnnotation {
        label: "succeeded iteration admits a known defect".into(),
        evidence: format!(
            "iteration {}'s own feedback contains \"{phrase}\" while marked succeeded:true -- goal \
             doc §5's Vertragsgemäße/Sachmangelfreie row names this exact gap: nothing else blocks \
             marking work \"done\" with open, known defects. No later iteration signals it was \
             ever fixed, so this stays flagged.",
            hit.iteration
        ),
    })
}

/// "no test stage before implement" (proposal §5's own example): a real
/// `devsystem.implement` iteration ran with no `devsystem.test` iteration before it
/// anywhere in the run's history.
///
/// **Real gap found live by the stress test, same day as the review-gate rubber-
/// stamp findings, and closed the same way**: this check originally only asked
/// *whether* a `devsystem.test` record existed before `implement` -- a
/// substance-free `feedback: "tests pass"` counted exactly the same as real
/// testing, silently neutering the risk annotation that's supposed to flag
/// this. Live-verified before this fix: a rubber-stamp `devsystem.test`
/// iteration made this risk annotation vanish, then a real `devsystem.implement`
/// with feedback honestly admitting "no actual tests written for it" produced
/// zero risk findings. Now requires the SAME two real, mechanical substance
/// bars the review gate uses (length + distinct words) -- a test iteration that
/// doesn't clear them doesn't count as real evidence testing happened.
fn missing_test_before_implement(state: &RunState) -> Option<RiskAnnotation> {
    let implement_at = state.history.iter().position(|r| r.stage == STAGE_IMPLEMENT)?;
    let real_test_before = state.history[..implement_at].iter().any(|r| {
        r.stage == STAGE_TEST && {
            let trimmed = r.feedback.trim();
            trimmed.chars().count() >= MIN_TEST_FEEDBACK_LEN && distinct_word_count(trimmed) >= MIN_TEST_DISTINCT_WORDS
        }
    });
    if real_test_before {
        None
    } else {
        Some(RiskAnnotation {
            label: "no test stage before implement".into(),
            evidence: format!(
                "devsystem.implement first ran at iteration {}, with no devsystem.test iteration \
                 before it that's substantive enough to count as real evidence testing happened \
                 ({MIN_TEST_FEEDBACK_LEN}+ characters and {MIN_TEST_DISTINCT_WORDS}+ distinct words \
                 of feedback, not a rubber-stamp)",
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
///
/// **Real gap found live by the stress test, 2026-08-06**: this used to only look
/// at the LATEST iteration's own proposals, the same real bug class already fixed
/// elsewhere in this file for other checks -- a genuinely unbounded-cost role,
/// still live in the run's own spec, silently stopped being flagged the moment
/// any unrelated iteration followed it, even though nothing about the actual risk
/// had changed. Live-verified: proposed `devsystem.gpu_training` with no
/// `price_ceiling`, got flagged; one unrelated iteration later, the exact same
/// still-live, still-unbounded role produced zero risk findings. Fixed by scanning
/// all of history for an unbounded proposal whose `stage_id` is still present in
/// `state.added_stages` -- the real, checkable "is this specific risk still live"
/// signal, not "did the most recent iteration happen to mention it". A role that's
/// no longer in `added_stages` (rejected, or never applied) is correctly not
/// flagged either way.
///
/// **Real gap found live by the stress test, twenty-fifth run, 2026-08-06**:
/// `price_ceiling` is never actually enforced anywhere against a real bid's
/// price (confirmed by reading every real call site -- it's stored and shown,
/// never compared against anything) -- this risk exists precisely because
/// nothing else bounds real cost exposure. That makes `price_ceiling: Some(0)`
/// exactly as meaningless as `None`, not safer: a real `0` conveys no real
/// protection (there's nothing to actually enforce it), yet the check used to
/// only match `is_none()`, so a proposal setting `price_ceiling: 0` silently
/// produced zero risk findings -- live-confirmed: proposed and approved
/// `devsystem.new_service` with `price_ceiling: 0`, got `risks: []`, giving a
/// human reviewing this run a false "this is bounded" signal for a role that's
/// exactly as unbounded as one with no ceiling at all. `unwrap_or(0) == 0`
/// below treats both the same, honestly.
fn no_price_ceiling(state: &RunState) -> Option<RiskAnnotation> {
    // Real regression found live, same day as approved_stage_proposals was
    // introduced: scanning *only* the new field silently dropped every real,
    // still-unbounded role approved before that field existed -- confirmed
    // against the actual deployed webconference-android run, whose real
    // devsystem.document_extraction risk (price_ceiling never set, still
    // live in the spec) vanished the moment this deployed. `approved_stage_proposals`
    // is complete *going forward* (both real approval paths write to it now),
    // but `history.proposals` still holds the only record of everything
    // approved before it existed -- a real proposal is unbounded either way,
    // so this scans the union of both, not a replacement of one by the other.
    //
    // **Real gap found live, twenty-seventh run, 2026-08-06**: a human trying
    // to *fix* an already-live unbounded role by re-proposing the exact same
    // `stage_id` with a real `price_ceiling` got a genuine `200`
    // (`apply_proposal` correctly reports `AlreadyPresent` -- the role's own
    // service/tag really is unchanged) but the "fix" was silently ignored:
    // this used to take the *first* matching entry, which is always the
    // original, bad proposal. Now takes the *last* matching entry per real
    // `stage_id` -- `approved_stage_proposals` first (complete and real going
    // forward, including every re-proposal attempt now), falling back to
    // `history.proposals` only for a `stage_id` with no entry there at all
    // (pre-existing data from before that field existed) -- so a later, real,
    // better proposal actually supersedes an earlier bad one.
    let unbounded = state
        .added_stages
        .iter()
        .filter_map(|stage_id| {
            state
                .approved_stage_proposals
                .iter()
                .rev()
                .find(|p| &p.stage_id == stage_id)
                .or_else(|| state.history.iter().rev().flat_map(|h| h.proposals.iter().rev()).find(|p| &p.stage_id == stage_id))
        })
        .find(|p| p.use_existing_service.is_none() && p.price_ceiling.unwrap_or(0) == 0)?;
    Some(RiskAnnotation {
        label: "no price ceiling set".into(),
        evidence: format!(
            "role `{}` is live in this run's own spec, was proposed needing a new service (no use_existing_service) with no real price_ceiling ({}), and nothing since has bounded what filling it could cost -- price_ceiling is never actually enforced against a real bid's price, so 0 is exactly as unbounded as unset",
            unbounded.stage_id,
            unbounded.price_ceiling.map(|p| p.to_string()).unwrap_or_else(|| "none set".to_string())
        ),
    })
}

const MIN_ITERATIONS_BEFORE_FLAGGING_NO_REVIEW: usize = 3;

/// Real, mechanical **process**-level checks (#382 goal doc §4.3/§9: "self-
/// optimizing the process itself, not just the stage list"). Unlike
/// [`preflight_annotations`], these need the run's own live [`PipelineSpec`] too
/// -- they're about which roles are declared, not just what already happened --
/// so this is a genuinely separate function rather than folded into
/// `preflight_annotations` itself (which every existing caller, including
/// `checkin.rs`'s history-only rendering and `devsystem_checkin`'s binary, only
/// ever has a bare [`RunState`] for).
pub fn process_annotations(spec: &PipelineSpec, state: &RunState) -> Vec<RiskAnnotation> {
    let mut findings = Vec::new();
    if let Some(a) = no_review_role_despite_real_progress(spec, state) {
        findings.push(a);
    }
    findings
}

/// "no review role declared despite real progress": a run with real successful
/// iterations but no `devsystem.review` role in its own spec has no teeth on
/// gap #2's own mandatory review gate at all -- `toggle_requirement`'s gate is
/// scoped to only bite once `review` is declared, by design, so a run that never
/// declares it is silently exempt from the whole mechanism. Counts real
/// *successful* iterations, not just any history entry, and deliberately doesn't
/// match on a specific stage name (`devsystem.implement`) -- this pipeline's own
/// stages are custom-named per project (`devsystem.android_native_bridge`, not
/// `devsystem.implement`, on `webconference-android` itself), so "real progress
/// happened" is the honest, general signal available, not "the implement stage
/// specifically ran".
fn no_review_role_despite_real_progress(spec: &PipelineSpec, state: &RunState) -> Option<RiskAnnotation> {
    if spec.roles.iter().any(|r| r.tag == "review") {
        return None;
    }
    let successful = state.history.iter().filter(|h| h.succeeded).count();
    if successful < MIN_ITERATIONS_BEFORE_FLAGGING_NO_REVIEW {
        return None;
    }
    Some(RiskAnnotation {
        label: "no review role declared despite real progress".into(),
        evidence: format!(
            "{successful} successful iteration(s) so far, but this run has never declared a devsystem.review \
             role -- gap #2's mandatory review gate (requirements can't be marked verified without a real \
             review) has no teeth here at all, since it only applies once review is declared."
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AbortCriteria, IterationRecord, StageProposal, STAGE_REVIEW, STAGE_VERIFY};

    fn iteration(stage: &str, iteration: u32, feedback: &str, proposals: Vec<StageProposal>) -> IterationRecord {
        IterationRecord { run_id: "run-preflight".into(), stage: stage.into(), iteration, feedback: feedback.into(), proposals, succeeded: true, requirement_indices: Vec::new() }
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
        state.history.push(iteration(STAGE_TEST, 1, "added a real unit test covering the empty-input edge case and confirmed it fails without the fix", vec![]));
        state.history.push(iteration(STAGE_IMPLEMENT, 2, "wrote a helper function", vec![]));
        assert!(preflight_annotations(&state).is_empty());
    }

    #[test]
    /// Real gap named directly in the goal doc's own §5 quality-bar table,
    /// live-verified against the actual deployment before this check
    /// existed: this exact feedback produced zero risk findings.
    fn flags_a_succeeded_iteration_that_admits_a_known_defect() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(
            STAGE_IMPLEMENT,
            1,
            "Shipped the retry-on-failure feature. Known issue: it crashes on a null message id, not fixed yet, workaround needed before real use.",
            vec![],
        ));
        let findings = preflight_annotations(&state);
        assert!(findings.iter().any(|f| f.label == "succeeded iteration admits a known defect"), "got: {findings:?}");
    }

    #[test]
    fn does_not_flag_a_failed_iteration_that_mentions_a_known_defect() {
        let mut state = RunState::new("run-preflight");
        state.history.push(IterationRecord {
            run_id: "run-preflight".into(),
            stage: STAGE_IMPLEMENT.into(),
            iteration: 1,
            feedback: "Known issue: it crashes on a null message id, not fixed yet.".into(),
            succeeded: false,
            proposals: vec![],
            requirement_indices: vec![],
        });
        // A FAILED iteration honestly saying it's broken is exactly the honest
        // behavior this check wants to encourage -- only succeeded:true
        // paired with a defect admission is the real contradiction.
        assert!(!preflight_annotations(&state).iter().any(|f| f.label == "succeeded iteration admits a known defect"));
    }

    #[test]
    fn no_defect_admission_finding_for_a_genuinely_clean_success() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_IMPLEMENT, 1, "shipped the retry-on-failure feature, all real acceptance criteria verified", vec![]));
        assert!(!preflight_annotations(&state).iter().any(|f| f.label == "succeeded iteration admits a known defect"));
    }

    #[test]
    /// Real gap found live by the stress test, 2026-08-06, same bug shape as
    /// no_price_ceiling's own fix: a real, unfixed defect admission used to
    /// silently vanish from risk findings the moment any unrelated iteration
    /// followed it, even though the defect was never actually addressed.
    fn defect_admission_survives_an_unrelated_later_iteration() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(
            STAGE_IMPLEMENT,
            1,
            "Shipped the retry-on-failure feature. Known issue: it crashes on a null message id, not fixed yet, workaround needed before real use.",
            vec![],
        ));
        state.history.push(iteration(STAGE_IMPLEMENT, 2, "unrelated real work on a completely different feature entirely", vec![]));
        let findings = preflight_annotations(&state);
        assert!(
            findings.iter().any(|f| f.label == "succeeded iteration admits a known defect"),
            "the defect was never actually fixed -- it must still be flagged: {findings:?}"
        );
    }

    #[test]
    /// Real gap named directly in the goal doc's own §4.3 worked example
    /// ("this run's check-ins are too sparse"), live-verified before this
    /// check existed: checkin_every: 0 is accepted with zero validation and
    /// produced zero risk findings.
    fn flags_checkin_every_zero_as_effectively_disabled() {
        let mut state = RunState::new("run-preflight");
        state.criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 3, checkin_every: 0 };
        let findings = preflight_annotations(&state);
        assert!(
            findings.iter().any(|f| f.label == "mandatory check-in cadence effectively disabled" && f.evidence.contains("checkin_every is 0")),
            "got: {findings:?}"
        );
    }

    #[test]
    fn flags_checkin_every_at_or_past_max_iterations_as_effectively_disabled() {
        let mut state = RunState::new("run-preflight");
        state.criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 3, checkin_every: 20 };
        let findings = preflight_annotations(&state);
        assert!(
            findings.iter().any(|f| f.label == "mandatory check-in cadence effectively disabled"),
            "checkin_every == max_iterations can never fire on its own before the ceiling does: {findings:?}"
        );

        let mut state2 = RunState::new("run-preflight-2");
        state2.criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 3, checkin_every: 500 };
        assert!(preflight_annotations(&state2).iter().any(|f| f.label == "mandatory check-in cadence effectively disabled"));
    }

    #[test]
    fn no_checkin_cadence_finding_for_a_real_sensible_cadence() {
        let state = RunState::new("run-preflight");
        // RunState::new's own default (checkin_every: 5, max_iterations: 20) --
        // a real, sensible cadence must never be flagged.
        assert!(!preflight_annotations(&state).iter().any(|f| f.label == "mandatory check-in cadence effectively disabled"));
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
        state.history.push(iteration(STAGE_TEST, 1, "added a real test asserting the empty-message case never calls sendText and keeps focus", vec![]));
        state.history.push(iteration(STAGE_IMPLEMENT, 2, "wrote code", vec![]));
        assert!(preflight_annotations(&state).is_empty());
    }

    #[test]
    fn does_not_flag_when_test_runs_after_implement_but_still_before_a_later_implement() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_IMPLEMENT, 1, "wrote code", vec![]));
        state.history.push(iteration(STAGE_TEST, 2, "added a real test asserting the empty-message case never calls sendText and keeps focus", vec![]));
        state.history.push(iteration(STAGE_VERIFY, 3, "verified", vec![]));
        // Still flags -- the FIRST implement had no test before it, and that's the
        // real historical fact this check reports; it does not retroactively clear.
        let findings = preflight_annotations(&state);
        assert!(findings.iter().any(|f| f.label == "no test stage before implement"));
    }

    #[test]
    /// Real gap found live by the stress test, same day as the review-gate
    /// rubber-stamp findings: a substance-free devsystem.test iteration
    /// ("tests pass") used to count exactly the same as real testing.
    /// Live-verified against the actual deployment before this fix: the risk
    /// annotation silently vanished.
    fn a_rubber_stamp_test_iteration_still_flags_as_missing_real_test_evidence() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_TEST, 1, "tests pass", vec![]));
        state.history.push(iteration(STAGE_IMPLEMENT, 2, "shipped a real feature, no actual tests written for it though", vec![]));
        let findings = preflight_annotations(&state);
        assert!(
            findings.iter().any(|f| f.label == "no test stage before implement"),
            "a rubber-stamp test iteration must not silently satisfy this check: {findings:?}"
        );
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
        state.approved_stage_proposals.push(proposal(None, None));
        state.added_stages.push("devsystem.some_new_role".into());
        let findings = preflight_annotations(&state);
        assert!(findings.iter().any(|f| f.label == "no price ceiling set"));
    }

    #[test]
    /// Real gap found live by the stress test, twenty-fifth run, 2026-08-06:
    /// price_ceiling is never actually enforced against a real bid anywhere
    /// in this codebase, so a real `0` conveys no more real protection than
    /// `None` does -- live-confirmed a proposal with `price_ceiling: 0`
    /// produced zero risk findings before this fix, a false "this is
    /// bounded" signal for a role that's exactly as unbounded as one with no
    /// ceiling at all.
    fn flags_a_new_service_proposal_with_a_zero_price_ceiling_same_as_none() {
        let mut state = RunState::new("run-preflight");
        state.approved_stage_proposals.push(proposal(None, Some(0)));
        state.added_stages.push("devsystem.some_new_role".into());
        let findings = preflight_annotations(&state);
        assert!(
            findings.iter().any(|f| f.label == "no price ceiling set"),
            "a real 0 ceiling must be flagged exactly like no ceiling at all, since neither is ever enforced: {findings:?}"
        );
    }

    #[test]
    fn does_not_flag_a_proposal_with_a_price_ceiling_set() {
        let mut state = RunState::new("run-preflight");
        state.approved_stage_proposals.push(proposal(None, Some(100)));
        state.added_stages.push("devsystem.some_new_role".into());
        assert!(preflight_annotations(&state).is_empty());
    }

    #[test]
    /// Real gap found live by the stress test, twenty-seventh run, 2026-08-06:
    /// a human trying to *fix* an already-live unbounded role by re-proposing
    /// the exact same stage_id with a real price_ceiling gets a genuine `200`
    /// (apply_proposal correctly reports AlreadyPresent -- the role's own
    /// service/tag really is unchanged), but the fix itself used to be
    /// silently discarded: the check took the *first* matching entry, always
    /// the original bad proposal. Proves the real fix: the latest real
    /// proposal for a given stage_id wins.
    fn a_later_bounded_re_proposal_for_the_same_stage_id_clears_the_earlier_unbounded_one() {
        let mut state = RunState::new("run-preflight");
        state.added_stages.push("devsystem.some_new_role".into());
        // The original, bad proposal -- approved first, real price_ceiling never set.
        state.approved_stage_proposals.push(proposal(None, None));
        assert!(
            preflight_annotations(&state).iter().any(|f| f.label == "no price ceiling set"),
            "the original unbounded proposal must be flagged before any fix attempt"
        );
        // The real "fix" attempt: same stage_id, this time with a real ceiling.
        // apply_proposal itself would report AlreadyPresent for this (the role's
        // own service/tag is unchanged) -- both real call sites still push it here.
        state.approved_stage_proposals.push(proposal(None, Some(100)));
        assert!(
            preflight_annotations(&state).is_empty(),
            "the later, real, bounded re-proposal must actually clear the risk, not be silently ignored"
        );
    }

    #[test]
    fn does_not_flag_a_proposal_that_reuses_an_existing_service() {
        let mut state = RunState::new("run-preflight");
        state.approved_stage_proposals.push(proposal(Some("android-build-box"), None));
        state.added_stages.push("devsystem.some_new_role".into());
        assert!(preflight_annotations(&state).is_empty());
    }

    #[test]
    /// Real gap found live by the stress test, 2026-08-06: this check used to
    /// only look at the LATEST iteration's own proposals, so a genuinely
    /// unbounded-cost role, still live in the run's own spec, silently
    /// stopped being flagged the moment any unrelated iteration followed it.
    /// `approved_stage_proposals` (added 2026-08-06, the fix for a separate,
    /// bigger real gap -- see that field's own doc comment) is a flat,
    /// append-only record rather than iteration-scoped, so this specific bug
    /// shape can no longer recur structurally -- kept as a real regression
    /// test anyway, still proving an unrelated later iteration doesn't erase
    /// a still-live risk.
    fn still_flags_an_unbounded_role_after_an_unrelated_later_iteration() {
        let mut state = RunState::new("run-preflight");
        state.approved_stage_proposals.push(proposal(None, None));
        state.added_stages.push("devsystem.some_new_role".into());
        // The costly role is still live in the spec, but the LATEST iteration
        // is unrelated and carries no proposals at all.
        state.history.push(iteration(STAGE_IMPLEMENT, 2, "unrelated real work, nothing to do with the proposal", vec![]));
        let findings = preflight_annotations(&state);
        assert!(
            findings.iter().any(|f| f.label == "no price ceiling set"),
            "the risk is still real -- the role never got a price_ceiling -- so it must still be flagged: {findings:?}"
        );
    }

    #[test]
    fn does_not_flag_an_unbounded_proposal_that_was_never_actually_applied() {
        let mut state = RunState::new("run-preflight");
        // A real proposal with no price_ceiling was made, but it was rejected
        // (or simply never approved) -- never added to the live spec (and so,
        // matching real production behavior, never pushed to
        // approved_stage_proposals either), so there's no real, live cost
        // risk to warn about.
        state.history.push(iteration(STAGE_REVIEW, 1, "proposing a costly new role", vec![proposal(None, None)]));
        assert!(
            !preflight_annotations(&state).iter().any(|f| f.label == "no price ceiling set"),
            "a proposal that never became a real, live role isn't a real, live cost risk"
        );
    }

    #[test]
    /// Real regression, same day as `approved_stage_proposals` shipped
    /// (2026-08-06): switching this check to scan *only* the new field
    /// would silently drop every real risk approved before that field
    /// existed -- live-confirmed against the actual deployed
    /// webconference-android run, whose real `devsystem.document_extraction`
    /// risk (proposed via a real iteration, sitting only in `history`)
    /// vanished the moment the switch-only version deployed. Proves the real
    /// fix: `history.proposals` stays a real source too, not replaced.
    fn still_flags_an_unbounded_role_recorded_only_in_history_before_the_new_field_existed() {
        let mut state = RunState::new("run-preflight");
        // Deliberately NOT pushed to approved_stage_proposals -- simulates a
        // real state.json persisted before that field existed, where history
        // is the only real record.
        state.history.push(iteration(STAGE_REVIEW, 1, "proposing a costly new role", vec![proposal(None, None)]));
        state.added_stages.push("devsystem.some_new_role".into());
        assert!(state.approved_stage_proposals.is_empty(), "this test's whole point is an empty new field, real pre-existing data only");
        let findings = preflight_annotations(&state);
        assert!(
            findings.iter().any(|f| f.label == "no price ceiling set"),
            "a real, still-live, still-unbounded role must stay flagged from its history record alone: {findings:?}"
        );
    }

    #[test]
    fn flags_real_progress_with_no_review_role_declared() {
        let spec = crate::plan_only_spec("run-process", None);
        let mut state = RunState::new("run-process");
        // A custom-named stage, on purpose -- proves this check doesn't match on
        // "devsystem.implement" specifically, since real runs use project-specific
        // stage names (e.g. devsystem.android_native_bridge).
        for i in 1..=3 {
            state.history.push(iteration("devsystem.android_native_bridge", i, "real work", vec![]));
        }
        let findings = process_annotations(&spec, &state);
        assert!(findings.iter().any(|f| f.label == "no review role declared despite real progress"));
    }

    #[test]
    fn does_not_flag_before_the_minimum_iteration_count() {
        let spec = crate::plan_only_spec("run-process", None);
        let mut state = RunState::new("run-process");
        state.history.push(iteration("devsystem.android_native_bridge", 1, "real work", vec![]));
        state.history.push(iteration("devsystem.android_native_bridge", 2, "real work", vec![]));
        assert!(process_annotations(&spec, &state).is_empty(), "2 successful iterations is under the minimum -- must not flag yet");
    }

    #[test]
    fn does_not_flag_failed_iterations_toward_the_count() {
        let spec = crate::plan_only_spec("run-process", None);
        let mut state = RunState::new("run-process");
        for i in 1..=3 {
            state.history.push(IterationRecord {
                run_id: "run-process".into(),
                stage: "devsystem.android_native_bridge".into(),
                iteration: i,
                feedback: "failed attempt".into(),
                proposals: vec![],
                succeeded: false,
                requirement_indices: Vec::new(),
            });
        }
        assert!(process_annotations(&spec, &state).is_empty(), "failed iterations aren't real progress -- must not count toward the threshold");
    }

    #[test]
    fn does_not_flag_once_review_is_declared() {
        let spec = crate::full_spec("run-process", None);
        let mut state = RunState::new("run-process");
        for i in 1..=3 {
            state.history.push(iteration("devsystem.android_native_bridge", i, "real work", vec![]));
        }
        assert!(process_annotations(&spec, &state).is_empty(), "full_spec declares review -- must not flag a run that already has it");
    }

    fn requirement(statement: &str, criteria: Vec<&str>) -> crate::runner::Requirement {
        crate::runner::Requirement {
            statement: statement.into(),
            acceptance_criteria: criteria.into_iter().map(String::from).collect(),
            verified: false,
            verified_criteria: Vec::new(),
            auto_judge: false,
            proposed_by: None,
        }
    }

    #[test]
    /// Real gap named directly in the goal doc's own §4.3 -- the second
    /// worked example, "this role's acceptance criteria are too vague to be
    /// deterministic". "works" clears add_requirement's own
    /// MIN_ACCEPTANCE_CRITERION_ALNUM_CHARS gate (5 alphanumeric characters)
    /// but leaves the actual behavior entirely to the LLM's own judgment.
    fn flags_a_requirement_with_a_too_vague_acceptance_criterion() {
        let mut state = RunState::new("run-preflight");
        state.requirements.push(requirement("WHEN a user sends a message, THE SYSTEM SHALL deliver it", vec!["works"]));
        let findings = preflight_annotations(&state);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].label, "acceptance criteria too vague to be deterministic");
        assert!(findings[0].evidence.contains("requirement #0"));
        assert!(findings[0].evidence.contains("criterion #0"));
    }

    #[test]
    fn does_not_flag_a_genuinely_specific_acceptance_criterion() {
        let mut state = RunState::new("run-preflight");
        state.requirements.push(requirement("WHEN a user sends a message, THE SYSTEM SHALL deliver it", vec!["message arrives at the peer"]));
        assert!(preflight_annotations(&state).is_empty());
    }

    #[test]
    fn flags_the_first_vague_criterion_even_when_a_later_one_in_a_different_requirement_is_fine() {
        let mut state = RunState::new("run-preflight");
        state.requirements.push(requirement("WHEN x, THE SYSTEM SHALL y", vec!["message arrives at the peer"]));
        state.requirements.push(requirement("WHEN a, THE SYSTEM SHALL b", vec!["is fast"]));
        let findings = preflight_annotations(&state);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].evidence.contains("requirement #1"), "must name the real requirement that's actually vague, not the first one checked");
    }
}
