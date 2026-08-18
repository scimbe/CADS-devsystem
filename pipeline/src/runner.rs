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

/// A human's real inline note on one specific past iteration -- operator feedback,
/// UX pass 2026-08-18 ("mehr potentielle Einflussgabe... neue Eingriffspunkte"):
/// today the only way to react to an iteration is to submit an entirely new one via
/// New Iteration, with no way to attach a remark to a *specific already-happened*
/// entry without it getting lost among unrelated backlog/requirement text. Addressed
/// by `history_index`, the same array-position convention `toggle_backlog_item`/
/// `toggle_milestone` already use to reference one entry in a `Vec` -- deliberately
/// NOT `IterationRecord::iteration`, since issues #38/#52 already found live that
/// iteration numbers can repeat within a run's real history, making them unsafe as
/// a unique reference. Append-only (no edit/delete yet -- the same minimal first
/// slice this project's other real-but-narrow features started as, e.g. Requirements
/// before edit support existed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterationComment {
    pub history_index: usize,
    /// The real, gate-verified `x-gate-email` of whoever wrote this, same
    /// never-trust-the-body discipline as `RunState::owner_email`/`created_by`
    /// elsewhere. `None` for a caller with no real session (e.g. a local CLI).
    pub author: Option<String>,
    pub text: String,
    pub created_at: u64,
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
/// should make, not one to guess at here.
///
/// `auto_judge` (2026-08-05, operator decision on the open question above):
/// still human-driven by default -- a requirement starts and stays 100%
/// human-toggled unless its owner explicitly opts it into LLM judgment via
/// this flag. Deliberately per-requirement, not a global run/account
/// setting: the operator's own framing ("should be human, but he can ask for
/// automode") means opting in is a real, considered choice on a specific
/// requirement, not a blanket default anyone could silently inherit.
/// `#[serde(default)]` so every pre-existing requirement loads as
/// `auto_judge: false` (still fully human-driven) with no migration step.
/// Setting this flag alone does not itself judge anything -- it only
/// authorizes devsystem.assistant to do so; the actual judgment logic is a
/// separate, later increment.
///
/// `automode` (issue #31, 2026-08-10 -- the honest first slice, not the full feature):
/// a deliberately SEPARATE flag from `auto_judge` above, not a rename -- issue #31 asks
/// for a much broader scope (proposal -> bidding -> role-fill -> iteration, not just
/// judgment) and this project's own investigation of the issue named a real, unresolved
/// tension before any of that can be built safely: an automated path that auto-submits
/// iterations and auto-marks requirements verified with no real review in the loop is,
/// structurally, the same hole `qualifying_review_evidence` exists to close, just
/// through a different door. That question (does an automode-driven iteration still
/// have to clear the real review gate?) needs a real operator answer, not a guess --
/// so, mirroring `auto_judge`'s own precedent exactly, this flag is a real, honestly
/// scoped placeholder: recorded, persisted, visible in the GUI, and does not yet drive
/// any actual automatic proposal/bid/iteration behavior. `#[serde(default)]` so every
/// pre-existing requirement loads as `automode: false` (still fully human-driven) with
/// no migration step.
/// `proposed_by` (#382 goal doc, real gap #1 -- provenance): `None` means a human wrote
/// this requirement directly; `Some(stage_tag)` means an LLM role-filler proposed it
/// (mirrors `StageProposal::proposed_by`'s existing convention of naming the stage, not
/// a person). Without this, a user has no way to tell which requirements/acceptance
/// criteria are already theirs vs. which are still an LLM's first draft waiting on
/// review -- blocking the goal's own §3 ("the user must know which details the LLM set
/// first") and §4.4 (knowing what's safe to leave alone vs. needs tightening).
/// `#[serde(default)]` so every pre-existing requirement loads as `proposed_by: None`
/// (human-authored, the safe default) with no migration step.
///
/// `created_by` (real evaluator finding, issue #55): a genuinely separate signal from
/// `proposed_by` above -- that field answers "human-written or LLM-proposed", never
/// "which real account." An evaluator read `proposed_by: null` on a requirement they'd
/// just created through the GUI while signed in and concluded there was no author field
/// at all; `proposed_by: null` was actually correct there (it *is* human-authored), but
/// their deeper point holds independently: nothing anywhere records which real,
/// gate-verified account created a requirement, the same gap `owner_email` (runs) and
/// `submitted_by` (iterations) already closed elsewhere. Deliberately a NEW field
/// rather than repurposing `proposed_by`'s own established, tested meaning.
/// `#[serde(default)]` so every pre-existing requirement (created before this field
/// existed) loads as `created_by: None` -- honest, not a guess.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Requirement {
    pub statement: String,
    pub acceptance_criteria: Vec<String>,
    pub verified: bool,
    /// Real evaluator finding, issue #55: this used to be a bare `Vec<bool>` -- the
    /// platform's own highest-stakes verdict signal (a *confirmed* criterion, not just
    /// a report) carried zero provenance: no actor, no timestamp, nothing. Live-
    /// confirmed against the actual flagship `webconference-android` run before fixing:
    /// its own one real confirmed criterion (requirement 5, index 4) could not be
    /// attributed to anyone, and no reviewer could tell a real confirmation apart from
    /// any signed-in account's stray click. Now `Vec<Option<CriterionVerification>>`
    /// -- `None` means not confirmed, `Some(record)` carries who and when. Toggling
    /// off clears back to `None` (honest: nobody currently vouches for it), matching
    /// this project's own established "toggle" semantics for `verified` itself.
    ///
    /// `deserialize_verified_criteria` migrates the legacy `Vec<bool>` wire format in
    /// place: a legacy `true` becomes `Some(CriterionVerification { confirmed_by: None,
    /// confirmed_at: None })` -- the real fact "this was confirmed" survives, honestly
    /// paired with "who and when is unknown" rather than inventing either. A legacy
    /// `false` becomes `None`. `#[serde(default)]` so a requirement with no field at
    /// all (predates 2026-08-05) still loads as empty.
    #[serde(default, deserialize_with = "deserialize_verified_criteria")]
    pub verified_criteria: Vec<Option<CriterionVerification>>,
    #[serde(default)]
    pub auto_judge: bool,
    #[serde(default)]
    pub automode: bool,
    #[serde(default)]
    pub proposed_by: Option<String>,
    #[serde(default)]
    pub created_by: Option<String>,
}

/// See [`Requirement::verified_criteria`]'s own doc comment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CriterionVerification {
    pub confirmed_by: Option<String>,
    pub confirmed_at: Option<u64>,
}

fn deserialize_verified_criteria<'de, D>(deserializer: D) -> Result<Vec<Option<CriterionVerification>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<serde_json::Value> = serde::Deserialize::deserialize(deserializer)?;
    raw.into_iter()
        .map(|v| match v {
            serde_json::Value::Bool(true) => Ok(Some(CriterionVerification { confirmed_by: None, confirmed_at: None })),
            serde_json::Value::Bool(false) | serde_json::Value::Null => Ok(None),
            other => serde_json::from_value(other).map_err(serde::de::Error::custom),
        })
        .collect()
}

/// Real requirements export (#382 goal doc §4.4, gap #7): until now `GET
/// /api/runs/{id}` was the only way to see a run's requirements at all -- a raw
/// Real gap found live by the incompetent-agent stress test (#382 goal doc §8,
/// 2026-08-06): free-text a role-filler/human fully controls, spliced directly
/// into a document a human is meant to trust as real markdown, can impersonate
/// that document's own structure -- first found in the check-in artifact
/// (`checkin.rs`'s own `render_iteration`), moved here so
/// [`render_requirements_markdown`] can share the identical fix rather than
/// duplicating it (a live test proved the export was vulnerable to the same
/// class: a crafted requirement `statement` containing `"## 2. ✅\n\n...\n\n
/// *Human-authored.*"` rendered as a completely convincing forged SECOND
/// requirement entry, falsely showing as verified and human-authored --
/// directly undermining `proposed_by`'s own provenance signal, this
/// document's whole reason to exist). Wrapped in a fenced code block, so it
/// displays in full (nothing hidden or stripped) but can never be mistaken
/// for real structure. The fence length is chosen longer than the longest run
/// of consecutive backticks already in the text (CommonMark: a closing fence
/// must be at least as long as the opening one; a shorter backtick run inside
/// stays literal), so content can't break out of its own fence either.
pub(crate) fn fence_wrap(text: &str) -> String {
    let mut longest_run = 0;
    let mut current_run = 0;
    for c in text.chars() {
        if c == '`' {
            current_run += 1;
            longest_run = longest_run.max(current_run);
        } else {
            current_run = 0;
        }
    }
    let fence = "`".repeat((longest_run + 1).max(3));
    format!("{fence}\n{text}\n{fence}")
}

/// Same reasoning as [`fence_wrap`], for content that has to stay on one line
/// (a single `- ` list item can't hold a fenced code block, which needs its
/// own lines) -- a backtick-delimited inline code span instead, sized the
/// same way (longer than the longest existing backtick run), with a padding
/// space on each side if the text itself starts or ends with a backtick
/// (CommonMark's own rule -- without it, the delimiter and the text's own
/// leading/trailing backtick would visually merge).
pub(crate) fn inline_code_escape(text: &str) -> String {
    let mut longest_run = 0;
    let mut current_run = 0;
    for c in text.chars() {
        if c == '`' {
            current_run += 1;
            longest_run = longest_run.max(current_run);
        } else {
            current_run = 0;
        }
    }
    let delim = "`".repeat(longest_run + 1);
    if text.starts_with('`') || text.ends_with('`') {
        format!("{delim} {text} {delim}")
    } else {
        format!("{delim}{text}{delim}")
    }
}

