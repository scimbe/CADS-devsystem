//! Pre-flight risk annotations (`docs/plan-stage.md`'s own documented "next slice",
//! proposal §5): real, mechanical checks over a run's history, meant to be seeded
//! into the canvas session *before* a human ever opens it. Every finding here
//! inspects real [`IterationRecord`]/[`StageProposal`] data -- nothing invented, no
//! LLM judgment call, just pattern checks a human reviewer would otherwise have to
//! do by hand.

use crate::runner::{distinct_word_count, inline_code_escape, RunState};
use crate::{contains_bidi_control_char, STAGE_IMPLEMENT, STAGE_REVIEW, STAGE_TEST};
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
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct RiskAnnotation {
    pub label: String,
    pub evidence: String,
    /// A structured re-propose target for the GUI's own "Fix it" action (#382
    /// goal doc §7, 2026-08-07) -- `None` for every risk kind except
    /// `no_price_ceiling`, the one case with a real, always-safe fix (re-
    /// propose the identical role with a real `price_ceiling` this time,
    /// exactly the "natural fix" this file's own doc comments already
    /// describe). Deliberately a structured field, not re-derived by parsing
    /// `evidence`'s human-readable text in the frontend -- this project's own
    /// "no invented signal" discipline (see the vague-acceptance-criteria and
    /// defect-admission checks above) applies here too: the real `stage_id`/
    /// `tag` are already known at the point this risk is built, so that's the
    /// one honest source for them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_target: Option<RiskFixTarget>,
}

/// The real `stage_id`/`tag` a "Fix it" GUI action needs to re-propose an
/// already-live-but-unbounded role with a real `price_ceiling` this time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RiskFixTarget {
    pub stage_id: String,
    pub tag: String,
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
    findings.extend(security_keyword_hit(state));
    findings.extend(missing_test_before_implement(state));
    if let Some(a) = no_review_for_succeeded_work(state) {
        findings.push(a);
    }
    findings.extend(no_price_ceiling(state));
    findings.extend(succeeded_iteration_admits_a_defect(state));
    if let Some(a) = checkin_cadence_effectively_disabled(state) {
        findings.push(a);
    }
    if let Some(a) = checkin_watermark_identity_drift(state) {
        findings.push(a);
    }
    findings.extend(vague_acceptance_criteria(state));
    findings.extend(historical_bidi_control_character(state));
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
// Real gap found live 2026-08-06, applying the exact same lens that just
// found `no_price_ceiling`'s own "stops at the first match" bug to every
// other check in this file: this one had the identical shape, an early
// `return Some(...)` inside a nested loop over every requirement/criterion.
// Live-confirmed against a real run with two separate requirements, each
// with its own genuinely vague criterion ("works", "is fast") -- only the
// FIRST ever got flagged, the second stayed completely invisible. Now
// collects every real vague criterion, not just the first one found.
fn vague_acceptance_criteria(state: &RunState) -> Vec<RiskAnnotation> {
    let mut findings = Vec::new();
    for (i, r) in state.requirements.iter().enumerate() {
        for (ci, c) in r.acceptance_criteria.iter().enumerate() {
            if distinct_word_count(c) < MIN_ACCEPTANCE_CRITERION_DISTINCT_WORDS {
                findings.push(RiskAnnotation {
                    label: "acceptance criteria too vague to be deterministic".into(),
                    evidence: format!(
                        "requirement #{i}'s acceptance criterion #{ci} (\"{c}\") has fewer than \
                         {MIN_ACCEPTANCE_CRITERION_DISTINCT_WORDS} distinct words -- goal doc §1's own \
                         commitment (\"acceptance criteria specific enough to leave no real decision to \
                         the LLM\") needs more than a short label to actually constrain what a \
                         role-filler builds"
                    ),
                    fix_target: None,
                });
            }
        }
    }
    findings
}

/// Defense-in-depth for the bidi-control-character (Trojan Source,
/// CVE-2021-42574) class closed at every real write-time entry point this
/// session (#382 goal doc §8, 2026-08-06: requirement statement/criteria, milestones,
/// backlog, custom-panel title, stage-proposal rationale). Those fixes only
/// guard *new* writes -- they can't retroactively clean data already
/// persisted before they shipped, and they'd say nothing about a future field
/// that adds free text without remembering this check. A real, live audit of
/// every production `state.json` this repo actually has (110 files) found
/// zero contamination, but "audited once and found clean" isn't the same
/// guarantee as "structurally can't happen again" -- this is the same
/// mechanical-check discipline this whole file already applies to other
/// process gaps, seeded into the canvas session so a human sees it without
/// having to think to look. Deliberately scoped to the exact same fields the
/// write-time fixes cover, not `feedback` or panel `html` (the latter is
/// untrusted-by-design and sandboxed, same reasoning as the write-time fix).
fn historical_bidi_control_character(state: &RunState) -> Vec<RiskAnnotation> {
    let mut findings = Vec::new();
    let mut flag = |field: String, text: &str| {
        if contains_bidi_control_char(text) {
            findings.push(RiskAnnotation {
                label: "stored text contains a Unicode bidi control character".into(),
                evidence: format!(
                    "{field} contains a bidi control character (e.g. a right-to-left override) -- its \
                     displayed text may not match what's actually stored; this predates the write-time \
                     gate that now rejects new occurrences, or arrived before this specific field was covered"
                ),
                fix_target: None,
            });
        }
    };
    for (i, r) in state.requirements.iter().enumerate() {
        flag(format!("requirement #{i}'s statement"), &r.statement);
        for (ci, c) in r.acceptance_criteria.iter().enumerate() {
            flag(format!("requirement #{i}'s acceptance criterion #{ci}"), c);
        }
    }
    for (i, m) in state.milestones.iter().enumerate() {
        flag(format!("milestone #{i}'s description"), &m.description);
    }
    for (i, b) in state.backlog.iter().enumerate() {
        flag(format!("backlog item #{i}'s text"), &b.text);
    }
    for p in &state.custom_panels {
        flag(format!("custom panel {:?}'s title", p.id), &p.title);
    }
    for p in &state.pending_panel_proposals {
        flag(format!("pending panel proposal {:?}'s title", p.id), &p.title);
    }
    for p in &state.pending_panel_edit_proposals {
        flag(format!("pending panel edit proposal {:?}'s new_title", p.id), &p.new_title);
    }
    for p in &state.pending_stage_proposals {
        flag(format!("pending stage proposal {:?}'s rationale", p.id), &p.proposal.rationale);
    }
    for p in &state.approved_stage_proposals {
        flag(format!("approved stage proposal for {:?}'s rationale", p.stage_id), &p.rationale);
    }
    findings
}

