//! The Development System's pipeline spec (#382): `plan -> test -> implement -> review ->
//! verify -> remember -> improve`, each stage a [`RequiredRole`] whose `service` is a
//! [`ServiceType::Custom`] name -- no CADS-Tunnel core change needed per new stage type,
//! the whole point of the `ServiceType::Custom` generalization this repo builds on.
//!
//! This crate is deliberately small: it defines the spec and proves (via `convene()`,
//! tested below with real signed offers, not just declared types) that CADS-Tunnel's
//! existing crew-auction primitive -- built for the flappy-demo crew (#171) -- genuinely
//! works unmodified for a completely different pipeline's roles. Discovery
//! (`/registry/agents`), channel wiring (Agent-Fabric), and escrow settlement are all
//! reused as-is from CADS-Tunnel/ct-agent; nothing about agent-to-agent plumbing is
//! reinvented here (see the coordination repo's README for the full picture).

use ct_common::channel::ServiceType;
use ct_common::pipeline::{PipelineSpec, RequiredRole, SelectionPolicy};

pub mod checkin;
pub mod envelope;
pub mod improve;
pub mod preflight;
pub mod runner;

/// Real gap found live by the incompetent-agent stress test (#382 goal doc §8,
/// 2026-08-06): both real CLI binaries that persist a real ed25519 signing key
/// to disk (`devsystem_offer`'s `signing_key_from_file`, `devsystem_assistant`'s
/// `assistant_signing_key`) wrote it with a plain `fs::write`, which on Unix
/// lands at whatever the process's own umask allows -- confirmed live against
/// the actual deployed `devsystem_assistant` key file: real mode `664`,
/// world-readable. Private key material world-readable on a host that could
/// ever run another process under a different user (or, longer term, a
/// compromised unrelated process reading arbitrary files) lets that reader
/// impersonate this identity in the real crew-auction -- sign fraudulent
/// offers as if from the legitimate role-filler. Shared here so both real key-
/// writing call sites use the identical, correct fix instead of one getting it
/// and the other not (the same "two entry points, one bug class" lesson
/// already learned once this session for path validation). Restricts to
/// owner-only read/write (`0600`) immediately after writing -- the key's own
/// bytes are unchanged, only the file's permissions.
pub fn write_signing_key_restricted(path: &str, key_bytes: &[u8; 32]) -> std::io::Result<()> {
    std::fs::write(path, key_bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// The seven pipeline-stage service names (#382 §3), each a `ServiceType::Custom` --
/// a pipeline-designer-level decision, not a CADS-Tunnel core one.
pub const STAGE_PLAN: &str = "devsystem.plan";
pub const STAGE_TEST: &str = "devsystem.test";
pub const STAGE_IMPLEMENT: &str = "devsystem.implement";
pub const STAGE_REVIEW: &str = "devsystem.review";
pub const STAGE_VERIFY: &str = "devsystem.verify";
pub const STAGE_REMEMBER: &str = "devsystem.remember";
pub const STAGE_IMPROVE: &str = "devsystem.improve";

/// All seven stage names, in pipeline order.
pub const ALL_STAGES: [&str; 7] =
    [STAGE_PLAN, STAGE_TEST, STAGE_IMPLEMENT, STAGE_REVIEW, STAGE_VERIFY, STAGE_REMEMBER, STAGE_IMPROVE];

/// The real, current answer to "what tools does `devsystem.assistant` have" (#382
/// task: a GUI tool registry for the assistant role): none beyond its default
/// read-only grounding -- `devsystem_assistant`'s `ask_llm` passes this exact list
/// to `claude -p --disallowedTools`. Shared as one constant (not duplicated as a
/// string literal in both the assistant binary and the web API) so the two can
/// never drift apart and silently start lying to each other. There is no
/// ct-agent-connected tool registry to report on yet -- the assistant is
/// deliberately advice-only, grounded via fetched run state, never given
/// filesystem/shell/network access of its own.
pub const ASSISTANT_DISALLOWED_TOOLS: [&str; 6] = ["Edit", "Write", "Bash", "WebFetch", "WebSearch", "Agent"];

/// Build the real [`PipelineSpec`] for one pipeline run, keyed by `run_id` (the
/// coordination repo's convention: one GitHub Issue per run, `run_id` matching the
/// issue number/slug). `operator_pubkey_hex` is the Agent-Fabric channel operator key
/// governing this run's role channels -- `None` while a run has no channels wired yet
/// (the #382 first slice's scope: the `plan` stage only, see [`plan_only_spec`]).
pub fn full_spec(run_id: &str, operator_pubkey_hex: Option<String>) -> PipelineSpec {
    PipelineSpec {
        id: format!("devsystem-{run_id}"),
        roles: ALL_STAGES
            .iter()
            .map(|stage| RequiredRole {
                service: ServiceType::Custom((*stage).to_string()),
                units: 1,
                tag: stage.strip_prefix("devsystem.").unwrap_or(stage).to_string(),
                selection_policy: None,
            })
            .collect(),
        operator_pubkey_hex,
        selection_policy: SelectionPolicy::LowestFloor,
    }
}

/// The #382 first-slice spec: **only** the `plan` role, matching the committed
/// sequencing ("stand up the coordination repo + generalize RequiredRole/convene() ...
/// + plan/Plan-Canvas stage only, before committing to the full seven-stage build").
///
/// The other six stages exist in [`full_spec`] but are not wired into any real run yet.
pub fn plan_only_spec(run_id: &str, operator_pubkey_hex: Option<String>) -> PipelineSpec {
    PipelineSpec {
        id: format!("devsystem-{run_id}"),
        roles: vec![RequiredRole {
            service: ServiceType::Custom(STAGE_PLAN.to_string()),
            units: 1,
            tag: "plan".to_string(),
            selection_policy: None,
        }],
        operator_pubkey_hex,
        selection_policy: SelectionPolicy::LowestFloor,
    }
}

/// A real proposal a role-filler agent emits mid-iteration when it discovers this run
/// needs a stage/service the current [`PipelineSpec`] doesn't have yet -- e.g. "we need
/// an Android emulator to test the next slice against". This is the actual mechanism
/// behind the self-optimizing design (#382): the pipeline is not fixed at `full_spec()`,
/// it grows via proposals like this one, applied to the *live* spec by [`apply_proposal`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StageProposal {
    /// Which role/agent raised this (e.g. `"devsystem.implement"`, matching a stage
    /// tag) -- not a human identity, an accountability trail for which stage's
    /// role-filler asked for the new capability.
    pub proposed_by: String,
    /// The new stage's service name, e.g. `"devsystem.android_emulator_test"`. Always
    /// namespaced `devsystem.*` by convention (not enforced -- a pipeline designer
    /// could propose a bare custom name too).
    pub stage_id: String,
    /// Short tag for the new [`RequiredRole`] (mirrors the existing seven stages'
    /// `tag` convention: the `devsystem.` prefix stripped).
    pub tag: String,
    /// Why this stage is needed -- the actual content a human checks during a
    /// periodic ecc-plan-canvas check-in, not a machine-only field.
    pub rationale: String,
    /// If set, names an existing running service that can fill this role today (no
    /// new service needs to be built) -- otherwise the proposal implies "build one".
    pub use_existing_service: Option<String>,
    /// Auction seats needed for this role. Defaults to 1 in practice; kept explicit
    /// since a stage might need more than one filler (e.g. two review agents).
    pub units: u64,
    /// Maximum price this role's `CapacityOffer` may clear at, if the proposer set
    /// one. `None` means unbounded -- a real risk when a role could be filled by an
    /// external paid partner (proposal §5's own example), which `preflight`'s
    /// `no_price_ceiling` check flags. `#[serde(default)]` so already-committed
    /// proposals (recorded before this field existed) still deserialize.
    #[serde(default)]
    pub price_ceiling: Option<u64>,
}

/// What happened when a [`StageProposal`] was applied to a live [`PipelineSpec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposalOutcome {
    /// A new [`RequiredRole`] was appended to the spec.
    Added,
    /// The spec already declared a role for this `stage_id` -- applying a proposal is
    /// idempotent, never creates a duplicate role for the same service.
    AlreadyPresent,
}

