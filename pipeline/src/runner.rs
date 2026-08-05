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

/// One entry in a run's real backlog -- distinct from `history` (what already
/// happened) and stalled stages (proposed-but-unfilled roles): a plain "still needs
/// doing" list, operator feedback: "ich möchte die Liste der Taskliste... ein echtes
/// Backlog pro Run." Addressed by its index in `RunState::backlog`; checked off
/// rather than removed, so the record of what was planned survives.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BacklogItem {
    pub text: String,
    pub done: bool,
}

/// How one role's status is determined and (eventually) filled -- operator feedback
/// (#382 Roles panel ask 1/4): "Umschalten von Auktion zu einem dezidierten LLM
/// Agenten." `Auction` is today's only real behavior, unchanged: the role's status
/// comes from CADS-Tunnel's real crew auction (`GET /api/runs/{id}/auction`).
/// `Dedicated` is a devsystem-web-level bookkeeping concept, NOT a change to
/// `ct_common::pipeline::RequiredRole`/`convene()` themselves (those are CADS-Tunnel's
/// shared core primitives, used by every pipeline in this ecosystem -- extending them
/// for this one pipeline's GUI convenience would be a materially bigger, cross-repo
/// change than this ask needs). A `Dedicated` role's `label` is a plain human-chosen
/// identifier, not yet backed by a real reachability check the way
/// `devsystem.assistant`'s hardcoded probe is -- there is no general registry of
/// addressable LLM agents to check against yet (the real gap task #27/#29 already
/// found), so this deliberately doesn't fabricate one.
///
/// `accepted_bid` (operator ask: "in dem developer pipeline auch ohne Auktion
/// eines der angebotenen nutzen, in dem wir das Gebot ohne Auktion annehmen")
/// is `Some` when Dedicated was set by directly accepting one specific real
/// bid from the live auction view (its real `holder_label`/`price` snapshot
/// at accept time -- prices/bidders can change afterward, this is honestly a
/// point-in-time record of what was accepted, not a live-tracked one), and
/// `None` for a plain hand-typed label with no real bid behind it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RoleFillMode {
    Auction,
    Dedicated {
        label: String,
        #[serde(default)]
        accepted_bid: Option<AcceptedBid>,
    },
}

/// The real, point-in-time snapshot of a bid a human directly accepted --
/// see [`RoleFillMode::Dedicated`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptedBid {
    /// The auction view's own short display label for the bidder (`RoleBidView.who`,
    /// derived from their real pubkey) -- not the raw pubkey itself, which the
    /// auction view never exposes.
    pub holder_label: String,
    pub price: u64,
}

/// A real, human-added GUI panel beyond the core set the pipeline itself ships --
/// operator ask: extend the GUI with custom panels, addable/removable, with a
/// future path to a public marketplace repo. `html` is rendered inside a
/// `<iframe sandbox="allow-scripts">` in the GUI, not injected into the main
/// page -- a custom panel (hand-written, or later assistant-drafted/marketplace-
/// installed) never gets the main page's session, cookies, or DOM access this
/// way, a real trust-boundary decision, not an implementation detail (confirmed
/// with the operator before building this). `source` is `None` for a
/// hand-written panel and would carry the marketplace URL for an installed one,
/// once that increment exists -- not fabricated here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomPanel {
    pub id: String,
    pub title: String,
    pub html: String,
    pub source: Option<String>,
    pub created_at: u64,
}

/// A real completion/abort checkpoint, distinct from [`BacklogItem`] (informational
/// todo) and `AbortCriteria` (mechanical iteration/failure counts): operator
/// feedback: "ich möchte nicht nur Iterationen, sondern auch Milestones als
/// Abbruchkriterium definieren können." Reaching one is meaningful enough that
/// `toggle_milestone` (the 0->1 transition only) auto-pauses the run -- the same
/// `RunState::paused` mechanism a human uses to stop and correct something -- so a
/// milestone actually gates the run rather than being decorative.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Milestone {
    pub description: String,
    pub achieved: bool,
}