/// JSON blob, not something a non-technical stakeholder would download and read.
/// Renders every requirement as real Markdown: statement, verified/unverified
/// (with a per-criterion checklist), and provenance (`proposed_by`, per §3 --
/// whether this was a human's own requirement or still an LLM's first draft).
/// Pure and hermetically testable on purpose (no timestamp, no I/O) -- the web
/// layer decides what, if anything, to wrap around this.
/// `history` is used purely to compute each requirement's own real coverage line --
/// see the doc comment right below on why the heading numbering and this both
/// changed together (issue #35, #382 goal doc).
pub fn render_requirements_markdown(run_id: &str, requirements: &[Requirement], history: &[IterationRecord]) -> String {
    let mut md = format!("# Requirements: `{run_id}`\n\n");
    if requirements.is_empty() {
        md.push_str("No requirements defined yet.\n");
        return md;
    }
    let verified_count = requirements.iter().filter(|r| r.verified).count();
    md.push_str(&format!("{verified_count}/{} verified.\n\n", requirements.len()));
    for (i, r) in requirements.iter().enumerate() {
        // Real evaluator finding, issue #35: this used to number headings `## 1.`,
        // `## 2.`, ... -- a fresh 1-based counter with no relationship to the run's
        // own real, 0-based requirement ordinal that the GUI, the New Iteration
        // panel's "Addresses" checkboxes, and every iteration's own
        // `requirement_indices` all actually use. The same label meant two
        // different requirements depending on which surface you read (live-
        // confirmed: `webconference-android`'s real `#5`, the APK requirement,
        // exported as `## 6.`, with `## 5.` silently becoming a different
        // requirement entirely) -- an export handed to a reviewer as the
        // traceability artifact pointed them at the wrong text. Now emits the
        // same real ordinal every other surface uses, `#{i}`, so a citation like
        // "addresses requirement #5" means the same thing everywhere.
        md.push_str(&format!("## #{i} {}\n\n", if r.verified { "✅" } else { "◻" }));
        md.push_str(&fence_wrap(&r.statement));
        md.push_str("\n\n");
        // `proposed_by` is real, role-filler-controlled free text (add_requirement
        // only trims/empty-checks it) -- inline_code_escape'd for the same reason
        // `statement`/each criterion right above already are (real gap found live
        // 2026-08-06, this function's own residual instance of the class it
        // otherwise already closes).
        match &r.proposed_by {
            Some(stage) => md.push_str(&format!("*Proposed by {} -- not yet a human's own requirement unless separately confirmed.*\n\n", inline_code_escape(stage))),
            None => md.push_str("*Human-authored.*\n\n"),
        }
        // Real evaluator finding, issue #35 part 2: the export used to carry zero
        // coverage information -- a requirement with a real, substantive iteration
        // linked to it and one with none at all were byte-for-byte indistinguishable
        // in the document. Same real derivation the Requirements panel's own
        // `addressedBy` already uses (scan `history` for `requirement_indices`
        // containing this ordinal), mirrored here so the exported artifact matches
        // what a reviewer sees on screen, not a stripped-down copy of it.
        let addressed: Vec<u32> = history.iter().filter(|h| h.requirement_indices.contains(&i)).map(|h| h.iteration).collect();
        if addressed.is_empty() {
            md.push_str("*Not yet addressed by any iteration.*\n\n");
        } else {
            let list = addressed.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
            md.push_str(&format!("*Addressed by iteration(s) {list}.*\n\n"));
        }
        md.push_str("Acceptance criteria:\n\n");
        for (ci, c) in r.acceptance_criteria.iter().enumerate() {
            // Real evaluator finding, issue #55: a confirmed criterion now carries
            // real provenance (who, when) instead of a bare boolean -- surfaced here
            // too, not just in the API, so a reviewer relying on this check-in
            // markdown can actually tell a real, attributed confirmation apart from
            // legacy data where who/when is honestly unknown.
            let verification = r.verified_criteria.get(ci).and_then(|v| v.as_ref());
            let checked = verification.is_some();
            let provenance = match verification {
                Some(CriterionVerification { confirmed_by: Some(by), confirmed_at: Some(at) }) => format!(" (confirmed by {} at {at})", inline_code_escape(by)),
                Some(CriterionVerification { confirmed_by: None, confirmed_at: Some(at) }) => format!(" (confirmed at {at}, no account on the session)"),
                Some(_) => " (confirmed -- predates this project's own provenance tracking, who/when unknown)".to_string(),
                None => String::new(),
            };
            md.push_str(&format!("- [{}] {}{}\n", if checked { "x" } else { " " }, inline_code_escape(c), provenance));
        }
        md.push('\n');
    }
    md
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
    /// The real, full `StageProposal` behind every entry in `added_stages` -- not just
    /// the stage id, the actual proposal (including its real `price_ceiling`) that got
    /// applied. Real gap found live by the stress test, twenty-fifth run, 2026-08-06:
    /// `no_price_ceiling` (preflight.rs) only ever scanned `history.proposals`, which
    /// only a role-filler's own iteration-embedded proposals land in
    /// (`run_iteration` pushes the whole `IterationRecord` there) -- an
    /// assistant-relayed proposal, approved via `POST .../stages/proposals/{id}/approve`,
    /// never touches `history` at all (`approve_stage_proposal` mutates `spec`/
    /// `added_stages` directly, then discards the pending proposal), so its real
    /// `price_ceiling` became permanently unrecoverable the moment it was approved --
    /// not just invisible to one check, genuinely lost. The same "two real entry
    /// points, one bug class" shape already found and fixed this session for
    /// `validate_proposals`/markdown-fencing/`valid_run_id`/signing-key permissions.
    /// Both real call sites (`run_iteration` here, `approve_stage_proposal` in
    /// `web/src/main.rs`) push here whenever `apply_proposal` runs, so this is the
    /// one complete, honest record regardless of which path a proposal took --
    /// `#[serde(default)]` so pre-existing `state.json` files still load.
    ///
    /// **Real gap found live by the stress test, twenty-seventh run, 2026-08-06**:
    /// this used to only push on `ProposalOutcome::Added`, matching `added_stages`'
    /// own (correct, for a different reason) growth rule -- but that meant a human
    /// trying to *fix* an already-live unbounded role by re-proposing the exact same
    /// `stage_id` with a real `price_ceiling` this time got a genuine `200`
    /// (`AlreadyPresent` -- the role's own service/tag really is unchanged) while
    /// the fix itself was silently discarded: no_price_ceiling kept citing the
    /// *first* matching entry, so the risk stayed flagged with stale evidence
    /// forever, with no way to ever resolve it through the real proposal mechanism.
    /// Now pushed on every real `apply_proposal` call regardless of outcome --
    /// `no_price_ceiling` reads the *last* matching entry per `stage_id`, so a
    /// later, better proposal actually supersedes an earlier bad one, the same
    /// "latest real state wins" discipline this project already applies elsewhere
    /// (`preflight.rs`'s own doc comment on why several checks scan all of history,
    /// not just the newest entry, for a *different* reason -- this is its mirror:
    /// here the newest real statement of intent is the one that should win).
    #[serde(default)]
    pub approved_stage_proposals: Vec<crate::StageProposal>,
    /// This run's own bounded-loop criteria -- starts at [`AbortCriteria::default`] but
    /// a human can tune it per run (e.g. a run that's earned trust doesn't need a
    /// check-in every 5 iterations). `#[serde(default)]` so `state.json` files written
    /// before this field existed still load, falling back to the same defaults every
    /// run used to be hardcoded to.
    #[serde(default)]
    pub criteria: AbortCriteria,
    /// "Stop, let me look at this" -- operator feedback: "ich weiss nicht... wie ich
    /// es anhalten kann um es zu korrigieren." While `true`, `iterate_run` refuses
    /// new iterations with a real `409` instead of silently accepting them, the one
    /// real gate every real trigger below shares.
    ///
    /// **Real gap in this comment's own earlier claim, corrected 2026-08-06
    /// (stress-test run 47)**: this used to say `paused` was "set/cleared only by a
    /// human action... never by `run_iteration` itself," true only for the direct
    /// pause/resume API and already stale the moment [`toggle_milestone`] started
    /// auto-pausing on achievement. It's now genuinely false a second way too:
    /// `run_iteration` itself sets this on a real `Abort` (hitting the run's own
    /// `max_iterations`/`max_consecutive_failures`) -- confirmed live before that fix
    /// existed that `RunOutcome::Abort` was purely advisory, letting a run accept
    /// real iterations forever past its own configured bound. Three real triggers
    /// set it today: a human's own direct pause, [`toggle_milestone`] on achievement,
    /// and `run_iteration` on a real abort -- only the first is undone by anything
    /// other than an explicit resume; see each site's own doc comment for which.
    /// `#[serde(default)]` so pre-existing `state.json` files (none paused,
    /// obviously) still load.
    #[serde(default)]
    pub paused: bool,
    /// Real gap named honestly at the moment `run_iteration` gained its own real
    /// abort-pause trigger (2026-08-06, stress-test run 47/48): the paused banner
    /// looked identical whether a milestone was achieved, a human clicked Pause, or
    /// the run genuinely hit its own bound -- three real, different situations with
    /// no way to tell them apart at a glance. Set at every site that sets `paused =
    /// true` (a real, short, human-readable sentence, not a code), cleared back to
    /// `None` whenever `paused` is explicitly cleared via the direct resume API --
    /// `#[serde(default)]` so pre-existing `state.json` files (paused, if at all,
    /// from before this field existed) still load with an honest `None` rather than
    /// a guessed reason.
    #[serde(default)]
    pub pause_reason: Option<String>,
    /// This run's real backlog -- see [`BacklogItem`]. `#[serde(default)]` so
    /// pre-existing `state.json` files (no backlog yet) still load.
    #[serde(default)]
    pub backlog: Vec<BacklogItem>,
    /// This run's real per-iteration human comments -- see [`IterationComment`].
    /// `#[serde(default)]` so pre-existing `state.json` files (no comments yet)
    /// still load.
    #[serde(default)]
    pub iteration_comments: Vec<IterationComment>,
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
    /// The other real half of gap #4 (#382 goal doc §7.2, "beyond the current fixed
    /// Action enum"): the assistant could already `propose_custom_panel` (add) but
    /// had no path at all to remove an existing one, even though a human always
    /// could. Deliberately NOT a direct action the same way `ToggleBacklogItem` is
    /// -- removing an existing panel is destructive and irreversible (the human's
    /// own Remove button already gets a real confirm() dialog for exactly that
    /// reason), so it needs the same propose-then-approve trust model
    /// `pending_panel_proposals` already established for adding one, not the
    /// "safe, reversible, applies immediately" model the rest of the Action enum
    /// uses. `#[serde(default)]` so pre-existing `state.json` files (none proposed
    /// yet) still load.
    #[serde(default)]
    pub pending_panel_removal_proposals: Vec<PendingPanelRemovalProposal>,
    /// The last real piece of gap #4 (#382 goal doc §7.2): a human could add and
    /// remove a custom panel directly, and the assistant could propose either --
    /// but EDITING an existing panel's title/HTML had no path at all for either
    /// of them, only remove-then-re-add. A human's own direct edit
    /// (`update_custom_panel`) applies immediately, same trust level as their
    /// own direct Remove button (their own content, their own call) -- but the
    /// assistant's edit needs the same propose-then-approve gate as
    /// `pending_panel_removal_proposals`: overwriting a panel's real content is
    /// exactly as irreversible as removing it (the old title/HTML isn't kept
    /// anywhere once approved), so it gets the same "propose it, a human
    /// approves the actual overwrite" trust model, not `ToggleBacklogItem`'s
    /// safe/reversible/immediate one. `#[serde(default)]` so pre-existing
    /// `state.json` files (none proposed yet) still load.
    #[serde(default)]
    pub pending_panel_edit_proposals: Vec<PendingPanelEditProposal>,
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
    /// "Stack mode" slice 3 (operator ask, 2026-08-06): a real, editable draft
    /// next-iteration-plan option `devsystem.assistant` proposed at a
    /// checkpoint -- the operator's own explicit ask, verbatim intent: "the
    /// devsystem.assistant should be asked to add first drafts, that the user
    /// can delete, change and manipulate. I must be guided what is changed."
    /// Deliberately NOT a propose-then-approve queue like
    /// `pending_stage_proposals`/`pending_panel_proposals` -- a draft doesn't
    /// itself DO anything to live state, it's advisory text a human reads,
    /// edits, or discards, so there is no "apply" step to gate; edit/remove
    /// are direct human actions, the same trust level `ToggleBacklogItem`
    /// already gets. `#[serde(default)]` so pre-existing `state.json` files
    /// (none proposed yet) still load.
    #[serde(default)]
    pub pending_next_step_drafts: Vec<PendingNextStepDraft>,
    /// §7.2 gap #2's newest closed instance (#382 goal doc, 2026-08-07):
    /// re-auditing every human-editable field found a human can already
    /// delete a whole run (the Runs panel's own delete button, gated by a
    /// real `confirm()` -- "there's no undo"), but the assistant had no path
    /// at all, direct or gated. Deliberately NOT a direct action -- unlike
    /// `pause`/`resume` (fully reversible, the human's own button gets zero
    /// extra confirmation), deleting a run is exactly as destructive and
    /// irreversible as removing a custom panel, so it gets the identical
    /// propose-then-approve trust model `pending_panel_removal_proposals`
    /// already established, not `SetPaused`'s "safe, reversible, applies
    /// immediately" one. An `Option`, not a `Vec` like the panel-proposal
    /// queues -- there is only ever one real run to propose deleting, so at
    /// most one pending proposal is ever meaningful; a second `propose` call
    /// replaces the first rather than accumulating a queue of redundant
    /// requests. `#[serde(default)]` so pre-existing `state.json` files
    /// (none proposed yet) still load.
    #[serde(default)]
    pub pending_delete_run_proposal: Option<PendingDeleteRunProposal>,
    /// This run's real, structured requirements -- see [`Requirement`].
    /// `#[serde(default)]` so pre-existing `state.json` files (none defined
    /// yet) still load.
    #[serde(default)]
    pub requirements: Vec<Requirement>,
    /// Real, operator-directed feature request (CADS-devsystem issue #56):
    /// "the assistant should proactively propose additional requirements
    /// that round out coverage... user should be able to accept, edit, or
    /// reject each proposal individually before anything is actually
    /// added." First slice -- propose/approve/reject, same trust model as
    /// every other `pending_*_proposals` queue in this file (a proposal
    /// never appears in the real `requirements` list, never affects the
    /// review gate or coverage, until a human approves it here). The
    /// guided, interview-style dialog issue #56 also asks for is real,
    /// separate, larger UI work, not attempted in this same increment.
    /// `#[serde(default)]` so pre-existing `state.json` files (none
    /// proposed yet) still load.
    #[serde(default)]
    pub pending_requirement_proposals: Vec<PendingRequirementProposal>,
    /// Real, running totals of `devsystem.assistant`'s own real token/cost usage
    /// (#382 goal doc §7.3, gap #5) -- every `/ask` call already parses this
    /// exact data from the LLM CLI's own JSON output (`devsystem_assistant.rs`'s
    /// `parse_llm_json_output`), but until now it was returned to the caller once
    /// and never persisted anywhere, so there was no way to see this run's real
    /// cumulative spend. A running aggregate, not a per-call log: the goal here is
    /// "how much has this run's assistant usage cost so far", not a full replay
    /// history (that already exists, informally, in each chat exchange). `#[serde(default)]`
    /// so pre-existing `state.json` files (no usage recorded yet) still load as
    /// all-zero, not a migration.
    #[serde(default)]
    pub assistant_usage: AssistantUsageTotals,
    /// Real, bounded chat-exchange history (#382 goal doc §4.2, gap #6 --
    /// "the assistant's own chat exchanges aren't pulled in yet"). Until now
    /// a real `/ask` exchange lived nowhere durable server-side once the
    /// response reached the caller -- `assistant_usage`'s own doc comment
    /// assumed a full replay "already exists, informally, in each chat
    /// exchange", but that was the browser's own ephemeral chat window, not
    /// persisted state: closing the tab lost it for good. Bounded and
    /// rolling (oldest entries drop once full, via
    /// [`push_chat_exchange`]) -- this accumulates passively on every real
    /// `/ask` call, unlike a milestone/backlog item a human explicitly adds,
    /// so a hard reject-past-cap (this crate's usual defensive-cap pattern)
    /// would be the wrong shape here. `#[serde(default)]` so pre-existing
    /// `state.json` files (no history recorded yet) still load as empty.
    #[serde(default)]
    pub chat_history: Vec<ChatExchange>,
    /// Real gap found live 2026-08-07: the `checkin_every` cadence firing
    /// (`RunOutcome::CheckinDue`) was purely a one-time HTTP response value and a
    /// GUI toast shown right after the triggering `iterate` call -- nothing durable
    /// recorded that a check-in had actually become due. The moment a human missed
    /// that toast (a different tab, a page reload, coming back later, another
    /// session entirely), `run_health`'s own `iterations_until_checkin` silently
    /// reset to the *full* `checkin_every` value again (confirmed live against the
    /// real `webconference-android` run right after its own iteration 15 crossed
    /// this exact boundary: `iterations_until_checkin: 5`, `needs_attention:
    /// false` -- indistinguishable from a run that simply hadn't reached its next
    /// cadence point yet), silently clearing `needs_attention`'s own `<= 1` signal
    /// at the exact moment it should have been most true. The whole mandatory
    /// check-in mechanism (goal doc §8's "periodic check-ins with the human
    /// owner") was real at the moment it fired and then invisible forever after.
    /// The highest iteration count a human has explicitly acknowledged reviewing
    /// the check-in for, via the real `POST /checkin/acknowledge` endpoint this
    /// fix adds -- `0` means "never acknowledged," matching every pre-existing
    /// `state.json`'s real history honestly (none of them were ever acknowledged
    /// under a mechanism that didn't exist yet). `#[serde(default)]` for the same
    /// reason.
    #[serde(default)]
    pub checkin_acknowledged_through: u32,
    /// Real evaluator finding, issue #42 (suggestion #1, "id-key
    /// `checkin_acknowledged_through` rather than array position"): the field
    /// above is a bare positional integer into `history` -- issue #38's own
    /// live incident showed a history repair (compacting out a duplicate
    /// record) silently re-points every such position, and nothing recorded
    /// that it happened. `IterationRecord::id` (issue #38/#52) now gives every
    /// record real, stable identity; this captures the id of the record that
    /// was actually being acknowledged at `POST /checkin/acknowledge` time,
    /// alongside the position, not instead of it -- the cadence math in
    /// [`checkin_pending`] genuinely needs a count, but a count alone can't
    /// answer "does this watermark still point at the record a human actually
    /// looked at?" after a future history mutation. See
    /// `preflight::checkin_watermark_identity_drift`, the real, mechanical
    /// check built on this field. `None` for every pre-existing acknowledgment
    /// (recorded before this field existed, or a legacy `state.json` with no
    /// id-bearing history at that position) -- honestly absent, never guessed.
    #[serde(default)]
    pub checkin_acknowledged_through_id: Option<String>,
    /// Real evaluator finding, issue #41 (suggestion #2, "small: give Acknowledge
    /// check-in an optional free-text note, persisted next to the watermark"):
    /// `checkin_acknowledged_through` alone recorded THAT a human looked, never
    /// what they said -- and the check-in document's own `## Decision needed`
    /// section explicitly asks the reader for "your answer/direction," with
    /// nowhere in the web panel to give one. This is real, append-only history
    /// (never overwritten), not a single latest-note field -- the same discipline
    /// every other real record in this file already uses. `#[serde(default)]`
    /// for the same reason `checkin_acknowledged_through` is: no pre-existing
    /// `state.json` was ever written under a mechanism that didn't exist yet.
    #[serde(default)]
    pub checkin_notes: Vec<CheckinNote>,
    /// Real operator ask, verbatim intent, 2026-08-07: "ein echtes Plan Canvas
    /// panel: review plans by pointing, not retyping" -- the plan-stage
    /// human-review gate this project's own architecture has named since #382's
    /// very first framing (ECC's `ecc-plan-canvas`), never actually built as a
    /// real panel until now. `ecc-plan-canvas` itself is a real, working tool
    /// (annotate-an-element-and-approve, CLI+JSON loop) but a fundamentally
    /// different shape: a single loopback-only local server keyed by file path,
    /// no concept of runs, ownership, or multiple concurrent reviewers at all --
    /// incompatible with this platform's own real multi-tenant, owner-scoped
    /// run model. Rebuilt natively instead: the same real "point at the thing
    /// you mean" UX, backed by this run's own real state like everything else,
    /// not a separate process/file/port to deploy and keep alive.
    ///
    /// A real, pending annotation anchored to a specific block of the run's
    /// most recent `devsystem.plan` iteration's own feedback text -- exists
    /// only between when a reviewer points at something and when they deliver
    /// a verdict (`POST .../plan-canvas/verdict`): `approve` clears this list
    /// (the session concluded, folded into a real `devsystem.review` iteration
    /// instead); `request_changes` deliberately leaves it populated, so the
    /// exact things pointed at stay visible to the plan's own next author, not
    /// only summarized into free prose. `#[serde(default)]` so pre-existing
    /// `state.json` files (no annotations recorded under a mechanism that
    /// didn't exist yet) still load as empty.
    #[serde(default)]
    pub plan_canvas_annotations: Vec<PlanCanvasAnnotation>,
    /// Real evaluator finding, issue #39: "an iteration that needs an operator
    /// decision can only shout it in free-text backlog prose... nothing
    /// indexes it, nothing summarises it, nothing waits for it." `state` had
    /// seven structured channels for "the pipeline wants to do something and
    /// needs signed off" (`pending_stage_proposals` and its five siblings
    /// above) but none for the inverse, far more common case: "the pipeline
    /// cannot decide and needs answered." This is that channel, close to the
    /// exact shape the issue itself proposed. See [`PendingDecision`].
    /// `#[serde(default)]` so pre-existing `state.json` files (none asked
    /// yet) still load.
    #[serde(default)]
    pub pending_decisions: Vec<PendingDecision>,
}

/// See [`RunState::checkin_notes`]'s own doc comment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckinNote {
    /// The real `checkin_acknowledged_through` value this note was recorded
    /// against -- which check-in it's actually answering, not just a loose
    /// timestamp next to unrelated history.
    pub iteration: u32,
    /// Same real-identity pairing as `RunState::checkin_acknowledged_through_id`
    /// (issue #42, suggestion #1) -- the id of the record `iteration` named at
    /// the moment this note was written, so a later reader can tell whether
    /// `iteration`'s own position still means what it meant then. `None` for
    /// notes recorded before this field existed, or against a legacy position
    /// with no id-bearing record.
    #[serde(default)]
    pub iteration_id: Option<String>,
    pub note: String,
    /// Real, gate-verified identity (`X-Gate-Email`), honestly `None` for a
    /// header-less acknowledgment -- same convention as `confirmed_by`/`created_by`.
    pub acknowledged_by: Option<String>,
    pub acknowledged_at: u64,
}

/// See [`RunState::plan_canvas_annotations`]'s own doc comment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanCanvasAnnotation {
    pub id: String,
    /// A real, short excerpt of the plan text the reviewer actually pointed
    /// at -- not a CSS selector (this run's plan text isn't a fixed DOM
    /// structure across reloads the way a static artifact file is), a real
    /// snippet of the paragraph/block itself, so the annotation still means
    /// something even if the underlying plan iteration is long gone from view.
    pub anchor_snippet: String,
    pub text: String,
    pub created_at: u64,
}

/// See [`RunState::pending_decisions`]'s own doc comment. Distinct from
/// [`BacklogItem`] (a "still needs doing" task, checked off and effectively
/// removed from view) -- this is a genuine yes/no/free-text product question
/// the run cannot resolve on its own, and stays in this list, WITH its
/// answer, once answered, rather than disappearing. Same append-only,
/// never-silently-repointed discipline issue #38/#42 already forced onto
/// `history`: a decision is looked up by its stable `id`, never by position,
/// and answering only ever fills in `answer`/`answered_at`/`answered_by` in
/// place.
///
/// `asked_by_iteration_id` pairs the position with the record's own stable id
/// at the moment this decision was raised -- the exact same "position alone
/// can't survive a future history mutation" lesson issue #42 already forced
/// onto `RunState::checkin_acknowledged_through_id`, applied here before a
/// second real incident of the same class has to teach it again.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingDecision {
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub options: Option<Vec<String>>,
    pub asked_by_iteration: u32,
    #[serde(default)]
    pub asked_by_iteration_id: Option<String>,
    pub asked_at: u64,
    #[serde(default)]
    pub answer: Option<String>,
    #[serde(default)]
    pub answered_at: Option<u64>,
    #[serde(default)]
    pub answered_by: Option<String>,
}