/// Apply a [`StageProposal`] to a **live** [`PipelineSpec`], mutating it in place. This
/// is the literal mechanism by which "the pipeline builds itself" per the operator's
/// framing: a role-filler's feedback becomes a new auction-able role in the same spec
/// future iterations convene against, with no CADS-Tunnel core change required (still
/// just a new `ServiceType::Custom` name).
pub fn apply_proposal(spec: &mut PipelineSpec, proposal: &StageProposal) -> ProposalOutcome {
    let service = ServiceType::Custom(proposal.stage_id.clone());
    if spec.roles.iter().any(|r| r.service == service) {
        return ProposalOutcome::AlreadyPresent;
    }
    spec.roles.push(RequiredRole {
        service,
        units: proposal.units,
        tag: proposal.tag.clone(),
        selection_policy: None,
    });
    ProposalOutcome::Added
}

/// Reject a batch of [`StageProposal`]s that would corrupt a spec if [`apply_proposal`]
/// ever ran on them -- an empty `stage_id`, `tag`, or `rationale` (after trimming).
/// `apply_proposal` itself stays permissive-by-design (it's meant to be called on
/// already-trusted data) so this exists as a real, callable gate any *entry point* can
/// run first, and every entry point must actually call it -- see the incompetent-agent
/// stress test's eleventh real run (#382 goal doc §8, 2026-08-06). The bug this closes
/// wasn't the check missing once, it was the check living in exactly one of two real
/// entry points (`POST /api/runs/{id}/iterate` in `devsystem-web`) while the other
/// (`devsystem_iterate <run_id> <record.json>`'s local, non-`--remote` mode, which
/// calls `run_iteration`/`apply_proposal` directly against `runs/<run_id>/` with no
/// HTTP layer in between at all) still had none -- confirmed live: the exact same
/// garbage proposal that a fixed `devsystem-web` correctly rejects with a real `400`
/// still sailed straight through the local CLI and permanently added a
/// `ServiceType::Custom("")` role to the run's real spec. Pulling the check in here,
/// where both entry points can share the identical logic, is the actual fix -- not
/// re-duplicating the same `if` in a second place, which is exactly how it went missing
/// from the second place the first time.
/// Real gap found live 2026-08-06 (stress-test run 55), same "no upper bound" shape
/// `MAX_ABORT_CRITERIA_VALUE` (`web/src/main.rs`) already closed for a different
/// field: `units` (how many real bidders a role needs) was checked for `== 0` at
/// `propose_stage`/`quick_submit_offer` but never for an upper bound anywhere,
/// including here -- and an embedded proposal reaching `validate_proposals` applies
/// *immediately* to the live spec, no human review gate at all, making this the
/// more consequential of the three real entry points, not less. Live-confirmed
/// before this fix: an embedded proposal with `units: 0` got a real `200` and was
/// genuinely added to the live spec. A hundred is deliberately generous -- no real
/// role in this project has ever needed more than a handful of simultaneous
/// bidders -- not a tight arbitrary limit. Single source of truth for all three real
/// entry points (`propose_stage`, `quick_submit_offer`, and here), not three
/// separately-maintained copies.
pub const MAX_ROLE_UNITS: u64 = 100;