/// A real, structured requirement -- the concrete first slice of "requirement
/// management" the operator asked to research (2026-08-04). Modeled on the
/// EARS notation (Easy Approach to Requirements Syntax -- the industry-standard
/// way to keep a requirement unambiguous and testable for an LLM coding agent,
/// e.g. "WHEN a user sends a text message over an established channel, THE
/// SYSTEM SHALL persist it locally before confirming delivery to the UI"),
/// distinct from a [`Milestone`] (a checkpoint) or a [`BacklogItem`] (a task):
/// a requirement states an intended system behavior with concrete,
/// checkable `acceptance_criteria`, giving devsystem.assistant and any stage
/// something real to verify against instead of a vague wish.
///
/// `verified_criteria` (2026-08-05 follow-up) tracks per-criterion progress --
/// deliberately a SEPARATE, purely additive field rather than changing
/// `acceptance_criteria`'s own type (which would break every already-persisted
/// `state.json` that has it as a plain `Vec<String>`): `#[serde(default)]` so
/// every pre-existing file still loads (as if nothing were checked yet), and
/// `toggle_acceptance_criterion` grows it with real `false` padding on demand
/// rather than requiring it to already be the same length as
/// `acceptance_criteria`. Deliberately NOT auto-derived from/into `verified`
/// (the whole-requirement flag) in this slice -- a human confirming "yes,
/// this whole requirement is done" and a human ticking off individual
/// criteria are kept as two independent, explicit signals rather than
/// silently coupled, since coupling them is a real design choice a human
/// should make, not one to guess at here. This is deliberately still 100%
/// human-driven (no LLM judgment of whether a criterion is actually met) --
/// automatic acceptance-criteria checking by a verify stage remains a real,
/// separate, still-open question.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Requirement {
    pub statement: String,
    pub acceptance_criteria: Vec<String>,
    pub verified: bool,
    #[serde(default)]
    pub verified_criteria: Vec<bool>,
}

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
    /// This run's real backlog -- see [`BacklogItem`]. `#[serde(default)]` so
    /// pre-existing `state.json` files (no backlog yet) still load.
    #[serde(default)]
    pub backlog: Vec<BacklogItem>,
    /// This run's real completion/abort checkpoints -- see [`Milestone`].
    /// `#[serde(default)]` so pre-existing `state.json` files still load.
    #[serde(default)]
    pub milestones: Vec<Milestone>,
    /// The real target repository this run is actually building, if the human has
    /// told the pipeline -- operator feedback: "ich möchte Zugang zu aktuellem
    /// Code." Nothing else in this crate infers or hardcodes a repo per run (the
    /// whole point of #382: the pipeline mechanism stays project-agnostic); this
    /// is the one place a human states it, and only the GUI (client-side, against
    /// the real GitHub API) uses it, never devsystem-web itself guessing at
    /// URLs. `#[serde(default)]` so pre-existing `state.json` files still load.
    #[serde(default)]
    pub repo_url: Option<String>,
    /// The real, verified identity (Caddy's `forward_auth` `X-Gate-Email`, the exact
    /// header [`whoami`](../../web/src/main.rs)'s `/api/me` reports) of whoever was
    /// signed in when this run was created -- #382's "correct identification" gap:
    /// today's site-wide login gate has no per-run access control, so this is
    /// deliberately just a real, honest *label* ("who created this"), not
    /// enforcement -- `None` when the run was created without the gate header
    /// present (e.g. a direct API call, a pre-gate run). `#[serde(default)]` so
    /// pre-existing `state.json` files (no owner recorded) still load.
    #[serde(default)]
    pub owner_email: Option<String>,
    /// Per-role tag -> [`RoleFillMode`] override. A tag absent from this map means
    /// `Auction` (today's only behavior) -- so every pre-existing `state.json` loads
    /// with every role still auction-filled, unchanged. `#[serde(default)]` for the
    /// same reason.
    #[serde(default)]
    pub role_fill_modes: std::collections::HashMap<String, RoleFillMode>,
    /// Real, human-added GUI panels beyond the core set -- see [`CustomPanel`].
    /// `#[serde(default)]` so pre-existing `state.json` files (none added yet)
    /// still load.
    #[serde(default)]
    pub custom_panels: Vec<CustomPanel>,
    /// A custom panel the assistant has proposed but a human hasn't approved yet --
    /// the operator's own trust-model decision, confirmed before `custom_panels`
    /// slice 1 was even built: the assistant stays advice-only for anything that
    /// renders into the GUI ("proposes, human clicks install"), unlike milestones/
    /// backlog which it can act on directly. Approving moves it into
    /// `custom_panels` for real; rejecting just drops it. `#[serde(default)]` so
    /// pre-existing `state.json` files (none proposed yet) still load.
    #[serde(default)]
    pub pending_panel_proposals: Vec<PendingPanelProposal>,
    /// A new pipeline stage/role the assistant has proposed but a human hasn't
    /// approved yet -- same trust-model pattern as `pending_panel_proposals`, applied
    /// to the OTHER thing that renders into the live system: a real role real
    /// role-fillers can auction/bid against. A real role-filler's own mid-iteration
    /// `StageProposal` (attached to an `IterationRecord`) stays a completely separate
    /// path (`run_iteration` applies those immediately, unchanged) -- this field is
    /// specifically for the advisory chat assistant's speculative suggestions, which
    /// get the same "propose, human approves" gate custom panels do.
    /// `#[serde(default)]` so pre-existing `state.json` files (none proposed yet)
    /// still load.
    #[serde(default)]
    pub pending_stage_proposals: Vec<PendingStageProposal>,
    /// A real GitHub issue draft the assistant proposed after noticing a gap or
    /// error -- "self-healing" (operator ask, 2026-08-04): the assistant
    /// recognizes something's missing/broken and drafts a real issue for a
    /// target repo (e.g. CADS-webconference-demo), but NEVER posts it itself.
    /// Same trust-model pattern as `pending_panel_proposals`/
    /// `pending_stage_proposals` -- a human reviews and explicitly approves
    /// before anything reaches GitHub. `#[serde(default)]` so pre-existing
    /// `state.json` files (none proposed yet) still load.
    #[serde(default)]
    pub pending_issue_proposals: Vec<PendingIssueProposal>,
    /// This run's real, structured requirements -- see [`Requirement`].
    /// `#[serde(default)]` so pre-existing `state.json` files (none defined
    /// yet) still load.
    #[serde(default)]
    pub requirements: Vec<Requirement>,
}