/// True when this run has genuinely crossed a real `checkin_every` boundary (or
/// hit the `max_iterations` ceiling) that hasn't been explicitly acknowledged
/// since -- the real, persistent signal `iterations_until_checkin` alone cannot
/// give (see [`RunState::checkin_acknowledged_through`]'s own doc comment for
/// why). `checkin_every: 0` has no real boundary to cross (mirrors
/// `should_checkin`'s own identical fallback), so this is always `false` then --
/// the hard `max_iterations` ceiling is still a real, separate, already-visible
/// signal via `iterations_until_ceiling`.
pub fn checkin_pending(state: &RunState) -> bool {
    let completed = state.history.len() as u32;
    let every = state.criteria.checkin_every;
    if every == 0 || completed == 0 {
        return false;
    }
    let last_boundary = (completed / every) * every;
    last_boundary > 0 && last_boundary > state.checkin_acknowledged_through
}

/// See [`RunState::chat_history`]'s own doc comment for why this is rolling,
/// not a hard cap.
pub const MAX_CHAT_HISTORY: usize = 50;

/// One real `/ask` round-trip: what a human (or the GUI on their behalf)
/// actually asked, and what `devsystem.assistant` actually said back --
/// `Action`s dispatched are already visible in the `response` text itself
/// (`render_reply_with_action_results`'s own "Actions taken:" section), so
/// this doesn't duplicate that separately.
///
/// `requirement_indices` (#382 goal doc §4.2, gap #6's own "still open" note,
/// closed 2026-08-06): the real, honest form of per-requirement chat
/// attribution the doc comment on that gap's third slice said would need
/// "either a fragile text-match heuristic or a real schema change... both
/// risk showing a WRONG decision basis." Neither risk applies to what this
/// actually is: `devsystem_assistant.rs`'s own `ask()` already holds the real,
/// structured `Action`s it dispatched (`ToggleRequirement`/
/// `ToggleAcceptanceCriterion`, the only two variants that carry an *existing*
/// requirement's real index) at the exact moment it renders a reply -- this
/// records exactly those indices, not a guess from parsing prose or matching
/// keywords. `AddRequirement` deliberately never contributes an index here:
/// its own new requirement's final position is a server-assigned append, not
/// something the bridge can know without a second round-trip, and guessing
/// would reintroduce the exact "might attribute the wrong requirement" risk
/// this was built to avoid. `#[serde(default)]` so chat history recorded
/// before this field existed still loads as an empty (unattributed) list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatExchange {
    pub instruction: String,
    pub response: String,
    pub at: u64,
    #[serde(default)]
    pub requirement_indices: Vec<usize>,
}

/// Appends one real exchange to `state.chat_history`, dropping the oldest
/// once past [`MAX_CHAT_HISTORY`] -- a rolling window, not a hard reject,
/// since this accumulates passively rather than from an explicit "add"
/// action a human could instead be told to stop doing.
pub fn push_chat_exchange(state: &mut RunState, instruction: String, response: String, at: u64, requirement_indices: Vec<usize>) {
    state.chat_history.push(ChatExchange { instruction, response, at, requirement_indices });
    if state.chat_history.len() > MAX_CHAT_HISTORY {
        let overflow = state.chat_history.len() - MAX_CHAT_HISTORY;
        state.chat_history.drain(0..overflow);
    }
}

/// See [`RunState::assistant_usage`]'s doc comment. `total_cost_usd` is a plain
/// running sum of the LLM CLI's own real per-call `total_cost_usd` -- not derived
/// or estimated from token counts, since the actual provider-billed cost isn't a
/// pure function of token counts alone (cache-read pricing differs from
/// cache-write, etc.) and the CLI already reports the real number directly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub struct AssistantUsageTotals {
    pub call_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub total_cost_usd: f64,
}

impl AssistantUsageTotals {
    /// Adds one real call's usage (as `devsystem_assistant`'s own `/ask` response
    /// shape: `{"input_tokens", "output_tokens", "cache_creation_input_tokens",
    /// "cache_read_input_tokens", "total_cost_usd"}`) to this run's running
    /// totals. Missing/non-numeric fields count as zero rather than failing --
    /// usage accounting is real but deliberately best-effort, since it must never
    /// be the reason a real assistant reply fails to reach the caller.
    pub fn add_call(&mut self, usage: &serde_json::Value) {
        let tok = |field: &str| usage.get(field).and_then(|v| v.as_u64()).unwrap_or(0);
        self.call_count += 1;
        self.input_tokens += tok("input_tokens");
        self.output_tokens += tok("output_tokens");
        self.cache_creation_input_tokens += tok("cache_creation_input_tokens");
        self.cache_read_input_tokens += tok("cache_read_input_tokens");
        self.total_cost_usd += usage.get("total_cost_usd").and_then(|v| v.as_f64()).unwrap_or(0.0);
    }
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

/// See [`RunState::pending_next_step_drafts`]'s doc comment. No `approved`
/// flag and no separate live-vs-pending type split like
/// [`PendingPanelProposal`] -- there's nothing to "install," a draft is
/// exactly what it says: text a human can edit or remove, kept in one place.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingNextStepDraft {
    pub id: String,
    pub text: String,
    pub proposed_at: u64,
    /// Mirrors [`PendingRequirementProposal::triggered_by`]'s own doc comment and
    /// reasoning (#382 goal doc §7/§8 DAU-lens, 2026-08-10) -- automode's initial-
    /// proposals instruction can produce a `propose_next_step` just as easily as a
    /// `propose_requirement`, so this needed the same real traceability, not left
    /// as a known analogous gap. `Some("automode: <requirement statement>")` when
    /// this draft appeared as a direct result of an automode-trigger call; `None`
    /// for an ordinary chat-triggered draft. `#[serde(default)]` for the same
    /// backward-compat reason.
    #[serde(default)]
    pub triggered_by: Option<String>,
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

/// See [`RunState::pending_panel_removal_proposals`]'s doc comment. `panel_title`
/// is snapshotted at proposal time (not looked up live from `custom_panels`) so
/// the GUI can render a real, meaningful label even in the same response that's
/// about to remove the panel it refers to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingPanelRemovalProposal {
    pub id: String,
    pub panel_id: String,
    pub panel_title: String,
    pub proposed_at: u64,
}

/// See [`RunState::pending_panel_edit_proposals`]'s doc comment. `old_title` is
/// snapshotted at proposal time (same reasoning as `PendingPanelRemovalProposal`'s
/// `panel_title`) so the GUI can render a real "X -> Y" label without a second
/// lookup into `custom_panels`, which could itself have changed or vanished by
/// the time a human reviews the proposal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingPanelEditProposal {
    pub id: String,
    pub panel_id: String,
    pub old_title: String,
    pub new_title: String,
    pub new_html: String,
    pub proposed_at: u64,
}

/// See [`RunState::pending_delete_run_proposal`]'s doc comment. `rationale` is
/// required (unlike a panel removal, which is self-explanatory) -- a run
/// disappearing for good deserves a real, stated reason a human can weigh,
/// not just a bare id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingDeleteRunProposal {
    pub id: String,
    pub rationale: String,
    pub proposed_at: u64,
}

/// See [`RunState::pending_requirement_proposals`]'s own doc comment. Shape
/// mirrors [`Requirement`]'s own `statement`/`acceptance_criteria` pair
/// exactly -- approving one becomes a real `Requirement` verbatim, no
/// re-typing -- plus a `rationale` (why this requirement rounds out real
/// coverage), the same "not just the what, the why" convention
/// `StageProposal::rationale` already established for pipeline stages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRequirementProposal {
    pub id: String,
    pub statement: String,
    pub acceptance_criteria: Vec<String>,
    pub rationale: String,
    pub proposed_at: u64,
    /// #382 goal doc §7/§8 DAU-lens finding, 2026-08-10: an automode-triggered proposal
    /// (issue #31's "toggling ON fires an initial-proposals call") used to look
    /// identical to any other assistant proposal -- no structural link back to the
    /// toggle that caused it, only whatever prose the LLM happened to write in its own
    /// `rationale`. A user who toggles automode on then quickly off again (e.g. testing
    /// what it does) sees a proposal appear later with no visible connection to an
    /// action they believe they already cancelled -- relying on an LLM to remember to
    /// mention "this is from automode" in free text is exactly the kind of unenforced
    /// guidance this methodology exists to harden into a real guarantee instead.
    /// `Some("automode: <requirement statement>")` when this proposal was one of the
    /// ones that appeared as a direct result of an automode-trigger call (set by
    /// `trigger_automode_initial_proposals`, never client-supplied); `None` for every
    /// ordinary chat-triggered proposal, unaffected. `#[serde(default)]` so a run
    /// persisted before this field existed still deserializes.
    #[serde(default)]
    pub triggered_by: Option<String>,
}

