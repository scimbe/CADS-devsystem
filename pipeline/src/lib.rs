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

/// Trojan Source (CVE-2021-42574) bidi control characters -- a real DAU-lens gap
/// found live by the incompetent-agent stress test (#382 goal doc §8, 2026-08-06).
/// Moved here (from `web/src/main.rs`, where it was found and first fixed for
/// requirement statement/criteria, then extended to milestones, backlog, and
/// custom-panel title) so `validate_proposals` below -- reached from `devsystem-web`
/// AND from `devsystem_iterate`'s local, non-`--remote` CLI path, which has no HTTP
/// layer to share a check through any other way -- can close its own last-remaining
/// candidate, `rationale`, from the single real place both entry points already
/// share. `devsystem-web` re-exports this rather than keeping a second copy, the
/// same "single source of truth" discipline `MAX_ROLE_UNITS` above already
/// established for this exact pair of crates.
pub const BIDI_CONTROL_CHARS: [char; 9] = ['\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}'];

pub fn contains_bidi_control_char(s: &str) -> bool {
    s.chars().any(|c| BIDI_CONTROL_CHARS.contains(&c))
}

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
///
/// Extended 2026-08-06 (stress-test run 13) to reject a bidi control character in
/// `rationale` too -- live-confirmed before fixing: a rationale reading "Needed for
/// real testing" followed by a real U+202E and reversed text sailed through
/// untouched, visually hiding an admission ("This is a dangerous stage -- exposes
/// actual data extraction") that would otherwise have been the whole point of a
/// human reading the rationale before approving. This is the more consequential of
/// `rationale`'s two real entry points: an embedded proposal reaching this function
/// applies *immediately* to the live spec, no human review gate at all (see
/// `MAX_ROLE_UNITS`'s own doc comment above).
pub fn validate_proposals(proposals: &[StageProposal]) -> Result<(), String> {
    let bad: Vec<String> = proposals
        .iter()
        .filter_map(|p| {
            if p.stage_id.trim().is_empty() || p.tag.trim().is_empty() || p.rationale.trim().is_empty() {
                Some(format!("proposal for stage_id {:?} needs a non-empty stage_id, tag, and rationale", p.stage_id))
            } else if p.units == 0 || p.units > MAX_ROLE_UNITS {
                Some(format!("proposal for stage_id {:?} needs units between 1 and {MAX_ROLE_UNITS}, got {}", p.stage_id, p.units))
            } else if contains_bidi_control_char(&p.rationale) {
                Some(format!(
                    "proposal for stage_id {:?} has a rationale containing a Unicode bidi control character (e.g. a \
                     right-to-left override) -- these can make the visually displayed text not match what's actually stored",
                    p.stage_id
                ))
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

/// A submitted iteration's `stage` name -- short, identifier-shaped, unlike
/// `feedback`'s free-flowing prose, so 200 characters (not `feedback`'s much
/// looser bound) is generous for any real role tag while still finite.
const MAX_STAGE_LEN: usize = 200;

/// Real evaluator finding, issue #49, 2026-08-07: the mandatory review gate itself
/// (`toggle_requirement`) is sound -- six separate attempts to fake a review all
/// correctly got a real `409`. But the gate's entire notion of "a review happened"
/// is keyed on one field, `IterationRecord.stage`, and until this fix `stage` was
/// the one free-text field in this whole API with no validation at all: not
/// non-empty, not length-capped, not checked against the run's own declared roles.
/// Live-confirmed before fixing: `stage: ""`, `stage: "   "`, and a 5,000-character
/// `stage` all got a real `200` and were stored verbatim; `stage:
/// "devsystem.architekt-undeclared-probe"` (naming no role this run ever declared)
/// was accepted identically to a real role's own tag, with no auction ever having
/// convened for it. The user-visible trap: a reviewer who submits
/// `"  DEVSYSTEM.REVIEW  "` (case/whitespace near-miss) gets a real `200` and a
/// real, visible history entry that *reads* as a completed review -- the review
/// gate's own exact-match comparison then silently doesn't count it, with nothing
/// in the UI ever explaining why. A gate is only as strong as the integrity of what
/// it reads.
///
/// Checked against the RAW, untrimmed `stage` (not a trimmed copy) for role
/// membership deliberately -- trimming first would let exactly the near-miss case
/// above (correct role name, stray whitespace) silently pass this check while still
/// failing the review gate's own later exact match, reintroducing the identical
/// trap one step removed. Failing loudly here, at submission time, is the fix
/// suggestion #2 explicitly asks for.
///
/// `same_submission_proposals` is deliberately part of the real, valid set too --
/// the self-optimizing pipeline's own documented mechanism lets a role-filler
/// propose a brand-new stage AND report its own work under that exact stage name
/// in the identical submission (`apply_proposal` runs after this check, so the new
/// role doesn't exist in `spec.roles` yet at this point). Checking only the
/// pre-existing spec would incorrectly reject that real, intended case.
///
/// A stage in [`ALL_STAGES`] is deliberately valid regardless of `spec.roles` too
/// -- confirmed by re-checking the real flagship `webconference-android` run
/// before shipping this: its own real history genuinely uses `devsystem.improve`
/// (self-optimization iterations, e.g. adding a real requirement or proposing a
/// new role) despite `improve` never being, and never needing to be, an
/// auction-backed role in `spec.roles` -- it's the mechanism *by which* new roles
/// get proposed in the first place, so requiring it to already be a declared role
/// would be circular. `spec.roles` is the auction-backed *subset* of the seven
/// canonical stages a given run has chosen to make biddable, not the complete set
/// of valid stage names -- restricting to only `spec.roles` would have broken this
/// real, established, correct usage, not just a hypothetical one.
pub fn validate_stage(stage: &str, spec: &PipelineSpec, same_submission_proposals: &[StageProposal]) -> Result<(), String> {
    if stage.trim().is_empty() {
        return Err("stage must not be empty".to_string());
    }
    let len = stage.chars().count();
    if len > MAX_STAGE_LEN {
        return Err(format!("stage must be at most {MAX_STAGE_LEN} characters, got {len}"));
    }
    if contains_bidi_control_char(stage) {
        return Err(
            "stage contains a Unicode bidi control character (e.g. a right-to-left override) -- \
             these can make the visually displayed text not match what's actually stored"
                .to_string(),
        );
    }
    let canonical = ALL_STAGES.contains(&stage);
    let declared_already = spec.roles.iter().any(|r| r.service == ServiceType::Custom(stage.to_string()));
    let declared_this_submission = same_submission_proposals.iter().any(|p| p.stage_id == stage);
    if !canonical && !declared_already && !declared_this_submission {
        return Err(format!(
            "stage {stage:?} is not one of this project's seven canonical pipeline stages, does not name \
             any role currently declared in this run's own PipelineSpec, and this submission's own \
             proposals (if any) don't declare it either -- check spelling/case/whitespace against the \
             run's real roles (GET /api/runs/{{id}}), or include a matching proposal to declare it as a \
             new stage in this same submission"
        ));
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
    /// Real identity (GitHub issue #38, a live evaluator finding: the exact same
    /// iteration got submitted twice, byte-for-byte, into `webconference-android`'s
    /// real history -- with no field on the record to tell the two apart, notice
    /// the repeat, or say which one was real). Server-generated, the same
    /// `format!("{:016x}", rand::random::<u64>())` convention every other real id
    /// in this codebase already uses (`PendingDeleteRunProposal`, `RagDocument`,
    /// `PendingPanelProposal`, ...) -- deliberately never accepted from a client,
    /// so a role-filler/CLI cannot forge or collide it.
    ///
    /// Real evaluator finding, issue #52: `#38`'s original landing made this a
    /// bare `String` with `#[serde(default)]`, so every pre-existing history entry
    /// deserialized as `""` -- a valid-*looking* value, not an explicit absence. A
    /// census against the live deployment found 114 of 133 records platform-wide
    /// (18 of `webconference-android`'s own 21) carrying that sentinel, making
    /// `id` unable to actually identify a record: all 114 collide on the same
    /// empty string, and nothing could tell "not recorded" from "recorded as
    /// empty." Now `Option<String>`, matching `owner_email`'s own established
    /// honest-absence convention -- `None` really means "this record predates
    /// the id field," serialized as a real, visible `null`, not a value a naive
    /// consumer could mistake for identity. `deserialize_id_or_empty_as_none`
    /// normalizes the legacy `""` sentinel (and, for robustness, an explicit
    /// `null`) to `None` on load -- every real id minted since stays exactly as
    /// real; nothing is fabricated for the 114 that never had one.
    #[serde(default, deserialize_with = "deserialize_id_or_empty_as_none")]
    pub id: Option<String>,
    /// Real submission timestamp (same gap as `id` above, and the same issue #52
    /// finding: `0` is Unix epoch, a well-formed-*looking* 1970-01-01 timestamp,
    /// not an honest "unknown" -- any consumer that formatted it would render a
    /// confident, wrong date). Now `Option<u64>`; `deserialize_zero_as_none`
    /// normalizes the legacy `0` sentinel (and an explicit `null`) to `None` on
    /// load. Unix seconds when present, server-set the same way `unix_now()`
    /// already stamps every other real `*_at` field in this codebase.
    #[serde(default, deserialize_with = "deserialize_zero_as_none")]
    pub submitted_at: Option<u64>,
    /// Real evaluator finding, issue #40: the platform's own stated premise is a crew
    /// auction -- distinct crews bid for and win roles -- but until this field, the
    /// winning crew's identity was never written into the work record it produced.
    /// The only place any bidder identity ever appeared was the live auction view
    /// (`GET /api/runs/{id}/auction`), and every bid there expires 300 seconds after
    /// being issued -- so "who submitted iteration N" became permanently unanswerable
    /// the moment that window passed, for every iteration, on every run. Confirmed
    /// live against `webconference-android`: iteration 17's authorizing bid had long
    /// expired, and the role's current holder was a completely different, unrelated
    /// bidder -- no record anywhere still connected that iteration to the crew that
    /// actually produced it.
    ///
    /// `Some(the real, gate-verified x-gate-email)` when a signed-in browser session
    /// submitted the iteration -- the same source `/api/me` and `owner_email` already
    /// use, deliberately never trusted from the request body itself (a client could
    /// claim to be anyone). `None`, honestly, for the local `devsystem_iterate` CLI
    /// path (no browser session exists there at all) and for `--remote` M2M
    /// bearer-token submissions (a service-account credential, not a human identity --
    /// fabricating a person's name for it would be worse than an honest gap). This is
    /// a label ("which account's browser session submitted this"), not the winning
    /// bid's own identity (issue #40's suggestion #2, separate and not yet done): a
    /// role can be filled by an unattended agent process with no browser session at
    /// all, which this field alone cannot capture.
    #[serde(default)]
    pub submitted_by: Option<String>,
}

fn deserialize_id_or_empty_as_none<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = serde::Deserialize::deserialize(deserializer)?;
    Ok(opt.filter(|s| !s.is_empty()))
}

fn deserialize_zero_as_none<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<u64> = serde::Deserialize::deserialize(deserializer)?;
    Ok(opt.filter(|&t| t != 0))
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
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06, stress-test run 13): closes out the bidi-control-
    /// character class's last remaining candidate. `rationale` is what a human
    /// reads to decide whether a proposal is safe to approve -- live-confirmed
    /// before fixing: "Needed for real testing" + U+202E + reversed text sailed
    /// through untouched, visually hiding "This is a dangerous stage -- exposes
    /// actual data extraction" behind an innocuous-looking rationale. This is
    /// the more consequential of `rationale`'s two real entry points: an
    /// embedded proposal reaching this function applies immediately, no human
    /// review gate at all (see MAX_ROLE_UNITS's own doc comment above).
    fn validate_proposals_rejects_a_bidi_control_character_in_rationale() {
        let base = StageProposal {
            proposed_by: "devsystem.plan".to_string(),
            stage_id: "devsystem.real".to_string(),
            tag: "real".to_string(),
            rationale: "a genuine reason".to_string(),
            use_existing_service: None,
            units: 1,
            price_ceiling: None,
        };
        assert!(validate_proposals(std::slice::from_ref(&base)).is_ok(), "a genuine, clean rationale must pass");

        let mut bidi = base;
        bidi.rationale = "Needed for real testing\u{202e} noitcaxe atad lautca sesopxe -- egats suoregnad a si sihT".to_string();
        let err = validate_proposals(&[bidi]).expect_err("a rationale containing a bidi override character must be rejected");
        assert!(err.contains("bidi control character"), "{err}");
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
    fn validate_stage_rejects_empty_or_whitespace_only_stage() {
        let spec = plan_only_spec("stage-run", None);
        assert!(validate_stage("", &spec, &[]).is_err());
        assert!(validate_stage("   ", &spec, &[]).is_err(), "whitespace-only must count as empty, not just byte-empty");
    }

    #[test]
    fn validate_stage_rejects_a_stage_over_the_length_cap() {
        let spec = plan_only_spec("stage-run", None);
        let huge = "x".repeat(5_000);
        assert!(validate_stage(&huge, &spec, &[]).is_err());
        let at_cap = "x".repeat(MAX_STAGE_LEN);
        assert!(validate_stage(&at_cap, &spec, &[]).is_err(), "a stage this long names no real role and no canonical stage, so it's still rejected on membership grounds even though it's within the length cap");
    }

    #[test]
    fn validate_stage_accepts_any_canonical_all_stages_member_even_with_no_declared_role() {
        // The real, live flagship webconference-android run's own case: devsystem.improve
        // is used in real history despite never being a declared role in spec.roles --
        // requiring it to already be declared would be circular, since it's the mechanism
        // that proposes other roles in the first place.
        let spec = plan_only_spec("stage-run", None);
        for stage in ALL_STAGES {
            assert!(validate_stage(stage, &spec, &[]).is_ok(), "canonical stage {stage:?} must be valid regardless of spec.roles");
        }
    }

    #[test]
    fn validate_stage_accepts_a_role_declared_in_the_runs_own_spec() {
        // full_spec's own seven roles are already covered by the ALL_STAGES test above --
        // exercise the spec.roles path with a genuinely non-canonical custom role instead.
        let mut spec = plan_only_spec("stage-run", None);
        spec.roles.push(RequiredRole {
            service: ServiceType::Custom("devsystem.android_native_bridge".to_string()),
            units: 1,
            tag: "android_native_bridge".to_string(),
            selection_policy: None,
        });
        assert!(validate_stage("devsystem.android_native_bridge", &spec, &[]).is_ok());
    }

    #[test]
    fn validate_stage_accepts_a_same_submission_proposed_stage_not_yet_in_spec_roles() {
        // The self-optimizing pipeline's own real pattern: a role-filler proposes a new
        // stage AND reports its own work under that exact stage name in the same
        // submission -- apply_proposal runs after this check, so the new role doesn't
        // exist in spec.roles yet at validation time.
        let spec = plan_only_spec("stage-run", None);
        let proposal = StageProposal {
            proposed_by: "devsystem.improve".to_string(),
            stage_id: "devsystem.android_native_bridge".to_string(),
            tag: "android_native_bridge".to_string(),
            rationale: "real android-specific work needs its own stage".to_string(),
            use_existing_service: None,
            units: 1,
            price_ceiling: None,
        };
        assert!(validate_stage("devsystem.android_native_bridge", &spec, std::slice::from_ref(&proposal)).is_ok());
        assert!(
            validate_stage("devsystem.some_other_undeclared_stage", &spec, std::slice::from_ref(&proposal)).is_err(),
            "a stage naming no role, no canonical stage, and no matching proposal in this batch must still be rejected"
        );
    }

    #[test]
    fn validate_stage_rejects_a_genuinely_undeclared_non_canonical_stage() {
        // Live repro from issue #49: a role/stage name this run never declared and
        // never proposed must not be accepted identically to a real one.
        let spec = plan_only_spec("stage-run", None);
        let err = validate_stage("devsystem.architekt-undeclared-probe", &spec, &[]).unwrap_err();
        assert!(err.contains("architekt-undeclared-probe"), "the error should name the real rejected stage: {err}");
    }

    #[test]
    fn validate_stage_rejects_a_case_or_whitespace_near_miss_on_a_real_declared_role() {
        // Live repro from issue #49: "  DEVSYSTEM.REVIEW  " must not silently pass this
        // check only to then fail the review gate's own later exact-match comparison
        // with no explanation anywhere. Checked against the raw, untrimmed stage on
        // purpose -- see validate_stage's own doc comment.
        let spec = full_spec("stage-run", None);
        assert!(validate_stage("devsystem.review", &spec, &[]).is_ok(), "the real, exact declared stage must still work");
        assert!(validate_stage("  DEVSYSTEM.REVIEW  ", &spec, &[]).is_err(), "case/whitespace near-misses must be rejected, not silently accepted then silently ignored by the review gate");
    }

    #[test]
    /// Real evaluator finding, issue #52: a census against the live deployment found
    /// 114 of 133 real iteration records still carrying the legacy `id: ""` /
    /// `submitted_at: 0` sentinel from before these fields became `Option`. Every one
    /// of those records must still load, and must load as a real, honest `None` -- not
    /// `Some("")` / `Some(0)`, which would look like a valid-but-empty identity rather
    /// than an explicit absence.
    fn legacy_empty_id_and_zero_submitted_at_deserialize_as_none() {
        let json = r#"{"run_id":"r","stage":"devsystem.plan","iteration":1,"feedback":"f",
            "proposals":[],"succeeded":true,"requirement_indices":[],"id":"","submitted_at":0}"#;
        let record: IterationRecord = serde_json::from_str(json).expect("legacy record must still load");
        assert_eq!(record.id, None, "the legacy empty-string sentinel must normalize to a real, honest None");
        assert_eq!(record.submitted_at, None, "the legacy zero (Unix epoch) sentinel must normalize to a real, honest None");
    }

    #[test]
    /// A record with genuinely no `id`/`submitted_at` keys at all (pre-#38 data, before
    /// the fields existed) must also load as `None` -- the `#[serde(default)]` path,
    /// distinct from but equally as important as the empty-string/zero sentinel path
    /// above.
    fn a_record_with_no_id_field_at_all_also_deserializes_as_none() {
        let json = r#"{"run_id":"r","stage":"devsystem.plan","iteration":1,"feedback":"f",
            "proposals":[],"succeeded":true,"requirement_indices":[]}"#;
        let record: IterationRecord = serde_json::from_str(json).expect("pre-#38 record must still load");
        assert_eq!(record.id, None);
        assert_eq!(record.submitted_at, None);
    }

    #[test]
    fn a_real_id_and_submitted_at_round_trip_unchanged() {
        let json = r#"{"run_id":"r","stage":"devsystem.plan","iteration":1,"feedback":"f",
            "proposals":[],"succeeded":true,"requirement_indices":[],"id":"abc123","submitted_at":1786000000}"#;
        let record: IterationRecord = serde_json::from_str(json).expect("real record must load");
        assert_eq!(record.id, Some("abc123".to_string()), "a real, non-empty id must never be treated as absent");
        assert_eq!(record.submitted_at, Some(1786000000), "a real, non-zero timestamp must never be treated as absent");

        let serialized = serde_json::to_value(&record).unwrap();
        assert_eq!(serialized["id"], "abc123");
        assert_eq!(serialized["submitted_at"], 1786000000);
    }

    #[test]
    fn a_none_id_and_submitted_at_serialize_as_a_real_visible_null_not_an_omitted_field() {
        let record = IterationRecord { run_id: "r".into(), stage: "devsystem.plan".into(), iteration: 1, feedback: "f".into(), succeeded: true, ..Default::default() };
        assert_eq!(record.id, None);
        assert_eq!(record.submitted_at, None);
        let serialized = serde_json::to_value(&record).unwrap();
        assert!(serialized.get("id").is_some(), "the field must still be present in the JSON, not omitted");
        assert!(serialized["id"].is_null(), "and its value must be a real, visible null a consumer can branch on");
        assert!(serialized["submitted_at"].is_null());
    }

    #[test]
    /// Real evaluator finding, issue #40: a pre-existing history entry (before this
    /// field existed) has no `submitted_by` key at all -- must load as an honest
    /// `None`, not error out or silently invent an identity.
    fn a_record_with_no_submitted_by_field_deserializes_as_none() {
        let json = r#"{"run_id":"r","stage":"devsystem.plan","iteration":1,"feedback":"f",
            "proposals":[],"succeeded":true,"requirement_indices":[]}"#;
        let record: IterationRecord = serde_json::from_str(json).expect("pre-#40 record must still load");
        assert_eq!(record.submitted_by, None);
    }

    #[test]
    fn a_real_submitted_by_round_trips_unchanged() {
        let json = r#"{"run_id":"r","stage":"devsystem.plan","iteration":1,"feedback":"f",
            "proposals":[],"succeeded":true,"requirement_indices":[],"submitted_by":"scimbe@gmail.com"}"#;
        let record: IterationRecord = serde_json::from_str(json).expect("real record must load");
        assert_eq!(record.submitted_by, Some("scimbe@gmail.com".to_string()));
        let serialized = serde_json::to_value(&record).unwrap();
        assert_eq!(serialized["submitted_by"], "scimbe@gmail.com");
    }

    #[test]
    fn a_none_submitted_by_serializes_as_a_real_visible_null_not_an_omitted_field() {
        let record = IterationRecord { run_id: "r".into(), stage: "devsystem.plan".into(), iteration: 1, feedback: "f".into(), succeeded: true, ..Default::default() };
        assert_eq!(record.submitted_by, None);
        let serialized = serde_json::to_value(&record).unwrap();
        assert!(serialized.get("submitted_by").is_some(), "must still be present, not omitted");
        assert!(serialized["submitted_by"].is_null());
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
            ..Default::default()
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