/// Real evaluator finding, issue #42 (suggestion #1): `checkin_acknowledged_through`
/// is a bare position into `history`, and issue #38's own live incident showed a
/// history repair (compacting out a duplicate record) can silently re-point it at
/// the wrong record -- the operator's own prior acknowledgment then covers an
/// iteration they never actually reviewed, with no signal anywhere that this
/// happened. `checkin_acknowledged_through_id` (added alongside the position,
/// same firing as this check) records which record was *actually* being
/// acknowledged; this check is the mechanical, always-on comparison that catches
/// the two disagreeing again, rather than requiring a human evaluator to notice
/// by hand the way issue #42 itself was found. Deliberately only fires when
/// there's a real, checkable disagreement -- a `None` id (every acknowledgment
/// recorded before this field existed) is a legacy gap, not fresh evidence of
/// drift, and is left alone rather than false-positiving on history that
/// predates the mechanism entirely.
fn checkin_watermark_identity_drift(state: &RunState) -> Option<RiskAnnotation> {
    let through = state.checkin_acknowledged_through;
    if through == 0 {
        return None;
    }
    let recorded_id = state.checkin_acknowledged_through_id.as_ref()?;
    let current = state.history.get((through - 1) as usize);
    let current_id = current.and_then(|h| h.id.as_ref());
    if current_id != Some(recorded_id) {
        return Some(RiskAnnotation {
            label: "check-in acknowledgment watermark no longer matches the record it was recorded against".into(),
            evidence: format!(
                "checkin_acknowledged_through is {through}, recorded against iteration id {recorded_id:?} \
                 at acknowledgment time, but history position {through} now holds id {current_id:?} -- \
                 the history array was very likely mutated (compacted, reordered, or a record was \
                 removed) since the last acknowledgment, so this watermark may no longer cover the \
                 iteration a human actually reviewed (the exact real gap issue #42 found)"
            ),
            fix_target: None,
        });
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
            fix_target: None,
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
            fix_target: None,
        });
    }
    None
}