impl RunState {
    pub fn new(run_id: impl Into<String>) -> Self {
        RunState {
            run_id: run_id.into(),
            consecutive_failures: 0,
            history: Vec::new(),
            added_stages: Vec::new(),
            approved_stage_proposals: Vec::new(),
            criteria: AbortCriteria::default(),
            paused: false,
            pause_reason: None,
            backlog: Vec::new(),
            iteration_comments: Vec::new(),
            milestones: Vec::new(),
            repo_url: None,
            owner_email: None,
            role_fill_modes: std::collections::HashMap::new(),
            custom_panels: Vec::new(),
            pending_panel_proposals: Vec::new(),
            pending_panel_removal_proposals: Vec::new(),
            pending_panel_edit_proposals: Vec::new(),
            pending_stage_proposals: Vec::new(),
            pending_issue_proposals: Vec::new(),
            pending_next_step_drafts: Vec::new(),
            pending_delete_run_proposal: None,
            requirements: Vec::new(),
            pending_requirement_proposals: Vec::new(),
            assistant_usage: AssistantUsageTotals::default(),
            chat_history: Vec::new(),
            checkin_acknowledged_through: 0,
            checkin_acknowledged_through_id: None,
            checkin_notes: Vec::new(),
            plan_canvas_annotations: Vec::new(),
            pending_decisions: Vec::new(),
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
    let just_achieved = !was_achieved && milestone.achieved;
    let description = milestone.description.clone();
    // Real evaluator finding, issue #50: this used to set `pause_reason`
    // unconditionally on every achieve-transition, even when the run was
    // ALREADY paused for a genuine safety abort (max_iterations,
    // max_consecutive_failures, a due check-in -- see `ceiling_already_reached`/
    // `run_iteration`'s own real reasons). A milestone's own free text
    // (self-serve, any signed-in account, on any unowned run per #44/#45's fail-
    // open write path) would silently overwrite and permanently lose the real
    // reason, and un-achieving never restored it -- an operator reading "milestone
    // achieved: ..." had no way to know the run was actually halted on its
    // consecutive-failure budget. Only ever set a NEW reason when the run
    // genuinely wasn't already paused -- the first real reason a run halts for
    // wins and stays visible; a milestone reached while already correctly halted
    // doesn't get to relabel why.
    if just_achieved && !state.paused {
        state.paused = true;
        state.pause_reason = Some(format!("milestone achieved: {description}"));
    }
    Ok(())
}

/// Unlike [`toggle_milestone`], toggling a requirement's `verified` flag never
/// auto-pauses the run -- a requirement is a standing behavioral contract, not
/// a one-time checkpoint; marking it verified (or un-verifying it after a
/// regression) is routine bookkeeping, not an event that should interrupt the
/// loop.
///
/// **Real, mandatory quality gate** (#382 goal doc §5/§8, gap #2 -- "it is the
/// fault of the pipeline, not the user, if the process leads them not to the
/// perfect result"): a requirement can only be marked verified (false -> true)
/// if this run's own spec declares a `review` role AND a real
/// `devsystem.review` iteration that `succeeded` and named this requirement in
/// its `requirement_indices` already exists in history. This is a hard block,
/// not an advisory annotation like `preflight`'s risk findings -- a role-filler
/// (competent or not) cannot simply mark its own work done without a real
/// review having actually addressed it.
///
/// Scoped to runs that declare `review` as a role at all: `plan_only_spec` (what
/// every new run starts as) has no such role, so this never blocks a run that
/// hasn't opted `review` into its own pipeline -- there is nothing to gate
/// against for a stage that was never declared. Un-verifying (true -> false) is
/// always allowed unconditionally -- loosening a claim never needs a review to
/// justify it.
///
/// **Real gap found and closed by the incompetent-agent stress test itself**
/// (#382 goal doc §8, 2026-08-05): the gate above only checked that a
/// `devsystem.review` iteration *existed* and `succeeded` -- a real, live test
/// against this exact gate proved a completely lazy rubber-stamp
/// (`feedback: "looks fine to me"`) satisfied it just as well as real scrutiny
/// would have. That's precisely the failure mode the goal doc's own governing
/// principle names: the pipeline let a bad outcome through, not the reviewer's
/// fault to have written a short answer -- the gate's fault for not checking.
/// `MIN_REVIEW_FEEDBACK_LEN` is a deliberately crude, honestly-scoped mechanical
/// proxy (this codebase's own established convention -- see `preflight.rs` --
/// is simple, explainable checks, never fake LLM-judgment-in-disguise): a review
/// under this length cannot plausibly be real scrutiny of a specific
/// requirement's specific acceptance criteria. It does **not** verify the
/// review is actually good, only that it isn't trivially empty.
///
/// **The exact "longer but still-lazy" gap the goal doc named, closed for real,
/// live-verified before this fix and again after**: a real POST against this
/// gate with `feedback: "looks good looks good looks good looks good"` (45
/// characters, well past the length bar) got a real `200` and marked the
/// requirement verified -- length alone can't tell real scrutiny from padded
/// filler repeating the same few words. `MIN_REVIEW_DISTINCT_WORDS` adds a
/// second, complementary mechanical proxy: a real review of a specific
/// requirement's specific acceptance criteria uses more than a handful of
/// distinct words, even a short one. Both bars must clear -- length alone
/// passed "looks good looks good..."; distinct-word-count alone would pass a
/// single very long repeated word. Still an honestly crude proxy, not real
/// judgment: a generic-but-varied review ("looks good, works fine, nothing to
/// flag, all clear here") clears both bars without being real scrutiny either
/// -- that remaining gap is noted in the goal doc, not claimed solved here.
const MIN_REVIEW_FEEDBACK_LEN: usize = 25;
const MIN_REVIEW_DISTINCT_WORDS: usize = 8;

/// The exact "generic-but-varied" gap this gate's own comment above named as
/// still open ("a generic-but-varied review ('looks good, works fine,
/// nothing to flag, all clear here') clears both bars without being real
/// scrutiny either"): live-verified before this fix, that literal phrase
/// (10 distinct words, 57 characters) got a real `200` against the deployed
/// gate -- length and distinct-word-count alone can't tell varied filler from
/// real per-requirement scrutiny. A third, complementary mechanical proxy,
/// same crude-but-explainable discipline as `SECURITY_KEYWORDS`/
/// `DEFECT_ADMISSION_PHRASES` in `preflight.rs`: a curated list of generic
/// praise/filler words and common English function words. Honest, named
/// limitation, not claimed comprehensive -- a lazy reviewer who avoids every
/// word on this list while still saying nothing requirement-specific can
/// still slip through; this closes the exact phrasing the goal doc itself
/// used as its worked example, not every possible generic review.
const GENERIC_REVIEW_WORDS: &[&str] = &[
    "a", "an", "the", "to", "of", "in", "on", "for", "and", "or", "with", "this", "that", "it", "its", "as", "be",
    "by", "at", "is", "are", "was", "were", "no", "not", "good", "fine", "great", "nice", "ok", "okay", "correct",
    "perfect", "solid", "excellent", "clear", "clean", "nothing", "none", "flag", "flags", "everything", "all",
    "here", "there", "looks", "look", "looking", "works", "work", "working", "done", "complete", "completed",
    "lgtm", "issue", "issues", "seems", "seem", "appears", "appear",
];

/// Distinct alphanumeric words in `text`, excluding [`GENERIC_REVIEW_WORDS`] --
/// what's left after stripping generic praise and common function words. A
/// review that's varied and long purely because of filler like "looks good,
/// works fine, nothing to flag, all clear here" has zero words left; real
/// scrutiny of a specific requirement leaves real, specific vocabulary behind.
fn specific_word_count(text: &str) -> usize {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !GENERIC_REVIEW_WORDS.contains(w))
        .collect::<std::collections::HashSet<_>>()
        .len()
}

const MIN_REVIEW_SPECIFIC_WORDS: usize = 4;

/// Case-insensitive distinct alphanumeric "words" in `text` -- the same
/// tokenization spirit as `preflight.rs`'s other mechanical checks (simple,
/// explainable, no fake LLM-judgment-in-disguise). Punctuation-only repeats
/// ("good. good! good?") still collapse to one distinct word, same as
/// whitespace-separated repeats do.
pub(crate) fn distinct_word_count(text: &str) -> usize {
    let mut words: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect();
    words.sort();
    words.dedup();
    words.len()
}

/// Real gap found live by the stress test, same day, right after the padded-
/// review fix shipped: a review's real, substantive-looking feedback about ONE
/// requirement can be copied verbatim and reused to "review" a completely
/// unrelated requirement -- both the length and distinct-word bars pass
/// trivially, since the text itself genuinely is long and varied, just not
/// actually about the requirement it's being applied to. Live-verified before
/// this fix: reused the real feedback from the device-rotation requirement's
/// own review, named a completely unrelated network-retry requirement instead,
/// got a real `200`.
fn same_requirement_set(a: &[usize], b: &[usize]) -> bool {
    let a: std::collections::HashSet<_> = a.iter().collect();
    let b: std::collections::HashSet<_> = b.iter().collect();
    a == b
}

/// The real evidence bar gap #2's mandatory review gate applies once a run
/// declares `review` (see `toggle_requirement` below) -- pulled out into its
/// own function so gap #10's assistant-specific gate
/// (`web/src/main.rs`'s `toggle_requirement_handler`) can require the
/// identical real evidence **unconditionally**, on any run, regardless of
/// whether that run happens to have declared `review` at all. Same three
/// real bars, byte-identical logic to what this function replaced inline:
/// a qualifying `devsystem.review` iteration must exist, have succeeded,
/// name this exact requirement, and clear the length/distinct-word/
/// not-reused-elsewhere bars.
pub fn qualifying_review_evidence(state: &RunState, index: usize) -> Result<(), String> {
    // Every qualifying review (right requirement, succeeded), not just the first
    // or the last -- an early lazy rubber-stamp must not poison a genuinely
    // substantive review submitted afterward, and a later lazy one must not
    // undo an earlier real review either. The gate passes if AT LEAST ONE
    // qualifying review clears the length bar.
    let reviews: Vec<&IterationRecord> =
        state.history.iter().filter(|h| h.stage == "devsystem.review" && h.succeeded && h.requirement_indices.contains(&index)).collect();
    if reviews.is_empty() {
        return Err(format!(
            "requirement {index} cannot be marked verified yet -- no successful devsystem.review \
             iteration addressing requirement {index} (via its requirement_indices) exists yet. \
             Submit one first."
        ));
    }
    // The stress test's fifteenth real run (#382 goal doc §8, 2026-08-06): a
    // single review iteration can name an arbitrary number of requirements at
    // once via requirement_indices, but the length/distinct-word bars were
    // fixed constants regardless -- live-verified before this fix, one
    // generic "reviewed all of these, everything looks correct" iteration
    // (21 distinct words, comfortably clearing the flat 8-word bar) named
    // five completely unrelated requirements at once and satisfied the gate
    // for every one of them. The bar now scales with how many requirements a
    // single review claims to cover -- the same real per-requirement bar
    // applies that many times over, not once for the whole batch. A
    // genuinely thorough multi-requirement review naturally clears this (real
    // per-requirement observations accumulate real distinct content); a
    // shotgun "LGTM, checked everything" does not.
    let long_enough = |r: &&IterationRecord| {
        let trimmed = r.feedback.trim();
        let claimed = r.requirement_indices.len().max(1);
        trimmed.chars().count() >= MIN_REVIEW_FEEDBACK_LEN * claimed && distinct_word_count(trimmed) >= MIN_REVIEW_DISTINCT_WORDS * claimed
    };
    let reused_verbatim_elsewhere = |r: &&IterationRecord| {
        let trimmed = r.feedback.trim();
        state.history.iter().any(|other| {
            other.stage == "devsystem.review"
                && other.succeeded
                && other.feedback.trim() == trimmed
                && !same_requirement_set(&other.requirement_indices, &r.requirement_indices)
        })
    };
    let specific_enough = |r: &&IterationRecord| {
        let claimed = r.requirement_indices.len().max(1);
        specific_word_count(r.feedback.trim()) >= MIN_REVIEW_SPECIFIC_WORDS * claimed
    };
    let qualifies = |r: &&IterationRecord| long_enough(r) && !reused_verbatim_elsewhere(r) && specific_enough(r);
    if !reviews.iter().any(qualifies) {
        if reviews.iter().any(|r| long_enough(r) && reused_verbatim_elsewhere(r)) {
            return Err(format!(
                "requirement {index} cannot be marked verified yet -- the devsystem.review iteration \
                 addressing it reuses feedback text verbatim from a review of a different, unrelated \
                 requirement in this run's history. Real scrutiny of THIS requirement's specific \
                 acceptance criteria is required, not a copy-pasted review meant for something else."
            ));
        }
        if let Some(r) = reviews.iter().find(|r| long_enough(r) && !reused_verbatim_elsewhere(r) && !specific_enough(r)) {
            let claimed = r.requirement_indices.len().max(1);
            return Err(format!(
                "requirement {index} cannot be marked verified yet -- the devsystem.review iteration \
                 addressing it is long and varied enough to pass the length/distinct-word bars, but \
                 that's entirely generic praise and filler ('{} distinct non-generic word(s)', minimum \
                 {}) -- it never engages with anything specific to this requirement. A varied-sounding \
                 rubber-stamp doesn't satisfy this gate any more than a short one does.",
                specific_word_count(r.feedback.trim()),
                MIN_REVIEW_SPECIFIC_WORDS * claimed,
            ));
        }
        let best = reviews.iter().max_by_key(|r| (r.feedback.trim().chars().count(), distinct_word_count(r.feedback.trim()))).unwrap();
        let claimed = best.requirement_indices.len().max(1);
        let scale_note = if claimed > 1 {
            format!(
                " (this iteration names {claimed} requirements at once via requirement_indices, so the \
                 real bar for it is {claimed}x the usual minimum -- the same real per-requirement bar \
                 applies that many times over, not once for the whole batch)"
            )
        } else {
            String::new()
        };
        return Err(format!(
            "requirement {index} cannot be marked verified yet -- every devsystem.review iteration \
             addressing it is too short or too repetitive to plausibly be real scrutiny (best is \
             iteration {}, {} character(s) and {} distinct word(s); minimum {} characters AND {} \
             distinct words{scale_note}). A rubber-stamp, padded filler, or generic shotgun review \
             doesn't satisfy this gate.",
            best.iteration,
            best.feedback.trim().chars().count(),
            distinct_word_count(best.feedback.trim()),
            MIN_REVIEW_FEEDBACK_LEN * claimed,
            MIN_REVIEW_DISTINCT_WORDS * claimed,
        ));
    }
    Ok(())
}

pub fn toggle_requirement(spec: &PipelineSpec, state: &mut RunState, index: usize) -> Result<(), String> {
    let requirement = state.requirements.get(index).ok_or_else(|| format!("no requirement at index {index}"))?;
    if !requirement.verified && spec.roles.iter().any(|r| r.tag == "review") {
        qualifying_review_evidence(state, index)?;
    }
    let requirement = state.requirements.get_mut(index).unwrap();
    requirement.verified = !requirement.verified;
    Ok(())
}

/// Toggles a requirement's `auto_judge` opt-in -- see [`Requirement::auto_judge`]'s
/// own doc comment. Purely a permission flag; flipping it never itself changes
/// `verified`/`verified_criteria` -- an owner opting in doesn't retroactively
/// claim anything is now judged, just that judgment is now allowed to happen.
pub fn toggle_requirement_auto_judge(state: &mut RunState, index: usize) -> Result<(), String> {
    let requirement = state.requirements.get_mut(index).ok_or_else(|| format!("no requirement at index {index}"))?;
    requirement.auto_judge = !requirement.auto_judge;
    Ok(())
}

/// Toggles a requirement's `automode` opt-in -- see [`Requirement::automode`]'s own doc
/// comment for exactly what this does and does not do yet (issue #31's honest first
/// slice, same precedent as `toggle_requirement_auto_judge` above). Returns the real new
/// value so the caller can tell a `false -> true` transition apart from a `true -> false`
/// one -- issue #31's own second real slice (initial-proposals-on-enable) needs to fire
/// its one-shot trigger only on the former, never on every toggle.
pub fn toggle_requirement_automode(state: &mut RunState, index: usize) -> Result<bool, String> {
    let requirement = state.requirements.get_mut(index).ok_or_else(|| format!("no requirement at index {index}"))?;
    requirement.automode = !requirement.automode;
    Ok(requirement.automode)
}

/// Toggles a single acceptance criterion's real, human-set verified state --
/// see [`Requirement::verified_criteria`]'s own doc comment for why this is a
/// separate signal from `verified` itself. Grows `verified_criteria` with
/// real `None` entries up to `criterion_index` on demand rather than
/// requiring it to already be the same length as `acceptance_criteria` --
/// every pre-existing requirement (persisted before this field existed, or
/// simply never touched yet) starts effectively "nothing checked" without
/// needing a migration step.
///
/// `confirmed_by` (real evaluator finding, issue #55) is the real, gate-verified
/// identity of whoever is toggling this on -- `None` when no session exists (the
/// local/M2M path, same honesty convention `submitted_by` already established for
/// iterations). `now` is the real, current Unix timestamp -- passed in rather than
/// computed here so this function stays pure and hermetically testable, the same
/// deliberate "no timestamp, no I/O inside the pipeline crate" discipline
/// `render_requirements_markdown` already documents for itself; only used on the
/// transition into `Some` (confirming). Toggling back off clears to `None` outright
/// rather than keeping a stale "who last confirmed this before it was
/// un-confirmed" record around.
pub fn toggle_acceptance_criterion(state: &mut RunState, req_index: usize, criterion_index: usize, confirmed_by: Option<String>, now: u64) -> Result<(), String> {
    let requirement = state.requirements.get_mut(req_index).ok_or_else(|| format!("no requirement at index {req_index}"))?;
    if criterion_index >= requirement.acceptance_criteria.len() {
        return Err(format!("requirement {req_index} has no acceptance criterion at index {criterion_index}"));
    }
    if requirement.verified_criteria.len() <= criterion_index {
        requirement.verified_criteria.resize(criterion_index + 1, None);
    }
    requirement.verified_criteria[criterion_index] = match &requirement.verified_criteria[criterion_index] {
        Some(_) => None,
        None => Some(CriterionVerification { confirmed_by, confirmed_at: Some(now) }),
    };
    Ok(())
}

/// Real gap found live by the incompetent-agent stress test (#382 goal doc §8,
/// 2026-08-06): the exact same "two real entry points, one bug class" shape
/// already found and fixed this session for `validate_proposals`/
/// `validate_feedback` -- `web/src/main.rs`'s `iterate_run` HTTP handler
/// checks `requirement_indices` against `state.requirements.len()` before
/// ever calling [`run_iteration`], but `run_iteration` itself does nothing
/// with the field except silently store it, and `devsystem_iterate`'s local,
/// non-`--remote` CLI path calls `run_iteration` directly with no HTTP layer
/// in between at all. Live-confirmed before this fix: on a real run with
/// zero requirements, the local CLI accepted `requirement_indices: [999,
/// 1000]` with a real `iteration_outcome=Continue` and persisted it
/// permanently -- pure garbage traceability data with no bound checking it
/// whatsoever, the one real validation this session had (until now) only
/// ever wired into the HTTP path. A shared, standalone function (not folded
/// into `run_iteration` itself, matching `validate_feedback`'s own
/// precedent of validating BEFORE constructing/applying the real
/// `IterationRecord`) so every real entry point calls the identical gate --
/// `web/src/main.rs`'s own inline check now calls this instead of keeping a
/// second, separately-maintained copy of the same logic. Reports every
/// out-of-range index in one pass, not just the first, same convention
/// `iterate_run`'s own fix already established.
pub fn validate_requirement_indices(state: &RunState, indices: &[usize]) -> Result<(), String> {
    let bad: Vec<usize> = indices.iter().copied().filter(|&i| i >= state.requirements.len()).collect();
    if bad.is_empty() {
        Ok(())
    } else {
        let bad_list = bad.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
        Err(format!(
            "requirement_indices references out-of-range index(es) [{bad_list}], but state.requirements only has {} entries",
            state.requirements.len()
        ))
    }
}

/// Real gap found live by a non-technical evaluator, issue #46, 2026-08-07: `paused`
/// (checked at the top of `web/src/main.rs`'s `iterate_run`) is the ONLY gate a
/// submission has to clear -- and `POST /resume` unconditionally clears it without
/// re-checking whether the condition that caused the pause is still true. Reaching
/// `max_iterations` correctly pauses the run and correctly refuses the very next
/// submission (`paused` is still `true`) -- but the moment a human clicks Resume,
/// `paused` flips back to `false` with the ceiling never having moved, and the
/// following submission is accepted and durably recorded, `history.len()` now one
/// past the declared bound. Repeat: Resume, submit, re-pause, Resume, submit... a
/// stated "bounded super loop" (this project's own repeated architectural claim,
/// #382 goal doc's own governing principle) was not actually bounded at the one
/// place that matters. Reported live: `iterations: 2 / 1` in the real Health panel.
///
/// This is independent of `paused` on purpose -- checked directly against the run's
/// own current history/failure count and the criteria that (still) apply, so it
/// refuses regardless of how `paused` got cleared, not just the specific Resume path
/// that surfaced it. `Resume` remains the correct action for every OTHER pause
/// reason (a milestone, a manual pause) where the underlying condition genuinely
/// isn't still true the moment the run resumes.
///
/// Real evaluator finding, issue #47 (a follow-up to the fix above, 2026-08-07): this
/// gate's own error message has always promised an escape -- "a real, succeeded
/// iteration is needed to reset the streak before another can be accepted" -- but the
/// gate checked the run's state *before* the incoming submission, with no visibility
/// into what was actually being submitted. Once `consecutive_failures` reached the
/// bound, EVERY subsequent request was refused, including a genuine `succeeded: true`
/// submission that would have reset the streak to `0` -- the exact remedy the message
/// itself names. Live-confirmed the real deadlock: `consecutive_failures: 1`,
/// `max_consecutive_failures: 1`, a `succeeded: true` resubmission got the identical
/// `409` as a `succeeded: false` one, and the only way out was editing
/// `max_consecutive_failures` itself -- a door the error text never mentioned as the
/// *only* one that actually opened.
///
/// `incoming_succeeded` is the fix: a real success is let through specifically because
/// applying it is what clears the streak (`run_iteration`'s own `consecutive_failures =
/// 0` on `record.succeeded`) -- letting it land is the resolution, not a bypass. A
/// further `succeeded: false` while already at the bound is still refused outright, so
/// this remains a real ceiling, not a blanket unlock. `max_iterations` has no such
/// escape -- iteration count only ever grows, so it stays blocked regardless of what's
/// being submitted.
pub fn ceiling_already_reached(state: &RunState, criteria: &AbortCriteria, incoming_succeeded: bool) -> Option<String> {
    if state.consecutive_failures >= criteria.max_consecutive_failures && !incoming_succeeded {
        Some(format!(
            "already at {} consecutive failed iteration(s), at or past the configured limit of {} -- \
             raise max_consecutive_failures for this run, or submit a real, succeeded iteration to \
             reset the streak (a succeeded:true submission is let through specifically to clear it)",
            state.consecutive_failures, criteria.max_consecutive_failures
        ))
    } else if state.history.len() as u32 >= criteria.max_iterations {
        Some(format!(
            "already at {} of {} max iterations -- raise max_iterations for this run, or close it out; \
             resuming a run that already reached its ceiling does not raise the ceiling",
            state.history.len(),
            criteria.max_iterations
        ))
    } else {
        None
    }
}

/// Real evaluator finding, issue #39 (suggestion #3, the gating half of the
/// `pending_decisions` channel -- #382 goal doc §8, 2026-08-10). Suggestion #4 ("at
/// least visible on the run badge") shipped the same day (issue #39/#41 comment
/// history, task #97); this is the harder, deliberately-deferred second half: "a run
/// should not be allowed to burn its final iteration with a blocking question
/// outstanding." Real incident this closes: this project's own `webconference-android`
/// run had a genuine open question (offline-delivery support, `state.backlog[7]`
/// before the structured channel existed) sit unanswered while iterations kept landing
/// toward the run's own ceiling -- nothing stopped the very last slot from being
/// consumed with the question still open, after which no further iteration could ever
/// act on whatever the real answer turned out to be.
///
/// Deliberately narrow, matching the issue's own wording ("final iteration", not
/// "every iteration while something is open"): only refuses the ONE submission that
/// would consume the run's last remaining slot
/// (`state.history.len() + 1 == criteria.max_iterations`). An ordinary mid-run
/// decision does not block ordinary progress -- blocking every iteration on every
/// question that comes up would just recreate the "one careless click can't fix a
/// real gap" failure mode this project keeps closing elsewhere, in the opposite
/// direction. Independent of `incoming_succeeded`: a `succeeded: true` final
/// iteration is exactly as unable to act on a still-open answer as a failed one, so
/// unlike `ceiling_already_reached`'s consecutive-failure escape, there is no
/// "let success through" exception here.
pub fn final_iteration_blocked_by_unanswered_decision(state: &RunState, criteria: &AbortCriteria) -> Option<String> {
    if state.history.len() as u32 + 1 != criteria.max_iterations {
        return None;
    }
    let open: Vec<&str> = state.pending_decisions.iter().filter(|d| d.answer.is_none()).map(|d| d.question.as_str()).collect();
    if open.is_empty() {
        return None;
    }
    Some(format!(
        "this would be the run's final iteration ({} of {} max_iterations) while {} real decision(s) remain unanswered -- \
         answer them first (POST /api/runs/{{id}}/decisions/{{decision_id}}/answer), or raise max_iterations before \
         submitting: {}",
        state.history.len() + 1,
        criteria.max_iterations,
        open.len(),
        open.join("; ")
    ))
}

/// A fourth gap of the identical "two real entry points, one bug class" shape (#382
/// goal doc §8, 2026-08-06), deliberately deferred out of the `paused`-check fix
/// (firing ttt) rather than bundled in, since it protects a different and less severe
/// failure mode: `web/src/main.rs`'s `iterate_run` refuses a submission that's
/// byte-identical to the run's own immediately-preceding history entry (real
/// idempotency guard, found necessary live 2026-08-05 -- a same-day window of
/// overlapping `devsystem-web` container instances during a redeploy let two
/// functionally-identical iterations both land with the same computed iteration
/// number), but that check lived inline in the HTTP handler only.
/// `devsystem_iterate`'s local, non-`--remote` CLI path had no equivalent at all --
/// a client retry (or a script accidentally re-running the same `record.json` twice)
/// would silently append a second, indistinguishable history entry rather than being
/// refused. Takes the individual fields rather than a shared record/request type
/// since the HTTP handler's own check runs *before* it constructs its `IterationRecord`
/// (it only has the raw request body fields at that point), while the local CLI's own
/// `record` already exists in full by the time this runs -- a shared function over the
/// bare fields lets both real call sites use the identical comparison without either
/// one reshaping its own control flow to match the other.
pub fn duplicate_of_last_iteration(
    history: &[IterationRecord],
    stage: &str,
    feedback: &str,
    succeeded: bool,
    proposals: &[crate::StageProposal],
    requirement_indices: &[usize],
) -> Option<u32> {
    let last = history.last()?;
    if last.stage == stage
        && last.feedback == feedback
        && last.succeeded == succeeded
        && last.proposals == proposals
        && last.requirement_indices == requirement_indices
    {
        Some(last.iteration)
    } else {
        None
    }
}

/// The most recent real [`crate::StageProposal`] that shaped a `stage_id`, if any --
/// checking `approved_stage_proposals` first (complete going forward, both real
/// approval paths write to it now), falling back to `history.proposals` only for a
/// `stage_id` with no entry there at all (pre-existing data from before that field
/// existed). A later, real re-proposal for the same `stage_id` supersedes an
/// earlier one, matching `preflight::no_price_ceiling`'s own established "last
/// match wins" fix (#382 goal doc §8, twenty-seventh stress-test run). Used by
/// `no_price_ceiling` itself and by the GUI's `fix_target` (stage_id/tag,
/// #382 goal doc §7). **Not** used by [`price_ceiling_for`] below, which needs a
/// different, more conservative "last one that actually set a real ceiling"
/// search instead -- see its own doc comment for the real gap that distinction
/// closes.
pub fn latest_proposal_for_stage<'a>(state: &'a RunState, stage_id: &str) -> Option<&'a crate::StageProposal> {
    state
        .approved_stage_proposals
        .iter()
        .rev()
        .find(|p| p.stage_id == stage_id)
        .or_else(|| state.history.iter().rev().flat_map(|h| h.proposals.iter().rev()).find(|p| p.stage_id == stage_id))
}