/// See [`RunState::pending_stage_proposals`]'s doc comment for why this wraps
/// [`crate::StageProposal`] rather than being applied directly like a real
/// role-filler's own iteration-time proposal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingStageProposal {
    pub id: String,
    pub proposal: crate::StageProposal,
    pub proposed_at: u64,
}

/// See [`RunState::pending_issue_proposals`]'s doc comment. `repo` is
/// `owner/name` (e.g. `"scimbe/CADS-webconference-demo"`), not a full URL --
/// kept minimal, no free-form target the assistant could otherwise be tricked
/// or drift into pointing at an unrelated repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingIssueProposal {
    pub id: String,
    pub repo: String,
    pub title: String,
    pub body: String,
    pub proposed_at: u64,
}

/// See [`RunState::pending_panel_proposals`]'s doc comment for why this is a
/// separate, non-live shape from [`CustomPanel`] rather than just adding an
/// `approved: bool` flag to it -- a pending proposal never renders in the GUI at
/// all until a human approves it, so it deliberately can't be confused with a
/// live one at the type level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingPanelProposal {
    pub id: String,
    pub title: String,
    pub html: String,
    pub proposed_at: u64,
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
            backlog: Vec::new(),
            milestones: Vec::new(),
            repo_url: None,
            owner_email: None,
            role_fill_modes: std::collections::HashMap::new(),
            custom_panels: Vec::new(),
            pending_panel_proposals: Vec::new(),
            pending_stage_proposals: Vec::new(),
            pending_issue_proposals: Vec::new(),
            requirements: Vec::new(),
        }
    }
}

/// Toggle the milestone at `index`. The not-achieved -> achieved transition
/// auto-pauses the run via the same [`RunState::paused`] a human uses to stop and
/// correct something -- reaching a real milestone is a checkpoint, not decoration.
/// The achieved -> not-achieved direction (a human undoing a mistaken mark) does
/// NOT auto-unpause; resuming is always a separate, deliberate action.
pub fn toggle_milestone(state: &mut RunState, index: usize) -> Result<(), String> {
    let milestone = state.milestones.get_mut(index).ok_or_else(|| format!("no milestone at index {index}"))?;
    let was_achieved = milestone.achieved;
    milestone.achieved = !milestone.achieved;
    if !was_achieved && milestone.achieved {
        state.paused = true;
    }
    Ok(())
}

/// Unlike [`toggle_milestone`], toggling a requirement's `verified` flag never
/// auto-pauses the run -- a requirement is a standing behavioral contract, not
/// a one-time checkpoint; marking it verified (or un-verifying it after a
/// regression) is routine bookkeeping, not an event that should interrupt the
/// loop.
pub fn toggle_requirement(state: &mut RunState, index: usize) -> Result<(), String> {
    let requirement = state.requirements.get_mut(index).ok_or_else(|| format!("no requirement at index {index}"))?;
    requirement.verified = !requirement.verified;
    Ok(())
}