/// "touches auth" (proposal §5's own example): a real iteration's feedback, or any
/// proposal it carries, mentions a security-relevant keyword.
///
/// `p.stage_id` below is `inline_code_escape`d, not raw-interpolated -- a real gap
/// found live by the incompetent-agent stress test (#382 goal doc §8, 2026-08-06),
/// same "role-filler-controlled free text must not forge markdown structure" class
/// already closed for the Requirements Markdown export (stress-test check #9), just
/// never checked for `RiskAnnotation.evidence` until this run: `stage_id` has no
/// character restriction at any real entry point, and this evidence string flows
/// straight into `checkin.rs`'s own `.plan.md` artifact -- a real backtick-plus-
/// newline in a proposed `stage_id` broke out of the single-backtick span and
/// forged a fake markdown heading into the exact file `ecc-plan-canvas` renders for
/// a human to read and decide `approve`/`request-changes` on.
///
/// **The fourth real instance of the same "once satisfied/flagged, forgotten"
/// staleness bug found in one day, closed 2026-08-07**: this used to only ever
/// check the LATEST iteration -- flagged live, then genuinely vanished the moment
/// one completely unrelated iteration followed it, even though the real security-
/// sensitive change was still sitting there, still unreviewed. Live-confirmed
/// before this fix: a real iteration rewriting session auth-token handling
/// correctly flagged `touches auth/security`; the very next, totally unrelated
/// iteration (a README typo fix) made it disappear entirely. Unlike the
/// `no_review_for_succeeded_work`/`missing_test_before_implement` fixes earlier the
/// same day, this isn't a coverage-tracking question with a "since the last X"
/// window -- a security-relevant change is a real, permanent historical fact about
/// this run, the same as a defect admission or a bidi character. Fixed the same way
/// `succeeded_iteration_admits_a_defect` already does: scan all of history, collect
/// every real security-relevant iteration, not just the latest -- `Vec` instead of
/// `Option`, same real tradeoff already accepted there (a keyword mentioned once and
/// never actually reviewed keeps nagging; that's a smaller, named cost than silently
/// hiding a real, still-unreviewed security-sensitive change).
fn security_keyword_hit(state: &RunState) -> Vec<RiskAnnotation> {
    let mut findings = Vec::new();
    for h in &state.history {
        let feedback_lower = h.feedback.to_lowercase();
        if let Some(kw) = SECURITY_KEYWORDS.iter().find(|kw| feedback_lower.contains(**kw)) {
            findings.push(RiskAnnotation {
                label: "touches auth/security".into(),
                evidence: format!("iteration {}'s feedback mentions \"{kw}\"", h.iteration),
                fix_target: None,
            });
            continue;
        }
        for p in &h.proposals {
            let text = format!("{} {}", p.tag, p.rationale).to_lowercase();
            if let Some(kw) = SECURITY_KEYWORDS.iter().find(|kw| text.contains(**kw)) {
                findings.push(RiskAnnotation {
                    label: "touches auth/security".into(),
                    evidence: format!("proposal {}'s rationale mentions \"{kw}\"", inline_code_escape(&p.stage_id)),
                    fix_target: None,
                });
                break;
            }
        }
    }
    findings
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
///
/// **A second real gap in this same check, found live 2026-08-06 applying
/// the identical lens that found `no_price_ceiling`'s and
/// `vague_acceptance_criteria`'s own "only the first/latest match" bugs**:
/// the "scan all of history, stays flagged" fix above solved the defect
/// vanishing over time, but this still only ever returned ONE
/// `RiskAnnotation` via `Iterator::find` -- if two DIFFERENT succeeded
/// iterations each admit a genuinely different, unfixed defect, only the
/// most recent one's evidence was ever shown, the other silently invisible.
/// Live-confirmed: two real iterations, one admitting an unfixed session-
/// expiry security gap, the other an unfixed search crash, produced exactly
/// one finding -- the security defect was completely hidden. Now collects
/// every real defect-admitting succeeded iteration, not just the latest.
///
/// **A third real gap, issue #54, 2026-08-07**: this check had no awareness of
/// *which stage* produced the feedback -- and a [`STAGE_REVIEW`] iteration's
/// entire job is to find and report defects in *other* work. A real,
/// substantive review that found and honestly documented a genuine crash
/// (`webconference-android` iteration 22, live) got flagged identically to an
/// implementer who shipped broken code -- the more honest and thorough a
/// review, the more risk a run accrued, exactly backwards from what the
/// mandatory review gate exists to encourage, and directly at odds with
/// `no_review_for_succeeded_work` (a real review clears that risk, then
/// immediately trades it for this one). A `STAGE_REVIEW` iteration reports a
/// defect it found in someone else's work, not one it shipped -- succeeding
/// at that job is not evidence of admitting shipped-defective work, so
/// review iterations are excluded from this check entirely. Every other
/// stage (`implement`, `test`, `verify`, ...) is unaffected -- shipping code
/// with an admitted, unfixed defect stays flagged exactly as before.
///
/// Not fixed here, an honestly-named separate gap the same issue reports:
/// the phrase list itself is trivially evaded by rewording ("not fixed" vs.
/// "still awaits repair," semantically identical, only one flagged) -- adding
/// one more synonym wouldn't close that, only move the goalpost, and the
/// issue's own suggested real fix (a structural "open defect" field on the
/// record instead of prose matching) is separate, larger work.
fn succeeded_iteration_admits_a_defect(state: &RunState) -> Vec<RiskAnnotation> {
    state
        .history
        .iter()
        .filter(|h| {
            h.succeeded && h.stage != STAGE_REVIEW && {
                let feedback_lower = h.feedback.to_lowercase();
                DEFECT_ADMISSION_PHRASES.iter().any(|p| feedback_lower.contains(*p))
            }
        })
        .filter_map(|h| {
            let feedback_lower = h.feedback.to_lowercase();
            let phrase = DEFECT_ADMISSION_PHRASES.iter().find(|p| feedback_lower.contains(**p))?;
            Some(RiskAnnotation {
                label: "succeeded iteration admits a known defect".into(),
                evidence: format!(
                    "iteration {}'s own feedback contains \"{phrase}\" while marked succeeded:true -- goal \
                     doc §5's Vertragsgemäße/Sachmangelfreie row names this exact gap: nothing else blocks \
                     marking work \"done\" with open, known defects. No later iteration signals it was \
                     ever fixed, so this stays flagged.",
                    h.iteration
                ),
                fix_target: None,
            })
        })
        .collect()
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
///
/// **Real gap found live 2026-08-07, the same "once satisfied, satisfied forever"
/// shape as `no_price_ceiling`'s careless-re-proposal bypass and
/// `no_review_for_succeeded_work`'s stale-review gap, both fixed the same day**:
/// this only ever checked the FIRST real `devsystem.implement` iteration
/// (`Iterator::position`, not scanning every occurrence), so one real test early
/// in a run's history satisfied this permanently -- a SECOND, later `implement`
/// round shipping brand-new work with zero fresh test coverage since was never
/// checked at all, because a real test from long before the first implement was
/// still technically "before" it too. Fixed with a real sliding window: each
/// `devsystem.implement` occurrence is checked against only the history SINCE the
/// previous `devsystem.implement` (or the run's start, for the first one) --
/// matching this file's own "collect every real violation, not just the first"
/// precedent (`no_price_ceiling`, twenty-seventh stress-test run) and returning
/// `Vec` instead of `Option` for the same reason.
fn missing_test_before_implement(state: &RunState) -> Vec<RiskAnnotation> {
    let mut findings = Vec::new();
    let mut window_start = 0;
    for (idx, r) in state.history.iter().enumerate() {
        if r.stage != STAGE_IMPLEMENT {
            continue;
        }
        let real_test_since_last_implement = state.history[window_start..idx].iter().any(|h| {
            h.stage == STAGE_TEST && {
                let trimmed = h.feedback.trim();
                trimmed.chars().count() >= MIN_TEST_FEEDBACK_LEN && distinct_word_count(trimmed) >= MIN_TEST_DISTINCT_WORDS
            }
        });
        if !real_test_since_last_implement {
            findings.push(RiskAnnotation {
                label: "no test stage before implement".into(),
                evidence: format!(
                    "devsystem.implement ran at iteration {}, with no devsystem.test iteration \
                     since the previous implement (or the run's start) that's substantive enough \
                     to count as real evidence testing happened ({MIN_TEST_FEEDBACK_LEN}+ \
                     characters and {MIN_TEST_DISTINCT_WORDS}+ distinct words of feedback, not a \
                     rubber-stamp)",
                    r.iteration
                ),
                fix_target: None,
            });
        }
        window_start = idx + 1;
    }
    findings
}

/// Goal doc §5's own named "most direct next step" toward the quality-bar table:
/// "a role-filler can mark an iteration `succeeded: true` without passing
/// through `review`... at all." Deliberately advisory, not a hard block --
/// same trust level `missing_test_before_implement`/`no_price_ceiling` already
/// get, not the narrower hard `409` gap #2 already closed for
/// `toggle_requirement` specifically (`qualifying_review_evidence`,
/// `pipeline/src/runner.rs`). That gate only fires for a run that opted
/// `review` into its own spec and only blocks marking a *requirement*
/// verified; this is the broader, still-open half of the same gap -- a run
/// with real succeeded work and NO substantive `devsystem.review` iteration
/// anywhere in its history at all, regardless of whether the run declared a
/// review role or touches requirements. Turning this into a hard block is a
/// real, separate, later increment (per §4.3, "the user always leads") -- this
/// makes the gap visible first, the same safe rollout shape every other real
/// quality signal in this file already took before (if ever) becoming a gate.
fn no_review_for_succeeded_work(state: &RunState) -> Option<RiskAnnotation> {
    // Real gap found live 2026-08-07, the same "once satisfied, satisfied forever"
    // shape as `no_price_ceiling`'s own careless-re-proposal bypass fixed the same
    // day (`runner::price_ceiling_for`'s own doc comment) -- confirmed against the
    // actual `webconference-android` run before fixing: a real review (iteration
    // 12) genuinely cleared this risk, but real, NEW succeeded work (iteration 13,
    // `devsystem.improve`) landed right after it and was never itself reviewed --
    // the risk stayed silently gone regardless, since the old check only asked "has
    // there EVER been a substantive review anywhere," not "has the run's own most
    // recent succeeded work actually been covered by one." A run could ship
    // unlimited further unreviewed work forever after a single early review and
    // this would never flag it again.
    let last_work_idx = state.history.iter().rposition(|r| r.succeeded && r.stage != STAGE_REVIEW)?;
    let has_real_review_since = state.history[last_work_idx..].iter().any(|r| {
        r.stage == STAGE_REVIEW && {
            let trimmed = r.feedback.trim();
            trimmed.chars().count() >= MIN_TEST_FEEDBACK_LEN && distinct_word_count(trimmed) >= MIN_TEST_DISTINCT_WORDS
        }
    });
    if has_real_review_since {
        None
    } else {
        Some(RiskAnnotation {
            label: "no review stage for real, succeeded work".into(),
            evidence: format!(
                "this run has at least one succeeded:true iteration with no substantive \
                 devsystem.review iteration since it -- {MIN_TEST_FEEDBACK_LEN}+ characters and \
                 {MIN_TEST_DISTINCT_WORDS}+ distinct words of feedback, not a rubber-stamp, and not \
                 just an earlier review of now-superseded work -- advisory today, not a block \
                 (goal doc §5)"
            ),
            fix_target: None,
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
///
/// **Update, 2026-08-07**: the "never enforced anywhere" premise above is now only
/// partially true, not fully -- `set_role_fill_mode`'s real direct-accept path
/// (`web/src/main.rs`) now genuinely rejects accepting a bid priced over a role's
/// own real `price_ceiling` (via [`crate::runner::price_ceiling_for`], the same
/// lookup this check uses). Auction-cleared bids still aren't checked against it
/// anywhere -- this risk still correctly fires for those, and `evidence` below
/// says so honestly rather than claiming the whole gap is closed.
///
/// **Correction, same day, later firing**: an earlier version of this note (and the
/// goal doc) framed closing the "auction-cleared" half as needing a change to
/// CADS-Tunnel's own `convene_with_policy` -- checked, not assumed, and that
/// framing was wrong. `convene`/`convene_with_policy` are never actually called
/// anywhere in this repo's real request-handling code at all (confirmed by
/// grepping every real call site; the only real hits are this crate's own unit
/// tests and `ct_common`'s). `POST /api/runs/{id}/iterate` has no auction-winner
/// check of any kind -- any caller can submit real work for any stage regardless
/// of bidding, by this project's own established "the signature is the
/// authentication" convention. There is no real "a bid won the auction, now it may
/// submit work" code path in production to attach a ceiling check to at all --
/// the honest remaining gap is a genuinely open architectural question (should
/// `/iterate` ever require proof of winning?), not a smaller cross-repo patch,
/// and not guessed at here.
///
/// `p.stage_id` in the evidence string below is `inline_code_escape`d for the
/// same reason as `security_keyword_hit`'s own doc comment above -- this is the
/// exact evidence line whose raw, unescaped `stage_id` first proved the markdown-
/// forgery gap live (a stage_id containing a backtick and a newline injected a
/// real fake heading into `checkin.rs`'s `.plan.md` artifact through this line).
fn no_price_ceiling(state: &RunState) -> Vec<RiskAnnotation> {
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
    // Real gap found live 2026-08-06, checking the actual deployed
    // webconference-android run rather than assuming this already-audited
    // check was complete: `devsystem.review` has the identical unbounded
    // shape as `devsystem.document_extraction` (both `use_existing_service:
    // None`, `price_ceiling: None`, confirmed live) -- but this used to be a
    // single `Option<RiskAnnotation>`, built on `Iterator::find`, which stops
    // at the FIRST unbounded role in `added_stages` order and never checks
    // the rest. A run with two simultaneously-unbounded roles only ever
    // showed one of them, silently hiding the other -- the exact "a real risk
    // exists but nothing surfaces it" bug class this whole file exists to
    // catch, this time in one of its own checks. Now collects every real
    // unbounded role, not just the first.
    let unbounded: Vec<_> = state
        .added_stages
        .iter()
        .filter_map(|stage_id| crate::runner::latest_proposal_for_stage(state, stage_id))
        .filter(|p| p.use_existing_service.is_none() && p.price_ceiling.unwrap_or(0) == 0)
        .collect();
    unbounded
        .into_iter()
        .map(|p| RiskAnnotation {
            label: "no price ceiling set".into(),
            evidence: format!(
                "role {} is live in this run's own spec, was proposed needing a new service (no use_existing_service) with no real price_ceiling ({}), and nothing since has bounded what filling it could cost -- a real, positive price_ceiling IS now enforced against a directly-accepted bid (`set_role_fill_mode`, 2026-08-07), but auction-cleared bids still aren't checked against it anywhere, so 0 is still exactly as unbounded as unset for those",
                inline_code_escape(&p.stage_id),
                p.price_ceiling.map(|v| v.to_string()).unwrap_or_else(|| "none set".to_string())
            ),
            fix_target: Some(RiskFixTarget { stage_id: p.stage_id.clone(), tag: p.tag.clone() }),
        })
        .collect()
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
        fix_target: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AbortCriteria, IterationRecord, StageProposal, STAGE_REVIEW, STAGE_VERIFY};

    fn iteration(stage: &str, iteration: u32, feedback: &str, proposals: Vec<StageProposal>) -> IterationRecord {
        IterationRecord { run_id: "run-preflight".into(), stage: stage.into(), iteration, feedback: feedback.into(), proposals, succeeded: true, requirement_indices: Vec::new(), ..Default::default() }
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
    /// Real gap found live 2026-08-07: a real, unreviewed security-sensitive
    /// change used to disappear from the risk list the moment ANY unrelated
    /// iteration followed it, even though the sensitive change itself was still
    /// sitting there, still unreviewed. A security-relevant fact must stay
    /// visible, the same as a defect admission does.
    fn a_security_keyword_hit_survives_a_later_unrelated_iteration() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_IMPLEMENT, 1, "rewrote the session auth token handling, real security-sensitive change", vec![]));
        let findings = preflight_annotations(&state);
        assert!(
            findings.iter().any(|f| f.label == "touches auth/security"),
            "sanity check: the security-sensitive iteration above must genuinely flag first"
        );

        state.history.push(iteration(STAGE_IMPLEMENT, 2, "fixed an unrelated typo in the README, nothing else changed", vec![]));
        let findings = preflight_annotations(&state);
        assert!(
            findings.iter().any(|f| f.label == "touches auth/security" && f.evidence.contains("iteration 1")),
            "a real security-sensitive change must not vanish just because an unrelated iteration followed it: {findings:?}"
        );
    }

    #[test]
    fn no_security_finding_when_nothing_mentions_a_keyword() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_TEST, 1, "added a real unit test covering the empty-input edge case and confirmed it fails without the fix", vec![]));
        state.history.push(iteration(STAGE_IMPLEMENT, 2, "wrote a helper function", vec![]));
        // Isolates the security check specifically -- this fixture legitimately
        // trips the separate "no review for succeeded work" finding below (real
        // succeeded work, no devsystem.review iteration at all), so this can't
        // assert is_empty() anymore.
        assert!(!preflight_annotations(&state).iter().any(|f| f.label == "touches auth/security"));
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
            ..Default::default()
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
    /// Real gap, live-found 2026-08-06 applying the identical lens that found
    /// `no_price_ceiling`'s and `vague_acceptance_criteria`'s own "only the
    /// first/latest match" bugs. Two different succeeded iterations, each
    /// admitting a genuinely different, unfixed defect -- both must be
    /// flagged, not just the most recent one.
    fn flags_every_distinct_admitted_defect_not_just_the_most_recent() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(
            STAGE_IMPLEMENT,
            1,
            "Shipped the login flow. Known issue: session tokens never expire, a real security gap not fixed yet.",
            vec![],
        ));
        state.history.push(iteration(
            STAGE_IMPLEMENT,
            2,
            "Shipped the message search feature. Known bug: search crashes on empty query, not implemented a guard for it yet.",
            vec![],
        ));
        let findings = preflight_annotations(&state);
        let defects: Vec<_> = findings.iter().filter(|f| f.label == "succeeded iteration admits a known defect").collect();
        assert_eq!(defects.len(), 2, "both real, distinct, unfixed defects must be flagged, not just the most recent: {findings:?}");
        assert!(defects.iter().any(|f| f.evidence.contains("iteration 1")));
        assert!(defects.iter().any(|f| f.evidence.contains("iteration 2")));
    }

    #[test]
    /// Real evaluator finding, issue #54, live on `webconference-android`: a
    /// `devsystem.review` iteration's entire job is to find and report defects in
    /// OTHER work -- doing that well used to get it flagged identically to an
    /// implementer who shipped broken code, exactly backwards from what the
    /// mandatory review gate exists to encourage (and directly at odds with
    /// `no_review_for_succeeded_work`: a real review clears that risk, then
    /// immediately trades it for this one).
    fn a_review_iteration_reporting_a_defect_it_found_is_not_flagged() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(
            STAGE_REVIEW,
            1,
            "Reviewed native-bridge/src/channel.rs. Found exactly one real defect and is reporting that \
             defect. The review itself shipped nothing defective whatsoever. The defect it found is not \
             fixed yet, because fixing it belongs to the implementing role rather than to the reviewer.",
            vec![],
        ));
        let findings = preflight_annotations(&state);
        assert!(
            !findings.iter().any(|f| f.label == "succeeded iteration admits a known defect"),
            "an honest review reporting a defect it found in someone else's work is the stage succeeding, \
             not admitting shipped-defective work: {findings:?}"
        );
    }

    #[test]
    /// The exclusion is stage-specific, not a blanket exemption for the phrase --
    /// an implementer who actually ships known-defective code must still be
    /// flagged exactly as before.
    fn an_implement_iteration_admitting_a_defect_is_still_flagged_even_though_review_is_exempt() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_REVIEW, 1, "Found a defect during review. Known issue: not fixed yet.", vec![]));
        state.history.push(iteration(STAGE_IMPLEMENT, 2, "Shipped it anyway. Known issue: not fixed yet, workaround needed.", vec![]));
        let findings = preflight_annotations(&state);
        let defects: Vec<_> = findings.iter().filter(|f| f.label == "succeeded iteration admits a known defect").collect();
        assert_eq!(defects.len(), 1, "only the real implement-stage admission is flagged, the review-stage one is exempt: {findings:?}");
        assert!(defects[0].evidence.contains("iteration 2"));
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
    fn no_watermark_drift_finding_when_never_acknowledged() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_TEST, 1, "a real iteration", vec![]));
        // checkin_acknowledged_through stays 0 (RunState::new's own default) --
        // "never acknowledged" must never be reported as drift.
        assert!(!preflight_annotations(&state).iter().any(|f| f.label.contains("watermark")));
    }

    #[test]
    fn no_watermark_drift_finding_for_a_legacy_acknowledgment_with_no_recorded_id() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_TEST, 1, "a real iteration", vec![]));
        state.checkin_acknowledged_through = 1;
        state.checkin_acknowledged_through_id = None; // acknowledged before this field existed
        assert!(!preflight_annotations(&state).iter().any(|f| f.label.contains("watermark")));
    }

    #[test]
    fn no_watermark_drift_finding_when_the_recorded_id_still_matches() {
        let mut state = RunState::new("run-preflight");
        let mut rec = iteration(STAGE_TEST, 1, "a real iteration", vec![]);
        rec.id = Some("real-id-abc".to_string());
        state.history.push(rec);
        state.checkin_acknowledged_through = 1;
        state.checkin_acknowledged_through_id = Some("real-id-abc".to_string());
        assert!(!preflight_annotations(&state).iter().any(|f| f.label.contains("watermark")));
    }

    #[test]
    fn flags_watermark_drift_when_the_position_now_holds_a_different_record() {
        let mut state = RunState::new("run-preflight");
        let mut rec = iteration(STAGE_TEST, 1, "the record that's really there now", vec![]);
        rec.id = Some("real-id-after-compaction".to_string());
        state.history.push(rec);
        state.checkin_acknowledged_through = 1;
        // Simulates issue #42's exact real incident: a human acknowledged
        // position 1 when it held a different record (this id), and a later
        // history compaction silently moved a different record into that slot.
        state.checkin_acknowledged_through_id = Some("real-id-before-compaction".to_string());
        let findings = preflight_annotations(&state);
        assert!(
            findings.iter().any(|f| {
                f.label == "check-in acknowledgment watermark no longer matches the record it was recorded against"
                    && f.evidence.contains("real-id-before-compaction")
                    && f.evidence.contains("real-id-after-compaction")
            }),
            "got: {findings:?}"
        );
    }

    #[test]
    fn flags_watermark_drift_when_the_position_no_longer_exists_at_all() {
        let mut state = RunState::new("run-preflight");
        let mut rec = iteration(STAGE_TEST, 1, "a real iteration", vec![]);
        rec.id = Some("real-id".to_string());
        state.history.push(rec);
        // Acknowledged through position 2, but a later removal shrank history
        // back to 1 record -- the acknowledged record is gone entirely, the
        // most extreme real case of the same drift.
        state.checkin_acknowledged_through = 2;
        state.checkin_acknowledged_through_id = Some("a-record-that-no-longer-exists".to_string());
        let findings = preflight_annotations(&state);
        assert!(
            findings.iter().any(|f| f.label == "check-in acknowledgment watermark no longer matches the record it was recorded against"),
            "got: {findings:?}"
        );
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
        // Isolates the test-before-implement check specifically -- this fixture
        // legitimately trips the separate "no review for succeeded work" finding
        // (real succeeded work, no devsystem.review iteration at all), so this
        // can't assert is_empty() anymore.
        assert!(!preflight_annotations(&state).iter().any(|f| f.label == "no test stage before implement"));
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
    /// Real gap found live 2026-08-07, same "once satisfied, satisfied forever"
    /// shape as no_price_ceiling's/no_review_for_succeeded_work's own fixes the
    /// same day: one real test early in a run used to satisfy this permanently,
    /// so a SECOND, later implement round shipping brand-new work got zero
    /// coverage-check at all -- the old test from long before still counted as
    /// "before" it. Each implement round now needs its own fresh test since the
    /// previous implement, not just any test anywhere earlier in history.
    fn a_later_implement_round_with_no_fresh_test_since_the_previous_one_is_flagged_on_its_own() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_TEST, 1, "added a real test asserting the empty-message case never calls sendText and keeps focus", vec![]));
        state.history.push(iteration(STAGE_IMPLEMENT, 2, "first real feature, genuinely covered by the test above", vec![]));
        assert!(
            !preflight_annotations(&state).iter().any(|f| f.label == "no test stage before implement"),
            "sanity check: the first implement round must genuinely be covered"
        );

        state.history.push(iteration(STAGE_VERIFY, 3, "verified the first feature", vec![]));
        state.history.push(iteration(STAGE_IMPLEMENT, 4, "a second, later real feature, no fresh test written for it at all", vec![]));
        let findings = preflight_annotations(&state);
        assert!(
            findings.iter().any(|f| f.label == "no test stage before implement" && f.evidence.contains("iteration 4")),
            "a later implement round with no test since the previous one must be flagged on its own, not silently covered by an old test: {findings:?}"
        );
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

    #[test]
    /// Goal doc §5's own named "most direct next step": "a role-filler can
    /// mark an iteration succeeded: true without passing through review... at
    /// all." Advisory, not the narrower hard `409` gap #2 already closed for
    /// `toggle_requirement` specifically.
    fn flags_succeeded_work_with_no_review_iteration_anywhere_in_history() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_IMPLEMENT, 1, "shipped a real feature", vec![]));
        let findings = preflight_annotations(&state);
        assert!(findings.iter().any(|f| f.label == "no review stage for real, succeeded work"), "got: {findings:?}");
    }

    #[test]
    fn does_not_flag_when_a_real_substantive_review_iteration_exists() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_IMPLEMENT, 1, "shipped a real feature", vec![]));
        state.history.push(iteration(STAGE_REVIEW, 2, "reviewed the diff line by line, confirmed the edge cases are covered and the naming is clear", vec![]));
        assert!(!preflight_annotations(&state).iter().any(|f| f.label == "no review stage for real, succeeded work"));
    }

    #[test]
    /// Same "rubber-stamp doesn't count" discipline as the sibling test-stage
    /// check above -- a one-line "looks good" must not silently satisfy this.
    fn a_rubber_stamp_review_iteration_still_flags_as_missing_real_review_evidence() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_IMPLEMENT, 1, "shipped a real feature", vec![]));
        state.history.push(iteration(STAGE_REVIEW, 2, "looks good", vec![]));
        let findings = preflight_annotations(&state);
        assert!(
            findings.iter().any(|f| f.label == "no review stage for real, succeeded work"),
            "a rubber-stamp review iteration must not silently satisfy this check: {findings:?}"
        );
    }

    #[test]
    /// Real gap found live 2026-08-07 against the actual `webconference-android`
    /// run: a real, substantive review genuinely cleared this risk, but new,
    /// real succeeded work landed right after it and was never itself reviewed --
    /// the old "has there EVER been a substantive review anywhere" check stayed
    /// silently satisfied forever regardless. This is what closes it: a review
    /// only counts if it's at or after the run's own MOST RECENT succeeded work.
    fn re_flags_when_real_new_work_lands_after_the_only_real_review() {
        let mut state = RunState::new("run-preflight");
        state.history.push(iteration(STAGE_IMPLEMENT, 1, "shipped a real feature", vec![]));
        state.history.push(iteration(STAGE_REVIEW, 2, "reviewed the diff line by line, confirmed the edge cases are covered and the naming is clear", vec![]));
        assert!(
            !preflight_annotations(&state).iter().any(|f| f.label == "no review stage for real, succeeded work"),
            "sanity check: the review above must genuinely clear the risk first"
        );

        state.history.push(iteration(STAGE_IMPLEMENT, 3, "shipped a second, later real feature, never reviewed", vec![]));
        let findings = preflight_annotations(&state);
        assert!(
            findings.iter().any(|f| f.label == "no review stage for real, succeeded work"),
            "real new work after the only review must re-flag this risk, not stay silently satisfied forever: {findings:?}"
        );
    }

    #[test]
    fn does_not_flag_a_run_whose_only_succeeded_iterations_are_review_itself() {
        let mut state = RunState::new("run-preflight");
        // A review-only history has no OTHER real work that would need reviewing --
        // must not self-referentially flag review as missing review.
        state.history.push(iteration(STAGE_REVIEW, 1, "reviewed the plan doc, it's clear and complete", vec![]));
        assert!(!preflight_annotations(&state).iter().any(|f| f.label == "no review stage for real, succeeded work"));
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
    /// Real gap closed (#382 goal doc §7, 2026-08-07): `no_price_ceiling` is the
    /// one risk kind with an unambiguous, always-safe fix (re-propose the
    /// identical role with a real price_ceiling this time) -- the GUI's own
    /// "Fix it" action needs the real `stage_id`/`tag` to pre-fill that
    /// re-proposal, not re-derived by parsing `evidence`'s human-readable text.
    fn no_price_ceiling_finding_carries_a_real_fix_target_every_other_check_leaves_none() {
        let mut state = RunState::new("run-preflight");
        state.approved_stage_proposals.push(proposal(None, None));
        state.added_stages.push("devsystem.some_new_role".into());
        // Also trigger a second, unrelated risk kind in the same run, to prove
        // fix_target isn't accidentally set on checks that never populate it.
        state.criteria.checkin_every = 0;
        let findings = preflight_annotations(&state);

        let price_ceiling_finding = findings.iter().find(|f| f.label == "no price ceiling set").expect("no price ceiling set must fire");
        let target = price_ceiling_finding.fix_target.as_ref().expect("no_price_ceiling must carry a real fix_target");
        assert_eq!(target.stage_id, "devsystem.some_new_role");
        assert_eq!(target.tag, "some_new_role");

        let checkin_finding = findings
            .iter()
            .find(|f| f.label == "mandatory check-in cadence effectively disabled")
            .expect("the unrelated checkin-cadence risk must also fire in this scenario");
        assert!(checkin_finding.fix_target.is_none(), "a risk kind with no real fix action must never carry a fabricated target");
    }

    #[test]
    /// Real gap, live-found 2026-08-06 checking the actual deployed
    /// webconference-android run: `devsystem.review` had the identical
    /// unbounded shape as `devsystem.document_extraction` (both live,
    /// use_existing_service: None, price_ceiling: None), but only the latter
    /// was ever surfaced -- `no_price_ceiling` used to be built on
    /// `Iterator::find`, which stops at the first match in `added_stages`
    /// order and silently never looks at the rest. Two roles proposed
    /// unbounded at once here must both be flagged, not just the first one
    /// added.
    fn flags_every_simultaneously_unbounded_role_not_just_the_first() {
        let mut state = RunState::new("run-preflight");
        let first = StageProposal {
            proposed_by: STAGE_REVIEW.into(),
            stage_id: "devsystem.role_a".into(),
            tag: "role_a".into(),
            rationale: "test".into(),
            use_existing_service: None,
            units: 1,
            price_ceiling: None,
        };
        let second = StageProposal {
            proposed_by: STAGE_REVIEW.into(),
            stage_id: "devsystem.role_b".into(),
            tag: "role_b".into(),
            rationale: "test".into(),
            use_existing_service: None,
            units: 1,
            price_ceiling: None,
        };
        state.approved_stage_proposals.push(first);
        state.approved_stage_proposals.push(second);
        state.added_stages.push("devsystem.role_a".into());
        state.added_stages.push("devsystem.role_b".into());
        let findings = preflight_annotations(&state);
        let unbounded: Vec<_> = findings.iter().filter(|f| f.label == "no price ceiling set").collect();
        assert_eq!(unbounded.len(), 2, "both real unbounded roles must be flagged, not just the first: {findings:?}");
        assert!(unbounded.iter().any(|f| f.evidence.contains("devsystem.role_a")));
        assert!(unbounded.iter().any(|f| f.evidence.contains("devsystem.role_b")));
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
                ..Default::default()
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
            created_by: None,
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

    #[test]
    /// Real gap, live-found 2026-08-06 applying the exact same lens that just
    /// found `no_price_ceiling`'s own "stops at the first match" bug: this
    /// check had the identical shape (an early `return Some(...)` in a nested
    /// loop). Two separate requirements, each with its own genuinely vague
    /// criterion, must both be flagged, not just the first one found.
    fn flags_every_genuinely_vague_criterion_not_just_the_first() {
        let mut state = RunState::new("run-preflight");
        state.requirements.push(requirement("WHEN x, THE SYSTEM SHALL y", vec!["works"]));
        state.requirements.push(requirement("WHEN a, THE SYSTEM SHALL b", vec!["is fast"]));
        let findings = preflight_annotations(&state);
        let vague: Vec<_> = findings.iter().filter(|f| f.label == "acceptance criteria too vague to be deterministic").collect();
        assert_eq!(vague.len(), 2, "both genuinely vague criteria must be flagged, not just the first: {findings:?}");
        assert!(vague.iter().any(|f| f.evidence.contains("requirement #0")));
        assert!(vague.iter().any(|f| f.evidence.contains("requirement #1")));
    }

    #[test]
    /// Defense-in-depth for the bidi-control-character class (#382 goal doc §8,
    /// 2026-08-06): every write-time gate this session added only guards new
    /// writes, so this is the retroactive safety net -- a human should still see
    /// it flagged even if the text predates the fix, or arrived through some
    /// path that forgot the check. Covers every field the write-time fixes do:
    /// requirement statement/criteria, milestones, backlog, custom-panel title,
    /// and stage-proposal rationale (both pending and approved).
    fn flags_a_stored_bidi_control_character_in_every_covered_field() {
        let bidi = "approved\u{202e} for production tset ton si sihT";
        let mut state = RunState::new("run-preflight");
        state.requirements.push(requirement(&format!("WHEN x, THE SYSTEM SHALL {bidi}"), vec!["a real criterion"]));
        state.requirements.push(requirement("WHEN y, THE SYSTEM SHALL z", vec![bidi]));
        state.milestones.push(crate::runner::Milestone { description: bidi.to_string(), achieved: false });
        state.backlog.push(crate::runner::BacklogItem { text: bidi.to_string(), done: false });
        state.custom_panels.push(crate::runner::CustomPanel {
            id: "panel-1".into(),
            title: bidi.to_string(),
            html: "<p>x</p>".into(),
            source: None,
            created_at: 0,
        });
        state.pending_stage_proposals.push(crate::runner::PendingStageProposal {
            id: "prop-1".into(),
            proposal: StageProposal {
                proposed_by: "devsystem.assistant".into(),
                stage_id: "devsystem.x".into(),
                tag: "x".into(),
                rationale: bidi.to_string(),
                use_existing_service: None,
                units: 1,
                price_ceiling: None,
            },
            proposed_at: 0,
        });
        state.approved_stage_proposals.push(StageProposal {
            proposed_by: "devsystem.plan".into(),
            stage_id: "devsystem.y".into(),
            tag: "y".into(),
            rationale: bidi.to_string(),
            use_existing_service: None,
            units: 1,
            price_ceiling: None,
        });

        let findings = preflight_annotations(&state);
        let bidi_findings: Vec<_> = findings.iter().filter(|f| f.label == "stored text contains a Unicode bidi control character").collect();
        assert_eq!(bidi_findings.len(), 7, "every one of the seven bidi-laced fields must be flagged, not just some: {bidi_findings:?}");
        assert!(bidi_findings.iter().any(|f| f.evidence.contains("statement")));
        assert!(bidi_findings.iter().any(|f| f.evidence.contains("acceptance criterion")));
        assert!(bidi_findings.iter().any(|f| f.evidence.contains("milestone")));
        assert!(bidi_findings.iter().any(|f| f.evidence.contains("backlog item")));
        assert!(bidi_findings.iter().any(|f| f.evidence.contains("custom panel")));
        assert!(bidi_findings.iter().any(|f| f.evidence.contains("pending stage proposal")));
        assert!(bidi_findings.iter().any(|f| f.evidence.contains("approved stage proposal")));
    }

    #[test]
    fn a_run_with_no_bidi_control_characters_anywhere_gets_no_such_finding() {
        let mut state = RunState::new("run-preflight");
        state.requirements.push(requirement("WHEN x, THE SYSTEM SHALL y", vec!["a real, checkable criterion"]));
        state.milestones.push(crate::runner::Milestone { description: "a real milestone".into(), achieved: false });
        let findings = preflight_annotations(&state);
        assert!(
            !findings.iter().any(|f| f.label == "stored text contains a Unicode bidi control character"),
            "clean text must never trigger this finding: {findings:?}"
        );
    }
}