/// DAU-lens gap found live 2026-08-06 (#382 goal doc §8), same lens and same shape as
/// the `add_requirement` acceptance-criteria fix (`web/src/main.rs`): each of these
/// two checks used to `find` and reject on the FIRST bad proposal in the batch only.
/// A real, live-confirmed case: an iteration submitting two simultaneously-bad
/// embedded proposals (one with an empty `stage_id`, one with `units: 0`) got told
/// about only the first -- the second stayed completely invisible until a resubmit.
/// Every real caller passes a genuine batch (`body.proposals`/`record.proposals`, not
/// a single-element convenience wrapper), so this is a real, not hypothetical, case.
/// Now reports every bad proposal in the one batch that has them, not one
/// retry-and-learn cycle per additional mistake.
pub fn validate_proposals(proposals: &[StageProposal]) -> Result<(), String> {
    let bad: Vec<String> = proposals
        .iter()
        .filter_map(|p| {
            if p.stage_id.trim().is_empty() || p.tag.trim().is_empty() || p.rationale.trim().is_empty() {
                Some(format!("proposal for stage_id {:?} needs a non-empty stage_id, tag, and rationale", p.stage_id))
            } else if p.units == 0 || p.units > MAX_ROLE_UNITS {
                Some(format!("proposal for stage_id {:?} needs units between 1 and {MAX_ROLE_UNITS}, got {}", p.stage_id, p.units))
            } else {
                None
            }
        })
        .collect();
    if bad.is_empty() { Ok(()) } else { Err(bad.join("; ")) }
}