/// Toggles a single acceptance criterion's real, human-set verified state --
/// see [`Requirement::verified_criteria`]'s own doc comment for why this is a
/// separate signal from `verified` itself. Grows `verified_criteria` with
/// real `false` entries up to `criterion_index` on demand rather than
/// requiring it to already be the same length as `acceptance_criteria` --
/// every pre-existing requirement (persisted before this field existed, or
/// simply never touched yet) starts effectively "nothing checked" without
/// needing a migration step.
pub fn toggle_acceptance_criterion(state: &mut RunState, req_index: usize, criterion_index: usize) -> Result<(), String> {
    let requirement = state.requirements.get_mut(req_index).ok_or_else(|| format!("no requirement at index {req_index}"))?;
    if criterion_index >= requirement.acceptance_criteria.len() {
        return Err(format!("requirement {req_index} has no acceptance criterion at index {criterion_index}"));
    }
    if requirement.verified_criteria.len() <= criterion_index {
        requirement.verified_criteria.resize(criterion_index + 1, false);
    }
    requirement.verified_criteria[criterion_index] = !requirement.verified_criteria[criterion_index];
    Ok(())
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
            requirement_indices: Vec::new(),
        }
    }

    #[test]
    fn achieving_a_milestone_auto_pauses_the_run() {
        let mut state = RunState::new("run-milestone");
        state.milestones.push(Milestone { description: "APK builds and installs".into(), achieved: false });
        assert!(!state.paused);

        toggle_milestone(&mut state, 0).unwrap();
        assert!(state.milestones[0].achieved);
        assert!(state.paused, "reaching a milestone must pause the run for human review");
    }

    #[test]
    fn un_achieving_a_milestone_does_not_auto_unpause() {
        let mut state = RunState::new("run-milestone-undo");
        state.milestones.push(Milestone { description: "real APK build".into(), achieved: true });
        state.paused = true;

        toggle_milestone(&mut state, 0).unwrap();
        assert!(!state.milestones[0].achieved);
        assert!(state.paused, "undoing a mistaken mark should not silently resume the run");
    }

    #[test]
    fn toggling_an_out_of_range_milestone_index_fails_loudly() {
        let mut state = RunState::new("run-milestone-oob");
        assert!(toggle_milestone(&mut state, 0).is_err());
    }

    #[test]
    fn toggling_a_requirement_flips_verified_and_never_auto_pauses() {
        let mut state = RunState::new("run-req");
        state.requirements.push(Requirement {
            statement: "WHEN a user sends a text message over an established channel, THE SYSTEM SHALL persist it locally before confirming delivery to the UI".into(),
            acceptance_criteria: vec!["message survives an app restart".into(), "UI shows \"sent\" only after local persistence succeeds".into()],
            verified: false,
            verified_criteria: Vec::new(),
        });
        toggle_requirement(&mut state, 0).unwrap();
        assert!(state.requirements[0].verified);
        assert!(!state.paused, "unlike a milestone, verifying a requirement must not auto-pause the run");

        toggle_requirement(&mut state, 0).unwrap();
        assert!(!state.requirements[0].verified);
    }

    #[test]
    fn toggling_an_out_of_range_requirement_index_fails_loudly() {
        let mut state = RunState::new("run-req-oob");
        assert!(toggle_requirement(&mut state, 0).is_err());
    }

    #[test]
    fn toggling_an_acceptance_criterion_grows_verified_criteria_on_demand() {
        let mut state = RunState::new("run-criteria");
        state.requirements.push(Requirement {
            statement: "WHEN ..., THE SYSTEM SHALL ...".into(),
            acceptance_criteria: vec!["criterion A".into(), "criterion B".into(), "criterion C".into()],
            verified: false,
            verified_criteria: Vec::new(),
        });

        // A pre-existing requirement (persisted before this field existed, or
        // simply never touched) starts with an empty verified_criteria --
        // toggling criterion 2 must grow it with real false padding for 0/1,
        // not panic or silently no-op.
        toggle_acceptance_criterion(&mut state, 0, 2).unwrap();
        assert_eq!(state.requirements[0].verified_criteria, vec![false, false, true]);

        toggle_acceptance_criterion(&mut state, 0, 0).unwrap();
        assert_eq!(state.requirements[0].verified_criteria, vec![true, false, true]);

        // Un-toggling flips it back, doesn't just grow forever.
        toggle_acceptance_criterion(&mut state, 0, 2).unwrap();
        assert_eq!(state.requirements[0].verified_criteria, vec![true, false, false]);

        assert!(!state.requirements[0].verified, "toggling individual criteria must never silently flip the independent whole-requirement verified flag");
    }

    #[test]
    fn toggling_an_out_of_range_criterion_index_fails_loudly() {
        let mut state = RunState::new("run-criteria-oob");
        state.requirements.push(Requirement {
            statement: "WHEN ..., THE SYSTEM SHALL ...".into(),
            acceptance_criteria: vec!["only one criterion".into()],
            verified: false,
            verified_criteria: Vec::new(),
        });
        assert!(toggle_acceptance_criterion(&mut state, 0, 1).is_err());
        assert!(toggle_acceptance_criterion(&mut state, 5, 0).is_err(), "an out-of-range requirement index must also fail loudly");
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