/// The real, currently-enforceable price ceiling for a `stage_id`, if any -- `None`
/// covers both "never proposed with a `price_ceiling`" and "proposed with
/// `price_ceiling: Some(0)`", matching `preflight::no_price_ceiling`'s own explicit
/// reasoning that a real `0` is exactly as unbounded as unset (nothing to actually
/// enforce either way). Used by the real direct-accept enforcement gate
/// (`set_role_fill_mode`, 2026-08-07): a role with a genuine, positive ceiling now
/// actually bounds what a directly-accepted bid can cost, closing the honest gap
/// `no_price_ceiling`'s own doc comment named -- price_ceiling was stored and shown,
/// never compared against anything, anywhere, until this.
///
/// **Deliberately NOT `latest_proposal_for_stage(...).price_ceiling`, found live the
/// same day**: that function's own "last proposal wins" is correct for RISK
/// FLAGGING (a later re-proposal's own current intent is what should be shown as
/// live/unbounded), but wrong for ENFORCEMENT -- a careless later re-proposal that
/// simply omits `price_ceiling` (never claims to remove it, just doesn't mention it)
/// would silently un-bound a role that a real, earlier proposal had genuinely
/// bounded. Live-confirmed before this fix: proposed+approved a role with
/// `price_ceiling: 50`, then a second, careless re-proposal of the identical
/// `stage_id` with NO `price_ceiling` field at all -- a `999`-priced direct-accept
/// that should have stayed blocked got a real `200` instead, even though the risk
/// panel kept correctly showing `no price ceiling set` (that check's own "last
/// wins" is right for flagging, just not for this). Fixed by searching backward
/// through every real proposal for this `stage_id` (approved list first, falling
/// back to history) for the LAST ONE THAT ACTUALLY SET a real ceiling, skipping any
/// later re-proposal that simply didn't address it -- a real, positive ceiling, once
/// genuinely set, stays enforced until a later proposal explicitly sets a different
/// one, never silently by omission.
pub fn price_ceiling_for(state: &RunState, stage_id: &str) -> Option<u64> {
    state
        .approved_stage_proposals
        .iter()
        .rev()
        .filter(|p| p.stage_id == stage_id)
        .find_map(|p| p.price_ceiling.filter(|&c| c > 0))
        .or_else(|| {
            state
                .history
                .iter()
                .rev()
                .flat_map(|h| h.proposals.iter().rev())
                .filter(|p| p.stage_id == stage_id)
                .find_map(|p| p.price_ceiling.filter(|&c| c > 0))
        })
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
        let outcome = apply_proposal(spec, proposal);
        if outcome == ProposalOutcome::Added {
            state.added_stages.push(proposal.stage_id.clone());
        }
        // Real gap found live by the stress test, twenty-seventh run,
        // 2026-08-06 (see RunState::approved_stage_proposals' own doc
        // comment): pushed here regardless of Added vs AlreadyPresent, not
        // just on Added -- a human/filler trying to *fix* an already-live
        // unbounded role by re-proposing the same stage_id with a real
        // price_ceiling this time correctly gets AlreadyPresent from
        // apply_proposal (the role's own service/tag genuinely didn't
        // change), but that must not also silently discard the real, newer
        // price_ceiling information -- see no_price_ceiling's own doc
        // comment for how the "latest wins" read side makes this real.
        state.approved_stage_proposals.push(proposal.clone());
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
        // Real gap found live 2026-08-06 (stress-test run 47): `RunOutcome::Abort` used
        // to be purely advisory -- a string in the HTTP response, nothing more.
        // Live-confirmed the real severity before this fix: with `max_iterations: 2`,
        // iteration 2 correctly reported `"outcome":"Abort"`, but iterations 3 and 4
        // were STILL accepted -- `state.history` grew to 4 real entries, double the
        // configured bound, `paused` never flipped. This project's own central
        // architectural claim ("a bounded super loop," repeated throughout this
        // codebase's own doc comments) was genuinely NOT enforced at the one place
        // that matters. Reuses the exact same `paused` mechanism `toggle_milestone`
        // already established (real GUI banner, disabled New Iteration form, the
        // existing `if run_state.paused { 409 }` check `iterate_run` already runs at
        // the top) -- the next real iterate call is blocked by code that already
        // existed, not new enforcement logic.
        //
        // The "why paused" distinction named still-open above is done now too
        // (`RunState::pause_reason`'s own doc comment, run 49): which real bound was
        // actually hit, checked in the same order `should_abort` itself checks them,
        // so this never contradicts the real condition that fired.
        state.paused = true;
        state.pause_reason = Some(if state.consecutive_failures >= criteria.max_consecutive_failures {
            format!("{} consecutive failed iterations (limit {})", state.consecutive_failures, criteria.max_consecutive_failures)
        } else {
            format!("reached the {}-iteration limit", criteria.max_iterations)
        });
        RunOutcome::Abort
    } else if checkin_check {
        // Real gap found live by a non-technical evaluator, issue #48, 2026-08-07:
        // `RunOutcome::CheckinDue`'s own doc comment has always promised "the run
        // must pause here" -- but nothing ever did. `iterate_run` only ever turned
        // this variant into a string for the JSON response; `state.paused` stayed
        // `false`. Live-confirmed the real severity: with `checkin_every: 1`, six
        // iterations submitted back to back each correctly reported
        // `outcome: "CheckinDue"` and each was accepted and durably recorded anyway
        // -- a "mandatory" human check-in that never actually stopped anything, in
        // sharp contrast to the real, hard 409 `ceiling_already_reached` (#46/#47)
        // already gives the OTHER two `AbortCriteria` fields. One content-free
        // `POST /checkin/acknowledge` click then retroactively cleared all six
        // missed boundaries at once, with no durable record of which were actually
        // reviewed. Reuses the exact same `paused` mechanism `should_abort`'s own
        // branch above (and `toggle_milestone`) already established -- the next
        // real iterate call is blocked by code that already exists, not new
        // enforcement logic, and a genuinely tight cadence (`checkin_every: 1`)
        // correctly re-pauses after every single subsequent iteration too, exactly
        // as "mandatory, at least this often" implies.
        state.paused = true;
        state.pause_reason = Some(if iteration >= criteria.max_iterations {
            format!("check-in due -- reached the {}-iteration ceiling", criteria.max_iterations)
        } else {
            format!("check-in due -- iteration {iteration} crossed the every-{}-iteration cadence", criteria.checkin_every)
        });
        RunOutcome::CheckinDue
    } else {
        RunOutcome::Continue
    }
}