/// Real gap found live 2026-08-06 (stress-test run 45): every other real free-text
/// field in this codebase (milestones, backlog items, requirement statements, stage
/// proposals via [`validate_proposals`] above) already rejects whitespace-only
/// content -- an iteration's own `feedback` was the one exception, confirmed live at
/// the HTTP entry point. Same "two real entry points, one bug class" shape already
/// found this session for `validate_proposals` itself: `devsystem_iterate`'s local,
/// non-`--remote` CLI path calls [`run_iteration`] directly with no HTTP layer at
/// all, so a check added only in `web/src/main.rs` would leave that path unprotected.
/// A shared, standalone function (not folded into `run_iteration` itself, matching
/// `validate_proposals`'s own precedent of validating BEFORE constructing the real
/// `IterationRecord`) so every real entry point calls the identical gate.
pub fn validate_feedback(feedback: &str) -> Result<(), String> {
    if feedback.trim().is_empty() {
        return Err("feedback must not be empty".to_string());
    }
    Ok(())
}

/// Explicit, bounded termination criteria for one run's "super loop" (#382 §"Abbruch
/// kriterien"): the pipeline's own self-optimization is iterative, not unsupervised
/// forever -- these numbers are what make it a *bounded* loop.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct AbortCriteria {
    /// Hard ceiling on iterations for this run, regardless of progress.
    pub max_iterations: u32,
    /// Consecutive failed iterations (a role-filler reporting `succeeded: false`)
    /// before the run aborts rather than keeps retrying.
    pub max_consecutive_failures: u32,
    /// A mandatory human check-in (via ecc-plan-canvas) fires at least this often,
    /// even when every iteration is succeeding -- "regelmässiger Austausch mit dem
    /// Owner", not just on failure.
    pub checkin_every: u32,
}

impl Default for AbortCriteria {
    /// Conservative defaults for a brand-new run: short leash until a human has seen
    /// at least one real check-in.
    fn default() -> Self {
        AbortCriteria { max_iterations: 20, max_consecutive_failures: 3, checkin_every: 5 }
    }
}

/// One role-filler's real output for one iteration of a stage -- the unit the super
/// loop's abort/check-in logic below actually operates on.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IterationRecord {
    pub run_id: String,
    pub stage: String,
    pub iteration: u32,
    pub feedback: String,
    pub proposals: Vec<StageProposal>,
    pub succeeded: bool,
    /// Real requirement traceability (2026-08-04 operator ask, first slice shipped
    /// as `runner::Requirement` -- this is the deferred follow-up): indices into
    /// `RunState::requirements` this iteration claims to actually address. A
    /// role-filler's own self-reported assertion, not automatically verified --
    /// same honesty model as `feedback` itself. `#[serde(default)]` so every
    /// pre-existing `IterationRecord`/`state.json` history entry (none claimed
    /// any yet) still deserializes.
    #[serde(default)]
    pub requirement_indices: Vec<usize>,
}

/// True when this iteration must pause for a human check-in before continuing --
/// either the configured cadence was hit, or the run has reached its iteration
/// ceiling (the ceiling always forces a check-in, even off-cadence, so a run can never
/// silently run past `max_iterations` without a human seeing it).
pub fn should_checkin(record: &IterationRecord, criteria: &AbortCriteria) -> bool {
    if record.iteration == 0 || criteria.checkin_every == 0 {
        return record.iteration >= criteria.max_iterations;
    }
    record.iteration.is_multiple_of(criteria.checkin_every) || record.iteration >= criteria.max_iterations
}

/// True when the run should abort outright (not just pause for check-in): too many
/// consecutive failures, or the hard iteration ceiling was passed.
pub fn should_abort(consecutive_failures: u32, current_iteration: u32, criteria: &AbortCriteria) -> bool {
    consecutive_failures >= criteria.max_consecutive_failures || current_iteration >= criteria.max_iterations
}

#[cfg(test)]
mod tests {
    use super::*;
    use ct_common::channel::CapacityKind;
    use ct_common::pipeline::PipelineError;
    use ed25519_dalek::SigningKey;

    fn offer(seed: u8, services: Vec<ServiceType>, price: u64) -> ct_common::channel::CapacityOffer {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        ct_common::channel::CapacityOffer::sign_new_with_services(
            &sk,
            CapacityKind::CloudApiQuota,
            vec!["claude".into()],
            1,
            price,
            "usd".into(),
            0,
            1_000_000,
            services,
        )
    }

    fn holder(seed: u8) -> [u8; 32] {
        SigningKey::from_bytes(&[seed; 32]).verifying_key().to_bytes()
    }

    #[test]
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06): confirmed directly against the actual deployed
    /// devsystem_assistant key file (real mode 664, world-readable) before this
    /// fix existed.
    fn write_signing_key_restricted_writes_the_real_bytes_and_locks_permissions_to_owner_only() {
        let dir = std::env::temp_dir().join(format!("devsystem-key-perm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.key");
        let path_str = path.to_str().unwrap();
        let key_bytes = [7u8; 32];

        write_signing_key_restricted(path_str, &key_bytes).expect("write must succeed");

        let read_back = std::fs::read(&path).unwrap();
        assert_eq!(read_back, key_bytes, "the real key bytes must round-trip unchanged -- only permissions change");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the real on-disk file must be owner-read/write only, not world-readable");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn full_spec_declares_all_seven_stages_as_distinct_custom_services() {
        let spec = full_spec("run-1", None);
        assert_eq!(spec.roles.len(), 7);
        let services: Vec<_> = spec.roles.iter().map(|r| r.service.clone()).collect();
        for (i, a) in services.iter().enumerate() {
            for (j, b) in services.iter().enumerate() {
                assert!(i == j || a != b, "every stage is its own distinct ServiceType::Custom -- no accidental aliasing");
            }
        }
        assert_eq!(spec.roles[0].tag, "plan");
        assert_eq!(spec.roles[6].tag, "improve");
    }

    #[test]
    fn plan_only_spec_convenes_for_real_when_a_plan_role_filler_is_online() {
        // The actual proof this crate exists to make: CADS-Tunnel's convene() -- built
        // for the flappy-demo crew, never touched for this -- genuinely clears a real
        // auction for a devsystem-declared, non-demo role, with a real signed offer.
        let spec = plan_only_spec("test-run", None);
        let plan_filler = offer(1, vec![ServiceType::Custom(STAGE_PLAN.to_string())], 10);

        let assignments = spec
            .convene(&[plan_filler], 100)
            .expect("a real, valid, online offer for the plan role convenes");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].service, ServiceType::Custom(STAGE_PLAN.to_string()));
        assert_eq!(assignments[0].provider, holder(1));
        assert_eq!(assignments[0].price, 10);
    }

    #[test]
    fn plan_only_spec_fails_closed_with_no_role_filler_online() {
        // The protocol's own promise (#382's opening quote): "if not enough agents are
        // online for an auction, the protocol raises an error" -- never a silent partial
        // run. Proven here with zero offers at all.
        let spec = plan_only_spec("test-run", None);
        assert_eq!(
            spec.convene(&[], 100),
            Err(PipelineError::UnfilledRole { service: ServiceType::Custom(STAGE_PLAN.to_string()) })
        );
    }

    #[test]
    fn apply_proposal_adds_a_new_role_a_role_filler_can_actually_convene_for() {
        // The literal proof of the self-optimizing design: a role-filler's proposal
        // (e.g. "we need an Android emulator to test") becomes a real, auction-able
        // role in the live spec -- not a declared-but-inert stage.
        let mut spec = plan_only_spec("run-2", None);
        assert_eq!(spec.roles.len(), 1);

        let proposal = StageProposal {
            proposed_by: STAGE_IMPLEMENT.to_string(),
            stage_id: "devsystem.android_emulator_test".to_string(),
            tag: "android_emulator_test".to_string(),
            rationale: "the webconference-android slice needs a real emulator run before verify can pass".to_string(),
            use_existing_service: None,
            units: 1,
            price_ceiling: None,
        };
        assert_eq!(apply_proposal(&mut spec, &proposal), ProposalOutcome::Added);
        assert_eq!(spec.roles.len(), 2);

        let emulator_filler = offer(
            3,
            vec![ServiceType::Custom("devsystem.android_emulator_test".to_string())],
            7,
        );
        let plan_filler = offer(1, vec![ServiceType::Custom(STAGE_PLAN.to_string())], 10);
        let assignments = spec
            .convene(&[plan_filler, emulator_filler], 100)
            .expect("both the original plan role and the newly proposed role convene for real");
        assert_eq!(assignments.len(), 2);
        assert!(assignments
            .iter()
            .any(|a| a.service == ServiceType::Custom("devsystem.android_emulator_test".to_string()) && a.provider == holder(3)));
    }

    #[test]
    fn apply_proposal_is_idempotent_never_double_declares_a_stage() {
        let mut spec = full_spec("run-3", None);
        let before = spec.roles.len();
        let proposal = StageProposal {
            proposed_by: STAGE_TEST.to_string(),
            stage_id: STAGE_TEST.to_string(),
            tag: "test".to_string(),
            rationale: "already exists -- must be a no-op".to_string(),
            use_existing_service: None,
            units: 1,
            price_ceiling: None,
        };
        assert_eq!(apply_proposal(&mut spec, &proposal), ProposalOutcome::AlreadyPresent);
        assert_eq!(spec.roles.len(), before, "no duplicate role for an already-declared stage");
    }

    #[test]
    fn validate_proposals_rejects_an_empty_stage_id_tag_or_rationale() {
        let base = StageProposal {
            proposed_by: "devsystem.plan".to_string(),
            stage_id: "devsystem.real".to_string(),
            tag: "real".to_string(),
            rationale: "a genuine reason".to_string(),
            use_existing_service: None,
            units: 1,
            price_ceiling: None,
        };
        assert!(validate_proposals(std::slice::from_ref(&base)).is_ok(), "a genuine, non-empty proposal must pass");

        let mut empty_stage_id = base.clone();
        empty_stage_id.stage_id = "".to_string();
        assert!(validate_proposals(&[empty_stage_id]).is_err());

        let mut empty_tag = base.clone();
        empty_tag.tag = "  ".to_string();
        assert!(validate_proposals(&[empty_tag]).is_err(), "whitespace-only must count as empty, not just byte-empty");

        let mut empty_rationale = base;
        empty_rationale.rationale = "".to_string();
        assert!(validate_proposals(&[empty_rationale]).is_err());
    }

    #[test]
    /// Real gap found live 2026-08-06 (stress-test run 55): validate_proposals had
    /// no upper bound on `units` at all, and an embedded proposal reaching it
    /// applies immediately with no human review gate -- live-confirmed a real
    /// units:0 embedded proposal got a real 200 against the actual deployment.
    fn validate_proposals_rejects_zero_or_absurdly_large_units() {
        let base = StageProposal {
            proposed_by: "devsystem.plan".to_string(),
            stage_id: "devsystem.real".to_string(),
            tag: "real".to_string(),
            rationale: "a genuine reason".to_string(),
            use_existing_service: None,
            units: 1,
            price_ceiling: None,
        };
        assert!(validate_proposals(std::slice::from_ref(&base)).is_ok(), "a genuine units:1 proposal must pass");

        let mut zero_units = base.clone();
        zero_units.units = 0;
        assert!(validate_proposals(&[zero_units]).is_err(), "units:0 must be rejected -- a role needing zero bidders is meaningless");

        let mut absurd_units = base;
        absurd_units.units = u64::MAX;
        assert!(validate_proposals(&[absurd_units]).is_err(), "an absurdly large units value must be rejected, not silently accepted");
    }

    #[test]
    /// DAU-lens gap found live 2026-08-06 (#382 goal doc §8): the two checks above
    /// each used to reject on the FIRST bad proposal in a batch and stop, so a real
    /// iteration submitting several simultaneously-bad embedded proposals needed one
    /// resubmit per additional mistake to discover them all. Every real caller
    /// (`web/src/main.rs`, `devsystem_iterate.rs`) passes a genuine batch, so this is
    /// a real, not hypothetical, case.
    fn validate_proposals_reports_every_bad_proposal_in_one_batch_not_just_the_first() {
        let good = StageProposal {
            proposed_by: "devsystem.plan".to_string(),
            stage_id: "devsystem.real".to_string(),
            tag: "real".to_string(),
            rationale: "a genuine reason".to_string(),
            use_existing_service: None,
            units: 1,
            price_ceiling: None,
        };
        let empty_stage_id = StageProposal { stage_id: "".to_string(), tag: "".to_string(), rationale: "".to_string(), ..good.clone() };
        let zero_units = StageProposal { stage_id: "devsystem.other".to_string(), units: 0, ..good.clone() };

        let err = validate_proposals(&[empty_stage_id, good, zero_units]).expect_err("a batch with two distinct bad proposals must still be rejected");
        assert!(err.contains("needs a non-empty stage_id"), "the empty-field proposal must be named: {err}");
        assert!(err.contains("devsystem.other") && err.contains("needs units between"), "the zero-units proposal must ALSO be named, not require a separate resubmit: {err}");
    }

    #[test]
    fn validate_proposals_is_the_real_gate_run_iteration_itself_does_not_enforce() {
        // apply_proposal (and run_iteration, which calls it directly) stays
        // permissive by design -- it trusts already-validated data. This proves
        // the two facts together: an entry point that skips validate_proposals
        // really does let garbage through run_iteration, which is exactly the
        // live-confirmed bug this function exists to close at every real entry
        // point, not just one.
        let mut spec = full_spec("run-validate", None);
        let before = spec.roles.len();
        let garbage = StageProposal {
            proposed_by: "devsystem.plan".to_string(),
            stage_id: "".to_string(),
            tag: "".to_string(),
            rationale: "".to_string(),
            use_existing_service: None,
            units: 1,
            price_ceiling: None,
        };
        assert!(validate_proposals(std::slice::from_ref(&garbage)).is_err(), "the shared gate must catch it");
        assert_eq!(apply_proposal(&mut spec, &garbage), ProposalOutcome::Added, "apply_proposal itself has no opinion -- callers must gate first");
        assert_eq!(spec.roles.len(), before + 1, "confirms apply_proposal really would add the garbage role if a caller skipped validate_proposals");
    }

    #[test]
    fn validate_feedback_rejects_empty_or_whitespace_only_text() {
        assert!(validate_feedback("a real, non-empty account of what happened").is_ok());
        assert!(validate_feedback("").is_err());
        assert!(validate_feedback("   ").is_err(), "whitespace-only must count as empty, not just byte-empty");
    }

    #[test]
    fn should_checkin_fires_on_the_configured_cadence_and_at_the_ceiling() {
        let criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 3, checkin_every: 5 };
        let rec = |iteration: u32| IterationRecord {
            run_id: "run-4".into(),
            stage: STAGE_IMPLEMENT.into(),
            iteration,
            feedback: "ok".into(),
            proposals: vec![],
            succeeded: true,
            requirement_indices: Vec::new(),
        };
        assert!(!should_checkin(&rec(1), &criteria));
        assert!(!should_checkin(&rec(4), &criteria));
        assert!(should_checkin(&rec(5), &criteria), "hits the configured cadence");
        assert!(should_checkin(&rec(10), &criteria), "hits the cadence again");
        assert!(should_checkin(&rec(20), &criteria), "hard ceiling always forces a check-in");
    }

    #[test]
    fn should_abort_when_consecutive_failures_reach_the_bound_even_off_cadence() {
        let criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 3, checkin_every: 5 };
        assert!(!should_abort(2, 7, &criteria));
        assert!(should_abort(3, 7, &criteria), "three consecutive failures aborts regardless of iteration count");
        assert!(should_abort(0, 20, &criteria), "reaching the iteration ceiling also aborts");
    }

    #[test]
    fn an_offer_for_a_different_stage_never_fills_the_plan_role() {
        // Custom service types don't alias each other -- an implement-stage offer can't
        // accidentally clear the plan role just because both are ServiceType::Custom.
        let spec = plan_only_spec("test-run", None);
        let implement_filler = offer(2, vec![ServiceType::Custom(STAGE_IMPLEMENT.to_string())], 5);
        assert_eq!(
            spec.convene(&[implement_filler], 100),
            Err(PipelineError::UnfilledRole { service: ServiceType::Custom(STAGE_PLAN.to_string()) })
        );
    }
}