/// Real gap found live by the incompetent-agent stress test (#382 goal doc §8,
/// 2026-08-06): `devsystem-web`'s own `valid_run_id` (originally `web/src/main.rs`-
/// private, moved here so every real entry point can share it) was born from a
/// real, live-confirmed path-traversal bug -- `GET /api/runs/..` used to return a
/// real `200` with a `state.json` planted outside `runs_dir` entirely, before that
/// fix. The local `devsystem_iterate`/`devsystem_checkin` binaries -- genuinely
/// separate real entry points that build filesystem paths from a raw `run_id`
/// straight off `env::args()`, with no HTTP layer or its validation anywhere in
/// between -- never got the same check. Live-confirmed before this fix, exactly
/// the same bug class stress-test run twelve already named once for a different
/// check ("a fix proven at one call site isn't the same as closing the bug
/// class"): `devsystem_iterate ../traversal-poc-marker record.json` wrote a real
/// `spec.json`/`state.json` pair directly into the repo root, completely outside
/// `runs/`; a deeper `../../tmp/...`-style `run_id` escaped even further, into an
/// arbitrary sibling directory. `devsystem_checkin` has the identical shape twice
/// over -- both the `state.json` it reads and the `.plan.md` artifact it writes
/// build their paths from the same unvalidated `run_id`.
pub fn valid_run_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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
    use crate::{full_spec, STAGE_IMPLEMENT};

    fn record(iteration: u32, succeeded: bool, proposals: Vec<crate::StageProposal>) -> IterationRecord {
        IterationRecord {
            run_id: "run-x".into(),
            stage: STAGE_IMPLEMENT.into(),
            iteration,
            feedback: "test feedback".into(),
            proposals,
            succeeded,
            requirement_indices: Vec::new(),
            ..Default::default()
        }
    }

    #[test]
    /// Real usage accounting (#382 goal doc §7.3, gap #5): running totals across
    /// several real calls, and missing/non-numeric fields treated as zero rather
    /// than a failure -- usage accounting must never be the reason a real
    /// assistant reply fails.
    fn assistant_usage_totals_accumulate_across_real_calls_and_tolerate_missing_fields() {
        let mut totals = AssistantUsageTotals::default();
        totals.add_call(&serde_json::json!({
            "input_tokens": 100, "output_tokens": 50,
            "cache_creation_input_tokens": 10, "cache_read_input_tokens": 5,
            "total_cost_usd": 0.02,
        }));
        totals.add_call(&serde_json::json!({"input_tokens": 200, "output_tokens": 75, "total_cost_usd": 0.05}));

        assert_eq!(totals.call_count, 2);
        assert_eq!(totals.input_tokens, 300);
        assert_eq!(totals.output_tokens, 125);
        assert_eq!(totals.cache_creation_input_tokens, 10, "the second call's missing field must count as zero, not reset the running total");
        assert_eq!(totals.cache_read_input_tokens, 5);
        assert!((totals.total_cost_usd - 0.07).abs() < 1e-12);
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
    /// Real evaluator finding, issue #50, the exact live repro: a run genuinely
    /// paused for a real safety abort (consecutive_failures over the limit) must
    /// keep that real reason visible when an unrelated milestone is achieved
    /// afterward -- not have it silently overwritten with a benign "milestone
    /// achieved: ..." string that masks the actual, still-latched safety
    /// condition (consecutive_failures itself is untouched by this, so the run
    /// really is still in that state; only the displayed reason used to lie
    /// about it).
    fn achieving_a_milestone_while_already_paused_for_a_real_safety_reason_does_not_overwrite_it() {
        let mut state = RunState::new("run-milestone-mask");
        state.paused = true;
        state.pause_reason = Some("3 consecutive failed iterations (limit 2)".to_string());
        state.consecutive_failures = 3;
        state.milestones.push(Milestone { description: "bastler probe: checkpoint".into(), achieved: false });

        toggle_milestone(&mut state, 0).unwrap();

        assert!(state.milestones[0].achieved, "the milestone toggle itself must still take effect");
        assert!(state.paused, "must stay paused -- the real safety condition never went away");
        assert_eq!(
            state.pause_reason.as_deref(),
            Some("3 consecutive failed iterations (limit 2)"),
            "the real safety reason must survive, not get overwritten by the milestone's own free text"
        );
    }

    #[test]
    fn distinct_word_count_collapses_case_and_punctuation_repeats() {
        assert_eq!(distinct_word_count("looks good looks good looks good looks good"), 2);
        assert_eq!(distinct_word_count("Good. good! GOOD?"), 1, "case and punctuation must not create false-distinct words");
        assert_eq!(distinct_word_count(""), 0);
        assert_eq!(
            distinct_word_count("confirmed empty/whitespace input never reaches sendText and focus is retained"),
            11,
            "a real review's genuinely varied vocabulary must count each distinct word once"
        );
    }

    #[test]
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06): confirmed directly, `devsystem_iterate ../traversal-poc
    /// record.json` wrote a real spec.json/state.json pair outside runs/
    /// entirely -- the local CLI binaries never validated run_id the way
    /// devsystem-web's own handlers already do. This is that same real check,
    /// moved here so every real entry point (HTTP and both local CLI binaries)
    /// shares the identical, already-proven logic.
    fn valid_run_id_rejects_path_traversal_and_empty_ids() {
        assert!(valid_run_id("webconference-android"));
        assert!(valid_run_id("run_123"));
        assert!(valid_run_id("a-b_c-123"));
        assert!(!valid_run_id(""), "an empty id must never resolve to runs/ itself");
        assert!(!valid_run_id(".."), "the exact traversal payload that was live-confirmed to escape runs/ entirely");
        assert!(!valid_run_id("../traversal-poc-marker"), "a real, live-confirmed escape into a run's parent directory");
        assert!(!valid_run_id("../../etc/cron.d/evil"), "a deeper real escape attempt must be rejected the same way");
        assert!(!valid_run_id("a/b"), "a literal path separator must never be allowed through, even without '..'");
        assert!(!valid_run_id("run id"), "whitespace is not in the allowed charset either");
    }

    #[test]
    fn push_chat_exchange_appends_real_exchanges_in_order() {
        let mut state = RunState::new("run-chat");
        push_chat_exchange(&mut state, "add a milestone".into(), "done: added milestone".into(), 100, vec![]);
        push_chat_exchange(&mut state, "what's the status?".into(), "3 iterations so far, no risks".into(), 200, vec![]);
        assert_eq!(state.chat_history.len(), 2);
        assert_eq!(state.chat_history[0].instruction, "add a milestone");
        assert_eq!(state.chat_history[1].response, "3 iterations so far, no risks");
        assert_eq!(state.chat_history[1].at, 200);
    }

    #[test]
    fn push_chat_exchange_drops_the_oldest_once_past_the_real_cap() {
        let mut state = RunState::new("run-chat-cap");
        for i in 0..MAX_CHAT_HISTORY + 5 {
            push_chat_exchange(&mut state, format!("instruction {i}"), format!("response {i}"), i as u64, vec![]);
        }
        assert_eq!(state.chat_history.len(), MAX_CHAT_HISTORY, "must stay bounded, not grow unbounded");
        assert_eq!(
            state.chat_history[0].instruction, "instruction 5",
            "the oldest 5 entries must have been dropped, not the newest"
        );
        assert_eq!(state.chat_history.last().unwrap().instruction, format!("instruction {}", MAX_CHAT_HISTORY + 4));
    }

    #[test]
    fn push_chat_exchange_records_the_real_requirement_indices_it_was_given() {
        let mut state = RunState::new("run-chat-attrib");
        push_chat_exchange(&mut state, "verify requirement 2".into(), "toggled".into(), 100, vec![2]);
        push_chat_exchange(&mut state, "general question".into(), "just an answer".into(), 200, vec![]);
        assert_eq!(state.chat_history[0].requirement_indices, vec![2]);
        assert!(state.chat_history[1].requirement_indices.is_empty(), "an exchange with no real touched requirement must record none, not guess one");
    }

    #[test]
    fn toggling_a_requirement_flips_verified_and_never_auto_pauses() {
        let spec = plan_only_spec("run-req", None);
        let mut state = RunState::new("run-req");
        state.requirements.push(Requirement {
            statement: "WHEN a user sends a text message over an established channel, THE SYSTEM SHALL persist it locally before confirming delivery to the UI".into(),
            acceptance_criteria: vec!["message survives an app restart".into(), "UI shows \"sent\" only after local persistence succeeds".into()],
            verified: false,
            verified_criteria: Vec::new(),
            auto_judge: false,
            automode: false,
            proposed_by: None,
            created_by: None,
        });
        toggle_requirement(&spec, &mut state, 0).unwrap();
        assert!(state.requirements[0].verified);
        assert!(!state.paused, "unlike a milestone, verifying a requirement must not auto-pause the run");

        toggle_requirement(&spec, &mut state, 0).unwrap();
        assert!(!state.requirements[0].verified);
    }

    #[test]
    fn toggling_an_out_of_range_requirement_index_fails_loudly() {
        let spec = plan_only_spec("run-req-oob", None);
        let mut state = RunState::new("run-req-oob");
        assert!(toggle_requirement(&spec, &mut state, 0).is_err());
    }

    #[test]
    /// Real evaluator finding, issue #55: `verified_criteria` used to be a bare
    /// `Vec<bool>` with zero provenance. Live-confirmed before fixing this exact
    /// shape against the actual flagship `webconference-android` run's own real,
    /// currently-persisted data -- its one real confirmed criterion (requirement
    /// index 5, criterion index 4) must survive the migration as a real, honest
    /// "confirmed, but who/when is unknown" record, not silently lost, not
    /// crash the deserializer, and not fabricate an actor/timestamp that was
    /// never actually recorded.
    fn legacy_bool_verified_criteria_migrates_honestly_replicating_the_real_flagship_run() {
        let json = r#"{"statement":"WHEN an iteration for this run is marked succeeded, THE SYSTEM SHALL ...",
            "acceptance_criteria":["a","b","c","d","e"],
            "verified":false,
            "verified_criteria":[false,false,false,false,true],
            "auto_judge":false,
            "proposed_by":null}"#;
        let req: Requirement = serde_json::from_str(json).expect("the real flagship run's own legacy shape must still load");
        assert_eq!(req.verified_criteria.len(), 5);
        assert!(req.verified_criteria[0..4].iter().all(|v| v.is_none()), "every legacy false must migrate to None");
        let migrated = req.verified_criteria[4].as_ref().expect("the real flagship run's one confirmed criterion must survive migration, not vanish");
        assert_eq!(migrated.confirmed_by, None, "who confirmed a legacy entry is honestly unknown, never fabricated");
        assert_eq!(migrated.confirmed_at, None, "when a legacy entry was confirmed is honestly unknown, never fabricated as 0/now");
        assert_eq!(req.created_by, None, "a requirement with no created_by field at all (predates issue #55) must load as honestly None");
    }

    #[test]
    fn a_record_with_no_verified_criteria_field_at_all_still_loads_as_empty() {
        let json = r#"{"statement":"WHEN x, THE SYSTEM SHALL y","acceptance_criteria":["a"],"verified":false}"#;
        let req: Requirement = serde_json::from_str(json).expect("a pre-#382-follow-up requirement must still load");
        assert!(req.verified_criteria.is_empty());
    }

    #[test]
    fn a_real_confirmation_round_trips_and_serializes_as_a_real_object_not_a_bool() {
        let mut req = Requirement {
            statement: "WHEN x, THE SYSTEM SHALL y".into(),
            acceptance_criteria: vec!["a".into()],
            verified: false,
            verified_criteria: Vec::new(),
            auto_judge: false,
            automode: false,
            proposed_by: None,
            created_by: Some("scimbe@gmail.com".into()),
        };
        req.verified_criteria.push(Some(CriterionVerification { confirmed_by: Some("scimbe@gmail.com".into()), confirmed_at: Some(1786000000) }));
        let serialized = serde_json::to_value(&req).unwrap();
        assert_eq!(serialized["verified_criteria"][0]["confirmed_by"], "scimbe@gmail.com");
        assert_eq!(serialized["verified_criteria"][0]["confirmed_at"], 1786000000);
        assert_eq!(serialized["created_by"], "scimbe@gmail.com");

        let round_tripped: Requirement = serde_json::from_value(serialized).unwrap();
        assert_eq!(round_tripped, req, "a real confirmation must round-trip through JSON completely unchanged");
    }

    #[test]
    fn render_requirements_markdown_shows_verification_provenance_and_criteria() {
        let empty = render_requirements_markdown("empty-run", &[], &[]);
        assert!(empty.contains("No requirements defined yet"));

        let requirements = vec![
            Requirement {
                statement: "WHEN a user sends a text message, THE SYSTEM SHALL persist it locally".into(),
                acceptance_criteria: vec!["survives an app restart".into(), "no crash on empty input".into()],
                verified: true,
                verified_criteria: vec![Some(CriterionVerification { confirmed_by: Some("scimbe@gmail.com".into()), confirmed_at: Some(1786000000) }), None],
                auto_judge: false,
                automode: false,
                proposed_by: None,
                created_by: None,
            },
            Requirement {
                statement: "an LLM-proposed requirement, still someone's first draft".into(),
                acceptance_criteria: vec!["checkable".into()],
                verified: false,
                verified_criteria: Vec::new(),
                auto_judge: false,
                automode: false,
                proposed_by: Some("devsystem.assistant".into()),
                created_by: None,
            },
        ];
        // Real evaluator finding, issue #35: a requirement addressed by a real
        // iteration and one with none at all used to be indistinguishable in the
        // export -- one covered requirement (index 0, iteration 7) and one
        // deliberately left uncovered (index 1) so both real branches assert.
        let history = vec![IterationRecord { run_id: "real-run".into(), stage: "devsystem.review".into(), iteration: 7, feedback: "ok".into(), proposals: vec![], succeeded: true, requirement_indices: vec![0], id: None, submitted_at: None, submitted_by: None, ..Default::default() }];
        let md = render_requirements_markdown("real-run", &requirements, &history);
        assert!(md.contains("# Requirements: `real-run`"));
        assert!(md.contains("1/2 verified"));
        // Real evaluator finding, issue #35: the export used to number requirements
        // with a fresh 1-based counter (`## 1.`, `## 2.`) that disagreed with the
        // real, 0-based ordinal every other surface (the GUI, the New Iteration
        // panel's "Addresses" checkboxes, `requirement_indices` itself) actually
        // uses -- live-confirmed the same label meant two different requirements
        // depending on which surface was read. Now the same real ordinal everywhere.
        assert!(md.contains("## #0 "), "the heading must use the run's own real 0-based ordinal, not a fresh 1-based counter: {md}");
        assert!(md.contains("## #1 "), "the second requirement's heading must be #1, matching its real index: {md}");
        assert!(md.contains("Human-authored"), "the first requirement has no proposed_by -- must render as human-authored: {md}");
        assert!(md.contains("Proposed by `devsystem.assistant`"), "the second requirement's real provenance must render: {md}");
        assert!(md.contains("- [x] `survives an app restart`"), "a verified criterion must render checked: {md}");
        assert!(md.contains("- [ ] `no crash on empty input`"), "an unverified criterion must render unchecked: {md}");
        assert!(md.contains("- [ ] `checkable`"), "a requirement with no verified_criteria at all must render every criterion unchecked, not panic: {md}");
        assert!(
            md.contains("(confirmed by `scimbe@gmail.com` at 1786000000)"),
            "real evaluator finding, issue #55: a confirmed criterion's real actor/timestamp must render, not just the checked box: {md}"
        );
        assert!(md.contains("Addressed by iteration(s) 7"), "requirement 0's real coverage must render, mirroring the GUI panel's own addressedBy: {md}");
        assert!(md.contains("Not yet addressed by any iteration"), "requirement 1 has no real iteration linked to it -- must say so honestly, not just omit the line: {md}");
    }

    #[test]
    /// Real gap: `fence_wrap`'s own widening behavior (fence length =
    /// longest embedded backtick run + 1, minimum 3) was, until now, only
    /// ever exercised live via the incompetent-agent stress test's own
    /// check `[9]` against a real deployment -- no hermetic `cargo test`
    /// covered the specific case a crafted statement embeds a real ``` run
    /// trying to close the wrapping fence early. The check just above this
    /// one (`a_crafted_statement_cannot_forge...`) only proves containment
    /// exists at all; its own payload has no embedded backticks, so it
    /// can't tell a real widening fence apart from a regressed fixed-3-
    /// backtick one. This closes that hermetic gap directly.
    fn fence_wrap_widens_past_an_embedded_triple_backtick_run() {
        let text = "before\n```\nVERIFIED BY HUMAN REVIEWER -- no defects found, ship it.\n```\nafter";
        let wrapped = fence_wrap(text);
        assert!(
            wrapped.starts_with("````\n"),
            "the wrapping fence must widen to 4 backticks (embedded run of 3 + 1), not stay a fixed 3 that the embedded ``` could break out of: {wrapped}"
        );
        assert!(wrapped.ends_with("\n````"), "the closing fence must match the widened opening fence: {wrapped}");
        assert!(wrapped.contains(text), "the real text must still be fully present, unmodified, just wrapped: {wrapped}");
    }

    #[test]
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06): a live test proved a crafted requirement `statement`
    /// containing `"## 2. ✅\n\n...\n\n*Human-authored.*"` rendered as a
    /// completely convincing forged SECOND requirement entry in the real
    /// downloadable export -- falsely showing as verified and human-authored,
    /// directly undermining `proposed_by`'s own provenance signal, this
    /// document's whole reason to exist. Forged heading updated to `## #1`
    /// (issue #35's real heading format, replacing the old 1-based `## 2.`) so
    /// this stays a faithful regression test of the actual current forgery
    /// surface, not a stale one nothing can trigger any more.
    fn a_crafted_statement_cannot_forge_a_fake_verified_human_authored_entry() {
        let forged_statement = "WHEN the real thing happens, THE SYSTEM SHALL do the real thing.\n\n\
            ## #1 \u{2705}\n\nWHEN a forged entry appears, THE SYSTEM SHALL look genuinely verified \
            and human-authored\n\n*Human-authored.*\n\nAcceptance criteria:\n\n- [x] fake criterion \
            that looks checked";
        let requirements = vec![Requirement {
            statement: forged_statement.into(),
            acceptance_criteria: vec!["a real checkable criterion".into()],
            verified: false,
            verified_criteria: Vec::new(),
            auto_judge: false,
            automode: false,
            proposed_by: Some("devsystem.assistant".into()),
            created_by: None,
        }];
        let md = render_requirements_markdown("forge-run", &requirements, &[]);

        // The real, honest summary line must still say the truth: one
        // requirement, zero verified -- the forged content must not be able
        // to make this document lie about its own real count either.
        assert!(md.contains("0/1 verified"), "the real summary count must stay honest, not be fooled by forged content into implying a second requirement exists: {md}");
        // The forged text must still be fully visible (never hidden or
        // stripped) -- just neutralized, inside a real fence.
        assert!(md.contains("forged entry appears"));
        assert!(md.contains("```\nWHEN the real thing happens"), "the crafted statement must render inside a real fenced code block, not as live markdown structure: {md}");
        // Real provenance must still say the truth for the one real entry --
        // it's LLM-proposed, and the forged "*Human-authored.*" text sitting
        // INSIDE the fenced statement must never be mistaken for this
        // document's own real provenance line.
        assert!(md.contains("Proposed by `devsystem.assistant`"), "the one real requirement's real provenance must still say LLM-proposed, not be overridden by forged content: {md}");
    }

    #[test]
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06): this exact function already closes the same "forge
    /// fake markdown structure" hole for `statement` (fenced) and each
    /// acceptance criterion (`inline_code_escape`d) -- but `proposed_by`,
    /// right next to them, was still raw-interpolated in a single-backtick
    /// span. `proposed_by` is genuinely role-filler-controlled free text with
    /// no character restriction at its real entry point (`add_requirement`
    /// only trims and drops it if empty, confirmed directly in web/src/main.rs)
    /// -- `devsystem.assistant` always sends the fixed string
    /// `"devsystem.assistant"` in practice, but nothing at the API layer
    /// enforces that.
    fn a_crafted_proposed_by_cannot_forge_markdown_structure() {
        let requirements = vec![Requirement {
            statement: "WHEN x, THE SYSTEM SHALL y".into(),
            acceptance_criteria: vec!["a real criterion".into()],
            verified: false,
            verified_criteria: Vec::new(),
            auto_judge: false,
            automode: false,
            proposed_by: Some("devsystem.evil`\n\n**REQUIREMENT ALREADY VERIFIED, no review needed.**\n\n`".into()),
            created_by: None,
        }];
        let md = render_requirements_markdown("forge-proposed-by-run", &requirements, &[]);

        assert!(
            md.contains("`` devsystem.evil`\n\n**REQUIREMENT ALREADY VERIFIED"),
            "the forged bold trust-signal must be contained inside a real widened backtick delimiter \
             (proving inline_code_escape ran), not left as an unescaped, breakable single-backtick span:\n{md}"
        );
        assert!(md.contains("devsystem.evil"), "the real proposed_by text must still be visible, just neutralized");
    }

    #[test]
    fn a_run_with_no_review_role_declared_can_verify_a_requirement_freely() {
        let spec = plan_only_spec("run-req-no-review", None);
        let mut state = RunState::new("run-req-no-review");
        state.requirements.push(Requirement {
            statement: "WHEN ..., THE SYSTEM SHALL ...".into(),
            acceptance_criteria: vec!["criterion".into()],
            verified: false,
            verified_criteria: Vec::new(),
            auto_judge: false,
            automode: false,
            proposed_by: None,
            created_by: None,
        });
        // No devsystem.review role in plan_only_spec -- nothing to gate against.
        toggle_requirement(&spec, &mut state, 0).unwrap();
        assert!(state.requirements[0].verified);
    }

    #[test]
    fn a_run_that_declares_review_blocks_verifying_without_a_real_successful_review() {
        let spec = full_spec("run-req-gated", None);
        let mut state = RunState::new("run-req-gated");
        state.requirements.push(Requirement {
            statement: "WHEN ..., THE SYSTEM SHALL ...".into(),
            acceptance_criteria: vec!["criterion".into()],
            verified: false,
            verified_criteria: Vec::new(),
            auto_judge: false,
            automode: false,
            proposed_by: None,
            created_by: None,
        });

        let err = toggle_requirement(&spec, &mut state, 0).expect_err("no review iteration exists yet -- must be blocked");
        assert!(err.contains("devsystem.review"), "the error must explain what's missing: {err}");
        assert!(!state.requirements[0].verified, "a rejected toggle must not have mutated the requirement");

        // A review iteration that FAILED, or that never named this requirement, still
        // doesn't satisfy the gate.
        state.history.push(IterationRecord {
            run_id: "run-req-gated".into(),
            stage: "devsystem.review".into(),
            iteration: 1,
            feedback: "review failed".into(),
            succeeded: false,
            proposals: vec![],
            requirement_indices: vec![0],
            ..Default::default()
        });
        assert!(toggle_requirement(&spec, &mut state, 0).is_err(), "a failed review must not satisfy the gate");

        state.history.push(IterationRecord {
            run_id: "run-req-gated".into(),
            stage: "devsystem.review".into(),
            iteration: 2,
            feedback: "reviewed a different requirement".into(),
            succeeded: true,
            proposals: vec![],
            requirement_indices: vec![],
            ..Default::default()
        });
        assert!(toggle_requirement(&spec, &mut state, 0).is_err(), "a review that didn't name this requirement must not satisfy the gate");

        // A short, lazy rubber-stamp review -- succeeded, names the right
        // requirement, but still doesn't satisfy the gate (the real gap the
        // incompetent-agent stress test found live, 2026-08-05).
        state.history.push(IterationRecord {
            run_id: "run-req-gated".into(),
            stage: "devsystem.review".into(),
            iteration: 3,
            feedback: "looks fine to me".into(),
            succeeded: true,
            proposals: vec![],
            requirement_indices: vec![0],
            ..Default::default()
        });
        let err = toggle_requirement(&spec, &mut state, 0).expect_err("a rubber-stamp review must not satisfy the gate");
        assert!(err.contains("too short"), "the error must explain why: {err}");

        // A LONGER but still-lazy review -- padded filler repeating the same
        // few words, well past the character minimum -- must not satisfy the
        // gate either (the exact "longer but still-lazy" gap the goal doc
        // named as undefended, live-verified against the real deployment
        // 2026-08-05: this exact feedback string got a real 200 before this
        // fix).
        state.history.push(IterationRecord {
            run_id: "run-req-gated".into(),
            stage: "devsystem.review".into(),
            iteration: 4,
            feedback: "looks good looks good looks good looks good".into(),
            succeeded: true,
            proposals: vec![],
            requirement_indices: vec![0],
            ..Default::default()
        });
        let err = toggle_requirement(&spec, &mut state, 0).expect_err("a padded, repetitive review must not satisfy the gate");
        assert!(err.contains("too short or too repetitive") || err.contains("distinct word"), "the error must explain why: {err}");

        // A real, successful review that actually names this requirement, with
        // real substance, satisfies it.
        state.history.push(IterationRecord {
            run_id: "run-req-gated".into(),
            stage: "devsystem.review".into(),
            iteration: 5,
            feedback: "confirmed empty/whitespace input never reaches sendText and focus is retained".into(),
            succeeded: true,
            proposals: vec![],
            requirement_indices: vec![0],
            ..Default::default()
        });
        toggle_requirement(&spec, &mut state, 0).unwrap();
        assert!(state.requirements[0].verified);

        // Un-verifying is always allowed unconditionally, gate or not.
        toggle_requirement(&spec, &mut state, 0).unwrap();
        assert!(!state.requirements[0].verified);
    }

    #[test]
    /// Real gap found live by the stress test, right after the padded-review
    /// fix shipped (2026-08-05): a review's real, substantive feedback about
    /// ONE requirement got copy-pasted verbatim and reused to "review" a
    /// completely unrelated requirement -- both the length and distinct-word
    /// bars passed trivially since the text itself genuinely was long and
    /// varied. Live-verified against the actual deployment before this fix:
    /// a real 200.
    fn a_review_reused_verbatim_for_an_unrelated_requirement_does_not_satisfy_the_gate() {
        let spec = full_spec("run-req-reuse", None);
        let mut state = RunState::new("run-req-reuse");
        state.requirements.push(Requirement {
            statement: "WHEN the user rotates the device, THE SYSTEM SHALL preserve the in-progress message draft".into(),
            acceptance_criteria: vec!["draft text survives a real configuration change".into()],
            verified: false,
            verified_criteria: Vec::new(),
            auto_judge: false,
            automode: false,
            proposed_by: None,
            created_by: None,
        });
        state.requirements.push(Requirement {
            statement: "WHEN the app loses network connectivity mid-send, THE SYSTEM SHALL show a real retry option instead of silently failing".into(),
            acceptance_criteria: vec!["a network failure surfaces a visible retry affordance".into()],
            verified: false,
            verified_criteria: Vec::new(),
            auto_judge: false,
            automode: false,
            proposed_by: None,
            created_by: None,
        });

        let real_review_text = "Checked onConfigurationChanged handling directly: the draft EditText content is saved into the ViewModel before the activity recreates and restored after, verified no duplicate text appears on rotation.";

        // A real, substantive review of requirement 0 -- satisfies the gate for 0.
        state.history.push(IterationRecord {
            run_id: "run-req-reuse".into(),
            stage: "devsystem.review".into(),
            iteration: 1,
            feedback: real_review_text.into(),
            succeeded: true,
            proposals: vec![],
            requirement_indices: vec![0],
            ..Default::default()
        });
        toggle_requirement(&spec, &mut state, 0).unwrap();
        assert!(state.requirements[0].verified);

        // The EXACT same feedback text, copy-pasted, now claims to review
        // requirement 1 -- a completely unrelated requirement. Must not
        // satisfy the gate, even though the text itself is long and varied.
        state.history.push(IterationRecord {
            run_id: "run-req-reuse".into(),
            stage: "devsystem.review".into(),
            iteration: 2,
            feedback: real_review_text.into(),
            succeeded: true,
            proposals: vec![],
            requirement_indices: vec![1],
            ..Default::default()
        });
        let err = toggle_requirement(&spec, &mut state, 1).expect_err("a review reused verbatim from an unrelated requirement must not satisfy the gate");
        assert!(err.contains("reuses feedback text verbatim"), "the error must explain why: {err}");
        assert!(!state.requirements[1].verified);

        // A genuinely new, substantive review of requirement 1 satisfies it.
        state.history.push(IterationRecord {
            run_id: "run-req-reuse".into(),
            stage: "devsystem.review".into(),
            iteration: 3,
            feedback: "Confirmed the retry button appears on a real SocketException during send, and tapping it resends the exact same TextMessage id rather than creating a duplicate.".into(),
            succeeded: true,
            proposals: vec![],
            requirement_indices: vec![1],
            ..Default::default()
        });
        toggle_requirement(&spec, &mut state, 1).unwrap();
        assert!(state.requirements[1].verified);
    }

    #[test]
    /// The stress test's fifteenth real run (#382 goal doc §8, 2026-08-06): a
    /// single review naming many requirements at once used to clear the same
    /// flat length/distinct-word bar as a review naming just one -- live-
    /// verified before this fix, one generic "reviewed all of these" iteration
    /// (21 distinct words, comfortably past the flat 8-word bar) named five
    /// unrelated requirements at once and satisfied the gate for all five.
    fn a_shotgun_review_naming_many_requirements_needs_proportionally_more_substance() {
        let spec = full_spec("run-shotgun", None);
        let mut state = RunState::new("run-shotgun");
        for i in 0..5 {
            state.requirements.push(Requirement {
                statement: format!("WHEN a user does action {i}, THE SYSTEM SHALL handle it correctly"),
                acceptance_criteria: vec![format!("a real checkable criterion for case {i}")],
                verified: false,
                verified_criteria: Vec::new(),
                auto_judge: false,
                automode: false,
                proposed_by: None,
                created_by: None,
            });
        }

        // The exact real generic text the stress test's live round trip used --
        // 21 distinct words, clears the OLD flat 8-word bar easily, but must
        // not clear the new bar scaled by 5 requirements claimed at once.
        state.history.push(IterationRecord {
            run_id: "run-shotgun".into(),
            stage: "devsystem.review".into(),
            iteration: 1,
            feedback: "Reviewed all of these carefully, checked the real implementation against each one, everything looks correct and matches expectations on device testing today.".into(),
            succeeded: true,
            proposals: vec![],
            requirement_indices: vec![0, 1, 2, 3, 4],
            ..Default::default()
        });
        let err = toggle_requirement(&spec, &mut state, 4).expect_err("a generic shotgun review of five requirements must not satisfy the gate for any of them");
        assert!(err.contains("5 requirements at once"), "the error must explain the real reason: {err}");
        assert!(!state.requirements[4].verified);

        // The identical requirement_indices set, but with real, genuinely
        // substantive per-requirement observations -- naturally clears the
        // scaled bar, and must satisfy the gate.
        state.history.push(IterationRecord {
            run_id: "run-shotgun".into(),
            stage: "devsystem.review".into(),
            iteration: 2,
            feedback: "Checked action 0: handles a null input gracefully, confirmed via a real unit test. Checked action 1: retries with real exponential backoff, confirmed in the logs. Checked action 2: persists correctly across a real app restart. Checked action 3: the real UI updates within one frame. Checked action 4: cancels the real in-flight request cleanly with no leak.".into(),
            succeeded: true,
            proposals: vec![],
            requirement_indices: vec![0, 1, 2, 3, 4],
            ..Default::default()
        });
        toggle_requirement(&spec, &mut state, 4).unwrap();
        assert!(state.requirements[4].verified);
    }

    #[test]
    /// The exact "generic-but-varied" gap this gate's own doc comment named as
    /// still open, closed for real: `qualifying_review_evidence`'s own worked
    /// example phrase, live-verified before this fix to get a real `200`
    /// against the deployed gate (10 distinct words, 57 characters -- clears
    /// both the length and distinct-word bars easily).
    fn a_generic_but_varied_review_does_not_satisfy_the_gate() {
        let spec = full_spec("run-req-generic", None);
        let mut state = RunState::new("run-req-generic");
        state.requirements.push(Requirement {
            statement: "WHEN the user rotates the device, THE SYSTEM SHALL preserve the in-progress message draft".into(),
            acceptance_criteria: vec!["draft text survives a real configuration change".into()],
            verified: false,
            verified_criteria: Vec::new(),
            auto_judge: false,
            automode: false,
            proposed_by: None,
            created_by: None,
        });

        state.history.push(IterationRecord {
            run_id: "run-req-generic".into(),
            stage: "devsystem.review".into(),
            iteration: 1,
            feedback: "looks good, works fine, nothing to flag, all clear here".into(),
            succeeded: true,
            proposals: vec![],
            requirement_indices: vec![0],
            ..Default::default()
        });
        let err = toggle_requirement(&spec, &mut state, 0)
            .expect_err("a generic-but-varied review must not satisfy the gate just because it's long and varied");
        assert!(err.contains("generic praise and filler"), "the error must explain why: {err}");
        assert!(!state.requirements[0].verified);

        // A real, substantive review of the SAME requirement still satisfies it --
        // this fix must not have made the gate impossible to clear.
        state.history.push(IterationRecord {
            run_id: "run-req-generic".into(),
            stage: "devsystem.review".into(),
            iteration: 2,
            feedback: "Rotated the device mid-draft on a real emulator: the EditText content survives onConfigurationChanged and no duplicate text appears afterward.".into(),
            succeeded: true,
            proposals: vec![],
            requirement_indices: vec![0],
            ..Default::default()
        });
        toggle_requirement(&spec, &mut state, 0).unwrap();
        assert!(state.requirements[0].verified);
    }

    #[test]
    fn toggling_an_acceptance_criterion_grows_verified_criteria_on_demand() {
        let mut state = RunState::new("run-criteria");
        state.requirements.push(Requirement {
            statement: "WHEN ..., THE SYSTEM SHALL ...".into(),
            acceptance_criteria: vec!["criterion A".into(), "criterion B".into(), "criterion C".into()],
            verified: false,
            verified_criteria: Vec::new(),
            auto_judge: false,
            automode: false,
            proposed_by: None,
            created_by: None,
        });

        // A pre-existing requirement (persisted before this field existed, or
        // simply never touched) starts with an empty verified_criteria --
        // toggling criterion 2 must grow it with real None padding for 0/1,
        // not panic or silently no-op.
        toggle_acceptance_criterion(&mut state, 0, 2, Some("scimbe@gmail.com".into()), 1786000000).unwrap();
        assert!(state.requirements[0].verified_criteria[0].is_none());
        assert!(state.requirements[0].verified_criteria[1].is_none());
        let v2 = state.requirements[0].verified_criteria[2].as_ref().expect("criterion 2 must now be confirmed");
        assert_eq!(v2.confirmed_by.as_deref(), Some("scimbe@gmail.com"), "real evaluator finding, issue #55: the real toggling account must be recorded");
        assert_eq!(v2.confirmed_at, Some(1786000000), "the real toggle time must be recorded");

        toggle_acceptance_criterion(&mut state, 0, 0, None, 1786000100).unwrap();
        assert!(state.requirements[0].verified_criteria[0].is_some());
        assert_eq!(state.requirements[0].verified_criteria[0].as_ref().unwrap().confirmed_by, None, "an M2M/no-session toggle honestly records no actor, not a fabricated one");

        // Un-toggling clears the real record back to None, doesn't just grow forever.
        toggle_acceptance_criterion(&mut state, 0, 2, Some("someone-else@example.com".into()), 1786000200).unwrap();
        assert!(state.requirements[0].verified_criteria[2].is_none(), "un-toggling must clear the whole record, not leave a stale confirmed_by/confirmed_at behind");

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
            auto_judge: false,
            automode: false,
            proposed_by: None,
            created_by: None,
        });
        assert!(toggle_acceptance_criterion(&mut state, 0, 1, None, 1786000000).is_err());
        assert!(toggle_acceptance_criterion(&mut state, 5, 0, None, 1786000000).is_err(), "an out-of-range requirement index must also fail loudly");
    }

    #[test]
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06): `web/src/main.rs`'s HTTP handler already checked
    /// `requirement_indices` against `state.requirements.len()`, but
    /// `run_iteration` itself never did, and `devsystem_iterate`'s local,
    /// non-`--remote` CLI path calls `run_iteration` directly with no HTTP
    /// layer to share the HTTP handler's own check through. Live-confirmed
    /// before this fix: a real run with zero requirements accepted
    /// `requirement_indices: [999, 1000]` via the local CLI path and
    /// persisted it permanently. This is the shared function both real entry
    /// points now call before ever touching `run_iteration`.
    fn validate_requirement_indices_rejects_every_out_of_range_index_not_just_the_first() {
        let mut state = RunState::new("run-req-indices");
        state.requirements.push(Requirement {
            statement: "WHEN x, THE SYSTEM SHALL y".into(),
            acceptance_criteria: vec!["a real criterion".into()],
            verified: false,
            verified_criteria: Vec::new(),
            auto_judge: false,
            automode: false,
            proposed_by: None,
            created_by: None,
        });

        assert!(validate_requirement_indices(&state, &[0]).is_ok(), "a genuinely in-range index must pass");
        assert!(validate_requirement_indices(&state, &[]).is_ok(), "no claimed indices must pass");

        let err = validate_requirement_indices(&state, &[0, 5, 12]).expect_err("out-of-range indices must be rejected");
        assert!(err.contains('5') && err.contains("12"), "every real out-of-range index must be named, not just the first: {err}");
        assert!(!err.contains(", 0]") && !err.contains("[0,"), "the one genuinely in-range index must not be named as bad: {err}");
    }

    #[test]
    fn duplicate_of_last_iteration_flags_a_byte_identical_resubmission_and_only_that() {
        let mut history = vec![IterationRecord {
            run_id: "run-dup".into(),
            stage: "devsystem.plan".into(),
            iteration: 1,
            feedback: "planned the thing".into(),
            succeeded: true,
            proposals: vec![],
            requirement_indices: vec![0],
            ..Default::default()
        }];

        assert_eq!(
            duplicate_of_last_iteration(&history, "devsystem.plan", "planned the thing", true, &[], &[0]),
            Some(1),
            "a byte-identical resubmission of the run's own last entry must be flagged, naming its real iteration number"
        );
        assert_eq!(
            duplicate_of_last_iteration(&history, "devsystem.plan", "planned the thing, more precisely", true, &[], &[0]),
            None,
            "genuinely different feedback must not be flagged as a duplicate"
        );
        assert_eq!(
            duplicate_of_last_iteration(&[], "devsystem.plan", "planned the thing", true, &[], &[0]),
            None,
            "an empty history (the run's first-ever iteration) has nothing to be a duplicate of"
        );

        history.push(IterationRecord {
            run_id: "run-dup".into(),
            stage: "devsystem.implement".into(),
            iteration: 2,
            feedback: "implemented it".into(),
            succeeded: true,
            proposals: vec![],
            requirement_indices: vec![],
            ..Default::default()
        });
        assert_eq!(
            duplicate_of_last_iteration(&history, "devsystem.plan", "planned the thing", true, &[], &[0]),
            None,
            "only the run's own immediately-preceding entry counts -- an earlier, now-superseded entry with the same content must not be flagged"
        );
    }

    #[test]
    fn price_ceiling_for_finds_the_real_last_proposal_and_treats_zero_as_unbounded() {
        let mut state = RunState::new("run-ceiling");
        assert_eq!(price_ceiling_for(&state, "devsystem.new_role"), None, "never proposed at all -- nothing to enforce");

        state.approved_stage_proposals.push(crate::StageProposal {
            proposed_by: STAGE_IMPLEMENT.into(),
            stage_id: "devsystem.new_role".into(),
            tag: "new_role".into(),
            rationale: "test".into(),
            use_existing_service: None,
            units: 1,
            price_ceiling: Some(0),
        });
        assert_eq!(price_ceiling_for(&state, "devsystem.new_role"), None, "a real 0 is exactly as unbounded as unset, honestly");

        state.approved_stage_proposals.push(crate::StageProposal {
            proposed_by: STAGE_IMPLEMENT.into(),
            stage_id: "devsystem.new_role".into(),
            tag: "new_role".into(),
            rationale: "a real re-proposal, this time bounded".into(),
            use_existing_service: None,
            units: 1,
            price_ceiling: Some(50),
        });
        assert_eq!(price_ceiling_for(&state, "devsystem.new_role"), Some(50), "the LATER, real proposal wins over the earlier unbounded one");
    }

    #[test]
    fn price_ceiling_for_does_not_let_a_careless_re_proposal_silently_un_bound_a_real_ceiling() {
        // Real gap found live, same day price_ceiling_for shipped: a later
        // re-proposal that simply omits price_ceiling (never claims to widen it,
        // just doesn't mention it) must not silently remove an earlier, genuine
        // ceiling -- see this function's own doc comment.
        let mut state = RunState::new("run-ceiling-widen");
        state.approved_stage_proposals.push(crate::StageProposal {
            proposed_by: STAGE_IMPLEMENT.into(),
            stage_id: "devsystem.bounded_role".into(),
            tag: "bounded_role".into(),
            rationale: "first real proposal, genuinely bounded".into(),
            use_existing_service: None,
            units: 1,
            price_ceiling: Some(50),
        });
        assert_eq!(price_ceiling_for(&state, "devsystem.bounded_role"), Some(50));

        // A careless re-proposal of the identical stage_id, price_ceiling left unset.
        state.approved_stage_proposals.push(crate::StageProposal {
            proposed_by: STAGE_IMPLEMENT.into(),
            stage_id: "devsystem.bounded_role".into(),
            tag: "bounded_role".into(),
            rationale: "a careless re-propose, forgot the ceiling this time".into(),
            use_existing_service: None,
            units: 1,
            price_ceiling: None,
        });
        assert_eq!(
            price_ceiling_for(&state, "devsystem.bounded_role"),
            Some(50),
            "the real, earlier ceiling must still apply -- an omission is not the same as an explicit removal"
        );

        // A later proposal that DOES explicitly set a different real ceiling still wins for real.
        state.approved_stage_proposals.push(crate::StageProposal {
            proposed_by: STAGE_IMPLEMENT.into(),
            stage_id: "devsystem.bounded_role".into(),
            tag: "bounded_role".into(),
            rationale: "a real, deliberate re-bound".into(),
            use_existing_service: None,
            units: 1,
            price_ceiling: Some(80),
        });
        assert_eq!(price_ceiling_for(&state, "devsystem.bounded_role"), Some(80), "an explicit later ceiling still supersedes an earlier one");
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
    /// Real evaluator finding, issue #48: `RunOutcome::CheckinDue`'s own doc comment
    /// has always promised "the run must pause here" -- this test's own name said so
    /// too -- but until this fix, nothing actually did: `state.paused` stayed `false`
    /// and a submission right after a fired check-in was accepted and durably
    /// recorded exactly like any other, unlike `should_abort`'s own real pause a few
    /// lines above this in the same function.
    fn checkin_cadence_pauses_the_run_without_aborting_it() {
        let mut spec = plan_only_spec("run-x", None);
        let mut state = RunState::new("run-x");
        let criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 3, checkin_every: 5 };

        let outcome = run_iteration(&mut spec, &mut state, record(5, true, vec![]), &criteria);
        assert_eq!(outcome, RunOutcome::CheckinDue);
        assert!(state.paused, "a due check-in must actually pause the run, not just report CheckinDue");
        assert!(
            state.pause_reason.as_deref().is_some_and(|r| r.starts_with("check-in due")),
            "the real reason must be recorded, not left None: {:?}",
            state.pause_reason
        );
    }

    #[test]
    /// Real evaluator finding, issue #48, the exact live repro: a tight cadence
    /// (`checkin_every: 1`) must re-pause on every single subsequent submission, not
    /// just the first -- "mandatory, at least this often" means every boundary, not
    /// only the earliest one. Live-reported before this fix: six iterations
    /// submitted back to back with `checkin_every: 1` were all accepted, none paused
    /// the run.
    fn checkin_cadence_of_one_pauses_every_single_iteration() {
        let mut spec = plan_only_spec("run-tight-cadence", None);
        let mut state = RunState::new("run-tight-cadence");
        let criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 3, checkin_every: 1 };

        for i in 1..=3 {
            state.paused = false; // the real effect of an operator resuming/acknowledging between iterations
            let outcome = run_iteration(&mut spec, &mut state, record(i, true, vec![]), &criteria);
            assert_eq!(outcome, RunOutcome::CheckinDue, "iteration {i} of a checkin_every:1 run must be CheckinDue");
            assert!(state.paused, "iteration {i} must pause the run for real");
        }
        assert_eq!(state.history.len(), 3);
    }

    #[test]
    fn checkin_pending_is_false_before_the_first_real_boundary_is_crossed() {
        let mut state = RunState::new("run-x");
        state.criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 3, checkin_every: 5 };
        for i in 1..5 {
            state.history.push(record(i, true, vec![]));
        }
        assert!(!checkin_pending(&state), "iteration 4 of 5 hasn't crossed the boundary yet");
    }

    #[test]
    fn checkin_pending_stays_true_across_further_iterations_until_explicitly_acknowledged() {
        let mut state = RunState::new("run-x");
        state.criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 3, checkin_every: 5 };
        for i in 1..=7 {
            state.history.push(record(i, true, vec![]));
        }
        // The real gap this closes: two whole iterations (6, 7) have passed since
        // the boundary at 5 fired, with zero human acknowledgment -- this must
        // still read as pending, not silently reset just because more iterations
        // ran.
        assert!(checkin_pending(&state), "iteration 5's boundary was crossed and never acknowledged");

        state.checkin_acknowledged_through = 5;
        assert!(!checkin_pending(&state), "acknowledging through the real boundary that fired clears it");
    }

    #[test]
    fn checkin_pending_re_flags_on_a_later_boundary_even_after_an_earlier_one_was_acknowledged() {
        let mut state = RunState::new("run-x");
        state.criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 3, checkin_every: 5 };
        for i in 1..=5 {
            state.history.push(record(i, true, vec![]));
        }
        state.checkin_acknowledged_through = 5;
        assert!(!checkin_pending(&state));

        for i in 6..=10 {
            state.history.push(record(i, true, vec![]));
        }
        assert!(checkin_pending(&state), "a genuinely later boundary (10) must re-flag, not stay silently satisfied by an earlier acknowledgment forever");
    }

    #[test]
    fn checkin_pending_is_false_when_the_cadence_is_disabled() {
        let mut state = RunState::new("run-x");
        state.criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 3, checkin_every: 0 };
        for i in 1..=10 {
            state.history.push(record(i, true, vec![]));
        }
        assert!(!checkin_pending(&state), "checkin_every: 0 has no real boundary to cross, mirroring should_checkin's own fallback");
    }

    #[test]
    fn consecutive_failures_abort_the_run() {
        let mut spec = plan_only_spec("run-x", None);
        let mut state = RunState::new("run-x");
        let criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 2, checkin_every: 5 };

        assert_eq!(run_iteration(&mut spec, &mut state, record(1, false, vec![]), &criteria), RunOutcome::Continue);
        assert_eq!(run_iteration(&mut spec, &mut state, record(2, false, vec![]), &criteria), RunOutcome::Abort);
        assert_eq!(state.consecutive_failures, 2);
        assert!(state.paused, "a real Abort must actually pause the run, not just report the outcome string");
    }

    #[test]
    /// Real gap found live 2026-08-06 (stress-test run 47): `RunOutcome::Abort` used
    /// to be purely advisory. Live-confirmed before this fix, against the actual
    /// deployment, not just this hermetic test: with max_iterations:2, iteration 2
    /// correctly reported "outcome":"Abort", but iterations 3 and 4 were STILL
    /// accepted, growing history to 4 real entries -- double the configured bound,
    /// with `paused` never flipping. This project's own central architectural claim
    /// ("a bounded super loop") was genuinely not enforced at the one place that
    /// matters. Proves both real halves: the run pauses on abort, AND (by reusing
    /// the exact same `paused` flag `iterate_run`'s own existing `if
    /// run_state.paused { 409 }` check already gates on) a further iteration on an
    /// already-aborted run must be rejected the identical way a milestone-paused run
    /// already is -- this exact real-world call-site check is exercised by
    /// `iterate_run_rejects_further_iterations_once_the_run_has_aborted` in
    /// web/src/main.rs, not duplicated here.
    fn hitting_max_iterations_pauses_the_run_for_real() {
        let mut spec = plan_only_spec("run-x", None);
        let mut state = RunState::new("run-x");
        let criteria = AbortCriteria { max_iterations: 2, max_consecutive_failures: 3, checkin_every: 10 };

        assert_eq!(run_iteration(&mut spec, &mut state, record(1, true, vec![]), &criteria), RunOutcome::Continue);
        assert!(!state.paused, "must not pause before the real bound is actually reached");
        assert_eq!(run_iteration(&mut spec, &mut state, record(2, true, vec![]), &criteria), RunOutcome::Abort);
        assert!(state.paused, "reaching max_iterations must actually pause the run, not just report Abort");
        assert_eq!(state.pause_reason.as_deref(), Some("reached the 2-iteration limit"));
    }

    #[test]
    /// Real evaluator finding, issue #46: reproduces the exact live repro -- reach the
    /// ceiling, resume (the one real action `POST /resume` performs: clear `paused`,
    /// nothing else), and confirm the run is STILL refused. Before this fix,
    /// `run_iteration` itself had no opinion here -- only the HTTP handler's now-cleared
    /// `paused` flag stood in the way, so this exact sequence let iteration 2 through
    /// against `max_iterations: 1`.
    fn ceiling_already_reached_still_refuses_after_paused_is_cleared_by_resume() {
        let mut spec = plan_only_spec("run-resume-ceiling", None);
        let mut state = RunState::new("run-resume-ceiling");
        let criteria = AbortCriteria { max_iterations: 1, max_consecutive_failures: 3, checkin_every: 0 };

        assert!(ceiling_already_reached(&state, &criteria, true).is_none(), "a fresh run has real headroom");
        assert_eq!(run_iteration(&mut spec, &mut state, record(1, true, vec![]), &criteria), RunOutcome::Abort);
        assert!(state.paused);

        // The one real effect `POST /api/runs/{id}/resume` has: clear `paused`. It does
        // NOT raise the ceiling -- state.history/state.criteria are untouched.
        state.paused = false;

        // Unlike max_consecutive_failures, a succeeded:true submission is NOT an escape
        // here -- iteration count only ever grows, so `true` must still be refused.
        let reason = ceiling_already_reached(&state, &criteria, true);
        assert!(
            reason.is_some(),
            "a run already at its max_iterations ceiling must still be refused after paused is cleared, \
             not just while paused was true, and regardless of the incoming submission's own succeeded flag"
        );
        assert!(reason.unwrap().contains("1 of 1"), "the refusal must name the real, current count");
    }

    #[test]
    /// Sibling case, issue #46/#47: the same real gap for max_consecutive_failures,
    /// which `should_abort`/`run_iteration` treat identically to the iteration ceiling
    /// but which resume clears exactly the same way.
    fn ceiling_already_reached_also_refuses_on_consecutive_failures_after_resume() {
        let mut spec = plan_only_spec("run-resume-failures", None);
        let mut state = RunState::new("run-resume-failures");
        let criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 1, checkin_every: 0 };

        assert_eq!(run_iteration(&mut spec, &mut state, record(1, false, vec![]), &criteria), RunOutcome::Abort);
        assert!(state.paused);
        assert_eq!(state.consecutive_failures, 1);

        state.paused = false;

        // A further failed submission at the bound is still refused -- this remains a
        // real ceiling, not a blanket unlock the moment anyone resumes.
        let reason = ceiling_already_reached(&state, &criteria, false);
        assert!(reason.is_some(), "a run already past max_consecutive_failures must still be refused after resume");
        assert!(reason.unwrap().contains("consecutive failed"));
    }

    #[test]
    /// A run mid-way through its budget (not yet at either bound) must not be refused --
    /// this gate is the ceiling itself, not a blanket block on every submission.
    fn ceiling_already_reached_allows_a_run_with_real_headroom_left() {
        let mut state = RunState::new("run-headroom");
        state.history.push(record(1, true, vec![]));
        state.consecutive_failures = 1;
        let criteria = AbortCriteria { max_iterations: 5, max_consecutive_failures: 3, checkin_every: 0 };
        assert!(ceiling_already_reached(&state, &criteria, false).is_none());
    }

    #[test]
    /// Real evaluator finding, issue #47 (the follow-up deadlock): once
    /// consecutive_failures reaches the bound, a real succeeded:true submission --
    /// exactly the escape this gate's own error message has always promised -- must be
    /// let through, since applying it is what actually resets the streak. Before this
    /// fix, this exact submission got refused identically to a further failure, and the
    /// only working remedy was editing max_consecutive_failures itself.
    fn ceiling_already_reached_lets_a_succeeded_submission_through_to_reset_the_streak() {
        let mut state = RunState::new("run-recover");
        state.consecutive_failures = 1;
        let criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 1, checkin_every: 0 };

        assert!(
            ceiling_already_reached(&state, &criteria, true).is_none(),
            "a succeeded:true submission must be let through -- it's the documented, intended way to \
             recover from the consecutive-failure ceiling, not a bypass of it"
        );
        assert!(
            ceiling_already_reached(&state, &criteria, false).is_some(),
            "a further succeeded:false submission at the same bound must still be refused -- this stays \
             a real ceiling, not unlocked for every submission once reached"
        );
    }

    #[test]
    /// The full real recovery, end to end: submitting the succeeded:true iteration the
    /// gate now lets through must actually clear consecutive_failures, so the run is
    /// genuinely usable again, not just permitted through once.
    fn a_real_succeeded_submission_actually_recovers_the_run_from_the_ceiling() {
        let mut spec = plan_only_spec("run-real-recovery", None);
        let mut state = RunState::new("run-real-recovery");
        let criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 1, checkin_every: 0 };

        assert_eq!(run_iteration(&mut spec, &mut state, record(1, false, vec![]), &criteria), RunOutcome::Abort);
        assert_eq!(state.consecutive_failures, 1);
        state.paused = false; // the real Resume action

        assert!(ceiling_already_reached(&state, &criteria, true).is_none(), "the gate must let the real recovery submission through");
        run_iteration(&mut spec, &mut state, record(2, true, vec![]), &criteria);
        assert_eq!(state.consecutive_failures, 0, "the streak must actually be cleared by the real success, not just permitted past the gate");
        assert!(
            ceiling_already_reached(&state, &criteria, false).is_none(),
            "the run must be genuinely usable again afterward, not just for the one recovery submission"
        );
    }

    #[test]
    /// Issue #39 suggestion #3, the gating half: a real unanswered decision must
    /// refuse the ONE submission that would consume the run's last remaining slot,
    /// but not an earlier one with real headroom still left.
    fn final_iteration_blocked_by_unanswered_decision_only_refuses_the_actual_final_slot() {
        let mut state = RunState::new("run-final-slot");
        state.pending_decisions.push(PendingDecision {
            id: "d1".into(),
            question: "should this run ever support offline delivery?".into(),
            options: None,
            asked_by_iteration: 1,
            asked_by_iteration_id: None,
            asked_at: 1,
            answer: None,
            answered_at: None,
            answered_by: None,
        });
        // 2 of 3 max_iterations already used -- the NEXT submission (the 3rd) would be
        // the final slot.
        state.history.push(record(1, true, vec![]));
        state.history.push(record(2, true, vec![]));
        let criteria = AbortCriteria { max_iterations: 3, max_consecutive_failures: 3, checkin_every: 0 };

        let reason = final_iteration_blocked_by_unanswered_decision(&state, &criteria);
        assert!(reason.is_some(), "the final-slot submission must be refused while a real decision sits unanswered");
        let reason = reason.unwrap();
        assert!(reason.contains("3 of 3"), "the refusal must name the real, current would-be count: {reason}");
        assert!(reason.contains("should this run ever support offline delivery?"), "the refusal must name the real open question, not just a count: {reason}");

        // Raise the ceiling by one -- the same submission is no longer the final slot,
        // so it must be let through despite the decision still being unanswered.
        let roomier = AbortCriteria { max_iterations: 4, max_consecutive_failures: 3, checkin_every: 0 };
        assert!(
            final_iteration_blocked_by_unanswered_decision(&state, &roomier).is_none(),
            "a submission that ISN'T the final slot must never be blocked by this gate, even with the same unanswered decision -- only the last slot is protected"
        );
    }

    #[test]
    fn final_iteration_blocked_by_unanswered_decision_allows_the_final_slot_once_answered() {
        let mut state = RunState::new("run-final-slot-answered");
        state.pending_decisions.push(PendingDecision {
            id: "d1".into(),
            question: "real question".into(),
            options: None,
            asked_by_iteration: 1,
            asked_by_iteration_id: None,
            asked_at: 1,
            answer: Some("real answer".into()),
            answered_at: Some(2),
            answered_by: Some("scimbe".into()),
        });
        state.history.push(record(1, true, vec![]));
        let criteria = AbortCriteria { max_iterations: 2, max_consecutive_failures: 3, checkin_every: 0 };

        assert!(
            final_iteration_blocked_by_unanswered_decision(&state, &criteria).is_none(),
            "an answered decision must never block the final slot -- only a genuinely unanswered one"
        );
    }

    #[test]
    fn final_iteration_blocked_by_unanswered_decision_is_none_with_no_decisions_at_all() {
        let mut state = RunState::new("run-final-slot-none");
        state.history.push(record(1, true, vec![]));
        let criteria = AbortCriteria { max_iterations: 2, max_consecutive_failures: 3, checkin_every: 0 };
        assert!(final_iteration_blocked_by_unanswered_decision(&state, &criteria).is_none());
    }

    #[test]
    /// Real "why paused" distinction (stress-test run 49): the milestone-achieve and
    /// abort-ceiling triggers must record genuinely different, honest reasons, not a
    /// shared generic string -- otherwise the whole point of this field (telling
    /// three real situations apart at a glance) is lost.
    fn consecutive_failures_and_milestone_achievement_record_distinct_real_reasons() {
        let mut spec = plan_only_spec("run-x", None);
        let mut state = RunState::new("run-x");
        let criteria = AbortCriteria { max_iterations: 20, max_consecutive_failures: 2, checkin_every: 10 };

        run_iteration(&mut spec, &mut state, record(1, false, vec![]), &criteria);
        run_iteration(&mut spec, &mut state, record(2, false, vec![]), &criteria);
        assert_eq!(state.pause_reason.as_deref(), Some("2 consecutive failed iterations (limit 2)"));

        let mut milestone_state = RunState::new("run-y");
        milestone_state.milestones.push(Milestone { description: "1:1 messaging works end to end".to_string(), achieved: false });
        toggle_milestone(&mut milestone_state, 0).expect("toggle a real milestone index");
        assert_eq!(milestone_state.pause_reason.as_deref(), Some("milestone achieved: 1:1 messaging works end to end"));
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
