# The Development System's goal (CADS-Tunnel#382)

This is the **only goal** "The Development System" is built toward, per the operator's own
framing (2026-08-05). Every future improvement to this pipeline is judged against this document,
not against local convenience. Written as a specification, not prose, per the goal's own first
principle: **specification first**.

## 1. Specification-first, deterministic regeneration

> The same detailed specification should lead to the same software fragment.

The spec is the source of truth; the running software is a *build artifact* of the spec, not the
other way around. A more detailed spec, re-run, should reproduce the same fragment — not a
plausible variant of it.

**Honest tension, not glossed over**: literal bit-for-bit determinism through an LLM generation
step is not achievable — LLMs are stochastic by construction, and this project's own role-fillers
are LLM agents. What *is* achievable, and is what this document commits to unless corrected:

- **Deterministic given a frozen spec + frozen toolchain**: once a spec detail is written down (a
  requirement, an acceptance criterion, a constraint), re-running the *build* step (compile, test,
  package) against unchanged inputs is already fully deterministic — this project already has that
  (hermetic Docker gates, pinned toolchains).
- **Convergent, not identical, at the generation step**: re-running an LLM role-filler against the
  *same* detailed spec should converge on a materially equivalent fragment (same requirements
  satisfied, same constraints honored, same acceptance criteria passing) — verified by the
  requirement/acceptance-criteria machinery already in `RunState`, not by diffing source text.
  Two independently-generated fragments that both satisfy the same detailed spec **are** "the same
  software fragment" in the sense that matters (correctness), even if their source text differs.
- **The lever for determinism is spec detail, not LLM constraint**: the more of the "how", not
  just the "what", is captured as spec (down to acceptance criteria specific enough to leave no
  real decision to the LLM), the closer generation gets to deterministic in practice. This is
  exactly §4.4 below.

**Confirmed with the operator (2026-08-05)**: behavioral/convergent determinism (above) is the
near-term, load-bearing definition — "same software fragment" means "same verified behavior
against the same detailed spec", not "same source bytes". Alongside that, LLM-free generation for
sufficiently-detailed spec slices is the **directional, longer-term aspiration** — the operator's
own words: *"even i do not believe that 3 is full possible"* — so this is pursued opportunistically
where a spec slice becomes detailed/stable enough to template or codegen deterministically, never
treated as a near-term requirement, and never blocks work on the convergent definition above. In
practice: as §3's provenance and §5's quality gate make specs detailed and stable enough that an
LLM role-filler's real decision space shrinks toward zero for a given slice, that slice becomes a
real candidate to replace with a deterministic generator — evaluated case by case, not assumed.

## 2. ECC skills bridge spec → fragment

`ecc-universal` (already installed, `ecc-plan-canvas` already load-bearing in this project's
check-in flow — see [Review a mandatory check-in](https://scimbe.github.io/CADS-devsystem-docs/how-to/review-a-checkin/))
ships a real skills catalog (`~/.npm-global/lib/node_modules/ecc-universal/skills/`) beyond
plan-canvas: `architecture-decision-records`, `api-design`, `blueprint` (construction-plan
generation with per-step self-contained context briefs and an adversarial review gate — the same
shape this goal wants for spec→fragment steps), and others. **Real gap**: this project uses exactly
one ECC surface (`ecc-plan-canvas`, for human review) and has never evaluated the rest of the
catalog for the spec-authoring and step-decomposition side of the pipeline. First concrete step:
audit the catalog against each pipeline stage (`plan`, `implement`, `test`, `review`, ...) for a
real fit, not a blanket "use everything".

## 3. LLM fills the spec at a high level; the user controls when/where

- The LLM's job: turn a high-level ask into a *detailed* spec (requirements, constraints,
  acceptance criteria) — filling in the "first draft" of details the user hasn't specified yet.
- The user's job: know, explicitly, which details were LLM-authored vs. user-authored, and freely
  override any of them in later iterations.
- **Real gap**: `Requirement` (`pipeline/src/runner.rs:137`) has no provenance field — no way to
  tell, today, whether a requirement or acceptance criterion was written by a human or proposed by
  an LLM role-filler. This blocks §4.2 and §4.4 outright and is the single highest-leverage schema
  change this goal implies.

## 4. User support

### 4.1 Current status
Already real and working: the run's health panel, `stalled_stages`, risk annotations (§ below),
the History panel. Matches the goal as-is.

### 4.2 The LLM's own decision basis, in SE terms
> Requirements, Milestones, Constraints — with all docs, all chat extracts.

**Real gap.** Today these live in *separate* panels (Requirements, Milestones, Backlog, History,
the assistant's own chat) with no single "here is everything that led to this decision" view. The
`zylos envelope`'s `constraints` field (`docs/role-contracts.md`) is the closest existing primitive
— it already threads "what the next stage must respect" between stages — but nothing surfaces it
alongside the actual chat/assistant exchange that produced it, in one place, for the user.

### 4.3 The user always leads
Structurally already true (every stage proposal needs approval or lands in a reviewable queue —
see [How the pipeline proposes and grows its own stages](https://scimbe.github.io/CADS-devsystem-docs/explanation/self-optimizing-pipeline/)).
What's *not* yet true: LLM agents "self-optimizing... the process itself, every iteration" — today
`devsystem.improve` proposes new *stages*, but nothing proposes improvements to the *process*
(e.g., "this run's check-ins are too sparse", "this role's acceptance criteria are too vague to be
deterministic per §1"). That's a real, distinct role this goal implies and none exists yet.

### 4.4 Requirements: always downloadable, always extensible toward determinism
- **Downloadable**: `GET /api/runs/{id}` already returns requirements as JSON (see
  [REST API reference](https://scimbe.github.io/CADS-devsystem-docs/reference/rest-api/)) — real,
  but not a *document* a non-technical stakeholder would want. Real gap: no
  `GET /api/runs/{id}/requirements/export` producing a real, human-readable spec document
  (Markdown/PDF) a user actually downloads and reads.
- **Always extensible without changing what's already there**: `Requirement.acceptance_criteria`
  is an unstructured `Vec<String>` today — appending detail is already non-breaking (the vector
  just grows), so this half of §4.4 already holds structurally. What's missing is §3's provenance
  field, so a user can tell which existing criteria are safe to leave alone vs. which are LLM
  guesses that should be tightened first.

## 5. The software fragment's own quality bar

German technical/legal quality-of-work standards, applied literally to what a role-filler ships,
not just referenced:

| Term | Working definition for this project | Where it's checked today |
|---|---|---|
| **Anerkannte Regeln der Technik** (recognized rules of engineering) | Follows this ecosystem's own established, load-bearing patterns (hermetic gates, no secrets in git, real tests over mocks) | Partially — `check-no-secrets.sh`, the hermetic gate; no single explicit gate role |
| **Stand der Technik** (state of the art) | Current, non-deprecated dependencies and idioms at time of delivery | Not checked — no dependency-freshness/deprecation gate exists |
| **Vertragsgemäße / Sachmangelfreie Leistung** (contract-conforming, defect-free delivery) | Satisfies every declared acceptance criterion, zero known open defects at delivery | Requirements/acceptance-criteria machinery exists; **partially checked as of `CADS-devsystem@9f9f5d2`** -- a `succeeded: true` iteration whose own feedback admits a known defect (`DEFECT_ADMISSION_PHRASES`) is now flagged as a real risk. Advisory, not a hard block, and beatable by phrasing a real defect without those exact words -- not the whole gap closed |
| **Fachgerecht / Fachmännisch** (professionally correct) | Passes the same review bar a competent human reviewer would apply | The `review` stage exists as a role; not mandatory for every change today |
| **Kunstgerecht** (in accordance with the craft) | Idiomatic to the language/framework, not just "works" | Not mechanically checked; currently relies on the role-filler's own judgment |
| **Kollektives Qualitätsverständnis** (collective quality understanding) | Meets what this project's own established conventions (this doc, `role-contracts.md`, CLAUDE.md files) already agree quality means | This document itself is a step toward making that explicit and checkable |

**Real gap, concrete and actionable**: none of the above is a *mandatory gate* today — a role-filler
can mark an iteration `succeeded: true` without passing through `review` or a dependency-freshness
check at all. The most direct next step toward this goal: make passing `devsystem.review` (or an
equivalent explicit quality-gate role) a structural precondition for a stage's iteration counting
as done, not just a role that can be filled if someone bids on it.

## 6. Tested and easily deployable

Already real and strong: hermetic Docker-based test gates throughout, `scripts/deploy-devsystem-web.sh`
/ `scripts/deploy-devsystem-assistant.sh` (real, reproducible, single-command redeploys with
readiness checks). Matches the goal as-is — no gap identified here.

## 7. Panels/windows (operator addendum, 2026-08-05)

Extends §4's user-support goal with concrete GUI requirements:

1. **Windows/panels must relate to the process actually needed right now** — not a fixed, always-
   identical panel set regardless of what stage/state a run is in. **Real gap**: today's GUI shows
   the same panel set (Runs, Roles, Requirements, Backlog, Milestones, History, RAG, Assistant,
   custom panels) for every run in every state — nothing hides an irrelevant panel or surfaces a
   more relevant one contextually (e.g. a run with zero requirements yet should foreground "add
   your first requirement", not bury it behind six other panels).
2. **Panel values must be editable by the user AND by `devsystem.assistant`** — today the assistant
   can only act through its fixed `Action` enum (`AddRequirement`, `ToggleBacklogItem`, ...,
   `pipeline/src/bin/devsystem_assistant.rs:248`), a real but narrow, pre-enumerated set. **Real
   gap**: no general "the assistant can edit whatever a human could edit in this panel" capability
   — every new editable field needs a new hand-written `Action` variant.
3. **A real overview of every agent used, its tokens, and its cost must exist.** **Real gap,
   confirmed by direct check**: no such view exists anywhere in `web/src/main.rs` or `index.html`
   today. `devsystem_assistant.rs` already parses real `usage` fields from the Claude CLI's JSON
   output (`tests::missing_usage_fields_default_to_zero_not_a_parse_failure` proves the field
   exists and is parsed) — the raw data is closer to hand than a blank slate, but it's parsed and
   discarded per-call, never persisted or aggregated per-run/per-agent.

## 8. Validation methodology: the incompetent-agent stress test and the DAU lens (operator
   mandate, 2026-08-05)

**The governing principle, stated by the operator directly**: *"It is the fault of the pipeline,
not the user of the pipeline, if the process leads him not to the perfect result."* Every gap in
this document is judged by this standard from here forward — a bad outcome is a missing or weak
gate, unclear guidance, or a process defect, fixed at the process level, never blamed on the
specific agent or human who hit it.

**Two adversarial validation lenses, both required, neither optional**:

- **The incompetent-agent stress test**: an LLM agent deliberately simulating the least competent
  realistic software engineer fills real pipeline roles (starting with `CADS-webconference-android`,
  explicitly authorized as a disposable proving ground — free to delete, overwrite, or reset as
  this test needs, since the goal is the *pipeline*, not this particular app). The pipeline's own
  gates (§5's quality bar, once real and mandatory — see gap #2) must catch what the agent misses
  and drive the *output* to senior-engineer quality regardless of the *filler's* competence. If the
  incompetent agent's bad output ships anyway, that is a real pipeline defect to fix, not evidence
  the test agent was "too bad" to use.
- **The DAU lens** (*Duemmster anzunehmender User* — the least competent user still assumed to
  engage in good faith): the human operator may have poor judgment, listen to the LLM's advice,
  weigh it, and sometimes choose wrong anyway. The GUI and `devsystem.assistant`'s guidance must
  still lead such a user toward a good outcome — catching and visibly flagging a bad choice (e.g. a
  vague requirement, a skipped review, an ignored risk annotation) rather than silently accepting
  it and letting a bad decision propagate unchallenged.

**Concrete, checkable success criterion** (translating "top 100 App Store level" into something
actually verifiable, since no real ranking is available to test against): the incompetent-agent run
produces an Android fragment that a real senior-engineer review would pass against every row of
§5's quality table, with genuinely polished UX (not scaffold-level), zero known open defects at
delivery, and full test coverage of its own acceptance criteria — not "compiles and has a
scaffold's worth of tests," which is roughly where `CADS-webconference-android` sits today.

**Standing mandate, not a bounded task**: *"do not stop until it is reached."* Honored via the
recurring dev-loop mechanism (a session-scoped, ~5-minute-cadence loop; the operator was told
directly that true unattended persistence past this session's lifetime needs a cloud schedule, not
just a session-only cron) — each firing lands one real, bounded, hermetically-tested increment
toward the ranked gaps below, not a single unbounded attempt to reach the whole goal at once.

**The stress test's first real run, 2026-08-05**: rather than only design the incompetent-agent
persona on paper, actually played it against a live, dedicated test run
(`stress-incompetent-agent`) -- added a real requirement, declared `review`, then submitted the
laziest realistic "review" an incompetent (or simply lazy) agent would produce: `feedback: "looks
fine to me"`, `succeeded: true`, correctly naming the requirement. **It worked** — the gate accepted
it and the requirement was marked verified, exactly the failure mode this whole methodology exists
to catch. Fixed for real: `toggle_requirement` now requires at least one qualifying review to clear
a minimum feedback length before the gate is satisfied (`CADS-devsystem@7622c95`) — an honestly
crude, mechanical proxy (filters trivially-empty rubber-stamps; does **not** verify real review
quality — a longer-but-still-padded lazy review remains a real, known, undefended gap). Re-verified
live against the actual deployment after shipping: a `"lgtm"` review is now a real `409`. This is the
methodology proving itself on the very first real attempt — not a demonstration that was staged to
succeed.

**The stress test's second real run, same day**: tried the next realistic incompetent-agent move --
a vague requirement ("WHEN the user does anything, THE SYSTEM SHALL work correctly") with trivial
acceptance criteria: `"ok"`, `"."`, `"done"`, and (found along the way, not planned) a criterion that
was ONLY a zero-width space (U+200B) — invisible in the GUI, since `.trim()` doesn't strip it
(Unicode category Cf/Format, not White_Space). **All four sailed through** as real, checkable
criteria. Fixed for real: acceptance criteria now need a minimum count of alphanumeric characters,
not just non-empty content (`CADS-devsystem@425597c`) — one mechanical rule catches both the
trivial-word case and the invisible-character case, since the latter has zero alphanumeric
characters under this count. Re-verified live: `"ok"` is now a real `400`.

**A real DAU-lens finding on the human side, same day** (`CADS-devsystem@d40cbe5`): un-verifying a
requirement is always unconditional server-side, by design — loosening a claim never needs a review
to re-justify it. That also meant a single careless click on an already-verified requirement's
checkbox silently discarded that status with zero warning. The GUI now confirms before the
destructive direction only (un-checking); verifying stays a single click, and individual acceptance
criteria — routine, frequent bookkeeping, unlike the whole-requirement flag the review gate and the
Markdown export both treat as the headline status — are deliberately left alone.

**First real groundwork step, 2026-08-05 (iteration 8)**: the mandatory review gate (gap #2) only
has teeth on a run that actually declares `devsystem.review` as a role — `webconference-android`
itself never had. Added it for real via a `devsystem.improve` proposal, the same immediately-applied
self-optimizing path any role-filler uses (not a special-cased admin action) — checked first that
this run has zero declared `Requirement` objects yet, so the addition is purely forward-looking and
retroactively blocks nothing already in progress. This is the real precondition the stress test
itself needs before it can prove anything: without `review` declared, there is no gate to test
against on this actual project, only in hermetic tests.

**Real infrastructure incident found and fixed along the way, same day**: multiple autonomous loops
now firing concurrently means multiple `deploy-devsystem-web.sh` invocations can race each other's
`docker build` against the same BuildKit cache mount. Real evidence: a `devsystem-demo.
bunsenbrenner.org` API round-trip (add a requirement, declare `review`, toggle before any review
iteration exists) returned `200`/`verified:true` instead of the expected `409` block, and a deploy
run's own logged output named a different image manifest hash than what `docker images` showed as
actually tagged moments later -- two builds landed close together and the wrong one won. Fixed with
a `flock` serializing real invocations of the script (`CADS-devsystem@2914d91`).

**Self-correction, same investigation**: the first attempt to *confirm* the stale binary used
`docker exec devsystem-web strings /app/devsystem-web | grep ...`, which reported zero matches for
newly-added code -- taken at the time as proof of staleness. It wasn't real evidence: `strings`
isn't installed at all in this container's `debian:trixie-slim` runtime base (confirmed directly,
`which strings` returns empty), so every one of those checks silently failed and reported zero
matches regardless of the binary's actual contents. The underlying diagnosis (a real race, evidenced
above) still holds and the fix is still correct -- but the specific "confirmed via strings" claim in
an earlier version of this section was false and has been corrected here. After the `flock` fix and
a clean rebuild+redeploy, the gate was re-verified with a real functional HTTP round-trip against
`127.0.0.1:8790` directly (bypassing the public gate cookie entirely) and now correctly returns
`409` before a review and `200` after one. Worth stating plainly, both halves: the race itself is
exactly the class of failure §8's own governing principle is about (a silent process gap, not a
competence failure of whichever loop triggered it) -- and so is trusting an unverified diagnostic
tool as if it were real evidence. Both get fixed, not just the first one.

**A second, related real bug found the same day, this time in actual application data, not just
infrastructure**: `webconference-android`'s own real history ended up with two entries both stored
as `"iteration": 8` (byte-identical stage/feedback/succeeded), and no `"iteration": 9"` at all --
found while checking in on this run's real state, not assumed. `write_lock` already serializes
concurrent `/iterate` requests *within* one `devsystem-web` process; it can't help against two
separate process instances each running their own independent lock, which the same overlapping-
deploy window made newly possible. Fixed at the data layer, not the infrastructure layer this time:
`/iterate` now rejects a submission that's byte-identical to the run's own immediately-preceding
entry with a real `409`, regardless of *why* a duplicate arrived (`CADS-devsystem@a12f135`). The
duplicate `"iteration": 8` entry itself was left in the run's real history rather than surgically
edited out -- it's an honest record of what actually happened, bug included; silently cleaning it up
after the fact would be its own small dishonesty. This makes three real, same-day findings from the
identical underlying cause (overlapping deploys): a stale binary, a false "confirmed" diagnostic
claim, and now duplicated application data -- worth naming as a pattern, not three unrelated bugs.

**The actual root cause of that whole pattern, found and fixed the same day**: `web/Dockerfile`'s
BuildKit cache mount targets `/work/target`, but neither of its two `cargo build --manifest-path`
invocations (`web/Cargo.toml`, `pipeline/Cargo.toml`) writes there by default -- each defaults to
`<manifest-dir>/target` (`/work/web/target`, `/work/pipeline/target`), confirmed directly against
the `cp` commands that have always read from exactly those paths, never `/work/target`. The mount
was caching an empty, unused directory -- **every single deploy this whole session recompiled the
full dependency tree from scratch**, regardless of how small the source change, which is the real
reason builds kept taking 5-15+ minutes and why the overlapping-deploy race window above was wide
enough to matter at all. Fixed with `ENV CARGO_TARGET_DIR=/work/target`, redirecting both cargo
invocations' real output to the directory the mount actually is (`CADS-devsystem@afaf021`). This is
the highest-leverage infrastructure fix of the whole day's incident cluster: every other fix (the
`flock`, the idempotency guard) treats a symptom of slow, overlap-prone deploys; this one shrinks
the window itself.

**Measured, not assumed**: a genuinely cold build (the cache mount empty for the first time since
this fix) took 7m1s. An immediate rebuild with zero source changes: 0.9s (Docker's own layer cache).
A rebuild after touching one line of `web/src/main.rs` -- forcing Docker's own layer cache to
invalidate, isolating what the BuildKit cache mount specifically contributes: **45s, roughly 9x
faster than cold**, recompiling only what actually changed instead of the full dependency tree.

**A real DAU-lens gap found while confirming `webconference-android`'s own M1 milestone, 2026-08-05**:
toggling a milestone to `achieved` has always auto-paused the run (`RunState.paused`, real, tested
code) -- but nothing on the docs site explained this anywhere. A DAU who hits it cold (submits
work, watches the run silently stop proposing, sees a banner with no context) has no way to know
this is expected, not broken. Fixed as a docs gap, not a code gap -- the behavior itself is correct
and intentional (exactly the "periodic check-ins" design this section already calls for), it just
had zero user-facing explanation. Closed:
[CADS-devsystem-docs@9bfb1af](https://github.com/scimbe/CADS-devsystem-docs/commit/9bfb1af), with a
real, live screenshot captured at the exact moment M1 was confirmed for real (see issue #13's
resolution below), not staged after the fact.

**Issue #13 (Android emulator walkthrough, labor-setup.com) genuinely closed, 2026-08-05**: reviewed
their evidence for real, not a rubber stamp -- cross-checked both submitted screenshots against each
other (device B's own identity exactly matches the peer field device A shows; the sent/received
message text is identical on both ends; both show a real Noise_IK session established) before
merging the evidence branch and toggling M1 achieved via the run's real API. This is the run's own
declared success criterion for the stress-test/DAU-lens groundwork above, now actually met, not just
asserted.

**Issue #14 (document extraction, labor-setup.com) real first increment merged, 2026-08-05**: an
earlier iteration of this same loop had mistakenly closed labor-setup.com's real PR #15, believing
it was a second automated persona duplicating the standing "don't build a competing extraction
agent" instruction -- it wasn't, it was their own real work under an explicit go-ahead already given
on this issue. Corrected: reopened, independently re-verified (cherry-picked the one real content
commit in isolation, 7/7 hermetic tests green, a genuine end-to-end run against a hand-built PDF via
real `pdftotext` producing exact real extracted text, their embeddings/vector-storage correction
checked directly against `vector_store.rs`/`rag.rs` and confirmed accurate), full CI green, then
merged for real: `devsystem_document_extraction_handler` (PDF-only) is now in `main`
([CADS-devsystem@0937992](https://github.com/scimbe/CADS-devsystem/commit/0937992)). DOCX/image/OCR
remain open with them, per the standing hand-off.

**The stress test's third real run, 2026-08-05**: went after the exact gap the first run's own
writeup had already named and left open -- "a longer-but-still-padded lazy review remains a real,
known, undefended gap." Added a fresh requirement to `stress-incompetent-agent`, then submitted
`feedback: "looks good looks good looks good looks good"` (45 characters, well past the existing
25-character minimum), `succeeded: true`, correctly naming the requirement. **It worked again** --
a real `200`, requirement marked verified, length alone unable to tell padded filler from real
scrutiny. Fixed for real: a second, complementary mechanical bar,
`MIN_REVIEW_DISTINCT_WORDS = 8` (`distinct_word_count`, case-insensitive, punctuation-collapsing) --
both the length and distinct-word bars must clear now
(`CADS-devsystem@b25e975`). Re-verified live against the actual deployment after shipping: the
exact same padded-review string that got a `200` before now gets a real `409`; a genuinely
substantive review submitted right after still gets a clean `200`. Honestly scoped, same as the
first fix: a generic-but-varied review ("looks good, works fine, nothing to flag, all clear here")
would still clear both bars without being real scrutiny -- a real, known, still-undefended gap,
named here rather than papered over.

**The stress test's fourth real run, 2026-08-05**: went after the exact remaining gap the third
run's own writeup had just named -- a generic-but-varied review would still clear both mechanical
bars. Tried the next realistic move: copy-paste a real, substantive review's feedback (the
device-rotation requirement's own genuine review text) and reuse it verbatim to "review" a
completely unrelated requirement (network-retry-on-send), naming that different requirement in
`requirement_indices` instead. **It worked again** -- a real `200`, both the length and distinct-
word bars passing trivially since the reused text genuinely was long and varied, just not actually
about the requirement it was applied to. Fixed for real: a qualifying review's feedback must not be
byte-identical (trimmed) to another successful `devsystem.review` iteration in this run's history
that named a different, non-overlapping set of requirements (`same_requirement_set`, a real
`HashSet` comparison) (`CADS-devsystem@187c4ac`). Re-verified live against the actual deployment
after shipping: the exact same reused-verbatim feedback that got a `200` before now gets a real
`409`; a genuinely distinct review of the same requirement still gets a clean `200`. Four real
stress-test runs, four real gaps found and closed the same day -- each one specifically the gap the
previous run's own writeup had honestly flagged as still open, not a new speculative worry.

**The stress test's fifth real run, 2026-08-05**: diversified beyond the review gate into the
OTHER real risk check this pipeline already had -- `missing_test_before_implement` (proposal §5's
own example: "no test stage before implement"). It only ever asked *whether* a `devsystem.test`
record existed before implement, never whether it had any real substance. Live-verified before this
fix: a rubber-stamp `feedback: "tests pass"` `devsystem.test` iteration silently made the risk
annotation vanish, then a real `devsystem.implement` iteration whose own feedback honestly admitted
"no actual tests written for it" produced zero risk findings on the run. Fixed the same way as the
review gate: a `devsystem.test` record only counts as real evidence testing happened if its feedback
clears the same two mechanical substance bars (25+ characters, 8+ distinct words --
`distinct_word_count` reused directly, not reimplemented) (`CADS-devsystem@49d6544`). Re-verified
live against the actual deployment after shipping, against the exact same run already used to prove
the gap: the same historical data that showed zero risks before this fix now correctly surfaces the
risk annotation, no resubmission needed. Five real stress-test runs, five real gaps found and closed
the same day.

**A real DAU-lens finding, 2026-08-05**: rejecting a stage/panel/issue proposal is exactly as
permanent as removing a custom panel or un-verifying a requirement -- `reject_stage_proposal`'s own
doc comment says it plainly ("there's nothing to undo beyond removing it from the pending list") --
but only those other two destructive actions asked for confirmation first. Live-verified before this
fix: a real pending proposal, one click on Reject, gone with zero trace anywhere in the run's state.
Fixed all three reject buttons (panel, stage, issue) with a specific `confirm()` naming exactly
what's being discarded (`CADS-devsystem@645a88d`). Verified live via a real Playwright browser, not
just curl: the real dialog names the exact stage, cancelling genuinely preserves the proposal,
accepting genuinely discards it.

**A second real DAU-lens finding, same day, same sweep**: removing an indexed RAG document had the
identical unconfirmed-permanent-delete shape -- arguably worse, since an uploaded PDF/DOCX/image's
original bytes aren't kept anywhere in this index at all, so re-adding it after an accidental click
means finding and re-uploading the real file again, not a quick undo. Fixed the same way (real
`confirm()` naming the exact document path) (`CADS-devsystem@8dc5f49`). Verified live via a real
Playwright browser: added a real manual document, the dialog names the exact file, cancel preserves
it, accept removes it.

**A third real DAU-lens finding, same sweep, different shape**: marking a memory entry "reviewed"
(`Trust::Governed`) is a real, one-directional attestation -- `govern_memory_entry`'s own doc
comment confirms it, and live-verified there's no un-govern route anywhere (calling it twice just
no-ops). Different from the first two findings -- no data is lost, only a trust flag flips -- but
the same permanence-with-no-undo shape: a careless click permanently records "a human reviewed
this" against an entry nobody actually reviewed. Fixed the same way, honestly phrased as an
attestation rather than data loss (`CADS-devsystem@0622996`). Verified live via a real Playwright
browser: the dialog names the exact stage/role, cancel leaves it genuinely "unreviewed", accept
genuinely flips it to "governed".

**The stress test's sixth real run, 2026-08-05, a different flavor**: rather than simulating a lazy
agent's next move, went after a gap §5's own quality-bar table already named directly --
Vertragsgemäße/Sachmangelfreie's "nothing blocks marking work 'done' with open, known defects."
Live-verified before this fix: a `devsystem.implement` iteration marked `succeeded: true` whose own
feedback said *"Known issue: crashes on a null id, not fixed yet, workaround needed"* produced zero
risk findings -- the pipeline had no way to notice an iteration contradicting itself. Fixed with a
new preflight check, same crude-but-honest mechanical spirit as `SECURITY_KEYWORDS`:
`DEFECT_ADMISSION_PHRASES`, six specific multi-word phrases, only fires on `succeeded: true` (a
FAILED iteration honestly saying it's broken is the behavior this wants to encourage, not flag)
(`CADS-devsystem@9f9f5d2`). Re-verified live against the exact run already used to prove the gap:
the same historical data that showed no relevant risk before now correctly surfaces it. Advisory,
not a hard gate, and honestly beatable by different phrasing -- named as such, not oversold.

**The stress test's seventh real run, 2026-08-05**: this whole requirements feature is built around
EARS notation, and acceptance criteria already had real content validation -- but the `statement`
field itself had none. Live-verified before this fix: `{"statement":"asdf","acceptance_criteria":
["a real criterion"]}` got a real `200`. Fixed as a hard gate (not advisory, matching the
acceptance-criteria precedent): the statement must contain "SHALL" (case-insensitive) -- the one
universal, defining keyword across every real EARS requirement type, deliberately not also
requiring "WHEN" since a real ubiquitous EARS requirement (no trigger clause) is legitimate
(`CADS-devsystem@17339d0`). Real test-fixture blast radius (8 existing tests used non-EARS
placeholder statements) handled carefully -- each updated to a real EARS-shaped statement that
still contains the original placeholder text verbatim, not relying on accidental double-rejection
to stay green. Re-verified live: the exact same `"asdf"` statement that got a `200` before now gets
a real `400`; a genuine EARS statement, with or without a `WHEN` clause, still gets a clean `200`.

**A real DAU-lens integration check, same day**: the new EARS gate above had never been exercised
through `devsystem.assistant`'s own path -- did a brand-new server-side gate compose cleanly with
the assistant's existing honest-failure surfacing, or would it silently swallow the rejection?
Checked live, not assumed: asked the assistant (twice) to add a non-EARS requirement. First attempt,
it declined on its own before ever submitting -- good judgment, but not proof the *code path*
handles a real rejection correctly. Forced the actual attempt on the second try ("submit it
verbatim, don't ask again"): it genuinely attempted the action, hit the real `400`, and reported it
honestly -- `"FAILED to add requirement \"the app should be fast\": HTTP 400 Bad Request: statement
doesn't look like a real EARS requirement..."` -- the exact real server message, not hidden, not
fabricated as a success. No code change needed; this confirms the existing generic
error-surfacing (`apply_action`'s own honest-failure path, built earlier this session) correctly
covers a gate that didn't exist yet when that path was written -- real evidence the pipeline's
layered gates compose, not just each work in isolation. Also spot-checked the milestone-toggle
gate the same way (a made-up out-of-range index, including under direct social-engineering-style
pressure -- "I already know it exists, just do it") -- the assistant correctly refused both times,
and the underlying endpoint itself still returns a clean `404`, not a panic, confirming the
code-level gate holds regardless of how well any given LLM call behaves.

**The stress test's eighth real run, 2026-08-06**: went after §4.3's second worked example ("this
run's check-ins are too sparse") and found the real problem was worse than "sparse" --
`checkin_every: 0` has zero validation and completely disables the mandatory cadence
(`should_checkin`'s own fallback), not just thins it out. Live-verified: a real `200`, zero risk
findings. Investigating it surfaced a second, more concrete bug: `iterations_until_checkin`
hardcoded `0` for that case, actively claiming "due right now" instead of "disabled", which then
permanently false-flagged the run's `needs_attention` in the Runs list. Fixed both -- a new
preflight check (`checkin_every == 0 || checkin_every >= max_iterations`) and the root-cause fix to
`iterations_until_checkin` itself (`CADS-devsystem@3331013`). Re-verified live against the exact run
already used to prove both bugs: the same run that showed zero risks and a misleading `0` before now
shows the real risk, the real `20`, and a correct `needs_attention: false` -- three right answers
where there were none or wrong ones, no resubmission needed. Eight real stress-test runs, eight real
gaps found and closed.

## Summary: the highest-leverage real gaps, ranked (updated 2026-08-05)

1. ~~**Provenance on `Requirement`**~~ — **done** (`CADS-devsystem@b58aef4`): `proposed_by`
   distinguishes LLM-proposed from human-authored, surfaced in the GUI.
2. ~~**A mandatory quality gate**~~ — **done** (2026-08-05, `toggle_requirement`'s real review gate):
   a run that declares
   a real `devsystem.review` role can no longer mark a requirement verified without a real,
   successful `devsystem.review` iteration that actually names it (`requirement_indices`) in
   history — a hard 409 block, not an advisory annotation. Scoped to runs that opt `review` into
   their own spec (`plan_only_spec`, what every new run starts as, has no such role, so this never
   blocks a run that hasn't declared it). Along the way, found and fixed a real related DAU-lens
   bug: the Requirements panel's checkbox stayed visually "checked" after a blocked toggle,
   silently misleading whoever's looking at it — now reverts to the real state on rejection.
   Still open for a later increment: this is one concrete slice of §5's quality bar (review actually
   happened), not the whole table (Stand der Technik / dependency freshness, Kunstgerecht /
   idiomatic-code checks, etc. remain unenforced).
3. **Context-relevant panels** (§7.1) — show what this run's actual state needs, not a fixed set.
   **First slice done** (`CADS-devsystem@de56d33`): the Pipeline chip on the panel toggle bar now
   carries a real badge with the run's actual pending-proposal count (stage + panel + issue
   proposals combined) -- a run with something awaiting approval no longer looks identical, from
   the toggle bar, to one with nothing outstanding. Found and fixed a real bug while verifying this
   live, not assumed correct from the diff: the toggle bar was never re-rendered after `selectRun`
   or any of the six proposal approve/reject handlers updated the run's data, so the badge would
   either never show on first opening a run or go stale after approving. Verified live both
   directions (badge appears on a real pending proposal, disappears on a real approve) via
   screenshots against a scratch run.
   **Second slice done** (`CADS-devsystem@29f3ad4`): a genuinely empty run's Requirements panel now
   shows an explicit first-action callout ("👋 Start here...") instead of a plain, easy-to-skim-past
   "No requirements yet." line, and auto-focuses the statement field -- exactly the concrete example
   §7.1 itself names. Guarded against the real regression this kind of fix invites: only steals focus
   when the panel is actually visible, and `requirements` isn't in `REFRESHABLE_PANELS`, so a
   periodic auto-refresh can never re-steal focus from wherever the user is actually working.
   Verified live both ways: a fresh run's statement input is genuinely focused (confirmed via
   `document.activeElement`), an existing run with requirements shows no banner. Still open: hiding
   panels genuinely irrelevant to the current stage (the rest of §7.1's ask) remains unaddressed.
4. **Assistant-editable panel values generally** (§7.2) — beyond the current fixed `Action` enum.
   **First slice done** (`CADS-devsystem@920f66e`): a human could already toggle one acceptance
   criterion independently of the whole requirement (`toggle_acceptance_criterion_handler`, the
   Requirements panel's per-criterion checkboxes); the assistant had no matching action at all
   until now. Verified live end to end against the actually-running `devsystem_assistant --serve`
   process (not just the unit suite): a real `/ask` call asking to toggle one specific criterion
   made the LLM correctly choose the new `toggle_acceptance_criterion` action, which dispatched to
   the real endpoint and actually flipped `verified_criteria[0]` from `true` to `false` on a live
   run. **Self-correction, 2026-08-05**: this entry previously claimed "the Backlog panel's items
   ... still have no assistant-editable path at all" -- checked directly against the actual `Action`
   enum before writing that, it was wrong. `AddBacklogItem`/`ToggleBacklogItem` already existed
   (`CADS-devsystem@920f66e` and earlier) and work end to end -- re-verified live just now via a
   real `/ask` call ("Add a real backlog item...") that dispatched correctly and actually appended
   to `state.backlog`. Corrected here rather than left stale. `RunState.repo_url` is also already
   fully covered by `set_repo_url` -- not a real gap either, just imprecise phrasing.

   The one genuinely still-open piece: **custom-panel removal/editing**. Checked directly: the
   assistant can `propose_custom_panel` (a new one, gated behind human approval) but has no action
   at all for removing or editing an EXISTING one, even though a human can
   (`POST /api/runs/{id}/panels/{panel_id}/remove`, now with its own real confirmation dialog,
   `CADS-devsystem@645a88d`'s sibling fix). Deliberately not built this cycle: unlike
   `ToggleAcceptanceCriterion` (safe, reversible, additive-in-effect), removing a panel is
   destructive and irreversible the same way a human's own confirm dialog exists to guard against
   -- giving the assistant that power as a *direct*, immediately-applied action (this enum's
   established pattern for safe/reversible actions) would be the wrong trust model, matching the
   same reasoning `ProposeCustomPanel`'s own doc comment already gives for why ADDING one is gated
   behind approval, not immediate. The honest next slice isn't "add a `RemoveCustomPanel` action" --
   it's designing a real pending-removal-proposal mechanism first, sized as its own increment, not
   assumed away.
5. ~~**An agents/tokens/costs overview**~~ — **done** (`CADS-devsystem@19c03ef` backend,
   `705b30e` GUI): `RunState.assistant_usage` persists real running totals (call count,
   input/output/cache tokens, `total_cost_usd`) on every real `/ask` call, and a real Assistant
   Usage panel (registered the same way every other panel is, auto-refreshable) now shows real
   cumulative cost + a token breakdown instead of raw JSON. Confirmed live: deployed in 14s (only
   the static file changed, no rebuild needed) and the panel's real markers verified present in the
   served page.
6. **A unified decision-basis view** (§4.2) — **first slice done** (`CADS-devsystem@cfaac7d`): each
   requirement's Requirements-panel entry now expands into a real "decision basis" -- the actual
   feedback and real constraints from every iteration that claimed to address it, right there,
   instead of sending someone to piece it together from the separate History/Memory Log panels.
   **Second slice done** (`CADS-devsystem@b124cd9`): the real structural blocker on the "chat/docs"
   half -- zero chat-exchange persistence existed anywhere -- is closed. `RunState.chat_history`
   (bounded, rolling, `MAX_CHAT_HISTORY = 50`) now persists every real `/ask` exchange's actual
   instruction and response, reusing gap #5's own established pattern. Re-read gap #5's own doc
   comment before starting, rather than trusting it: it had assumed a full chat replay "already
   exists, informally" -- that was always the browser's own ephemeral tab, never persisted
   server-side; closing the tab lost it for good. Verified live through the real
   `/api/runs/{id}/assistant` proxy route (not the assistant bridge's own `/ask` directly -- caught
   and corrected that exact mistake in this same increment): two real exchanges persist in order
   with their real text. Still open: this is the persistence half only -- the decision-basis view
   itself doesn't pull `chat_history` in yet, so a requirement's expandable panel still shows only
   iteration history, not chat exchanges. GUI wiring is the real next slice, not claimed done here.
   **Third slice done** (`CADS-devsystem@78521b2`): surfaced `chat_history` for real -- honestly
   scoped, not forced into the per-requirement view above. A `ChatExchange` has no field linking it
   to a specific requirement, and never will without either a fragile text-match heuristic or a real
   schema change touching `Requirement` and the assistant's own action-dispatch code -- both risk
   showing a WRONG decision basis, worse than none. Instead: a real "Recent assistant conversation"
   section at the Requirements panel level, most-recent-first. Verified live via a real Playwright
   browser against the exact run persistence was proven on last firing: the real section renders
   both real exchanges with their real timestamps and text. Still open: true per-requirement
   attribution -- the honest, harder version of this gap -- remains unbuilt, named as such.
7. ~~**A real requirements export**~~ — **done** (`CADS-devsystem@950931a`): `GET
   /api/runs/{id}/requirements/export` renders a real Markdown document (statement, a real
   verified checklist per acceptance criterion, provenance from gap #1), real `Content-Disposition`
   so a browser actually downloads it, and a "Download as Markdown" link in the Requirements panel.
   Verified live against both an empty run (webconference-android, honestly "no requirements
   defined yet") and a populated one.
8. ~~**ECC skills catalog audit**~~ — **audited** (`docs/ecc-skills-audit.md`, 2026-08-05): real
   fits found for `plan` (`architecture-decision-records`), `implement` (`android-clean-architecture`),
   `test` (`rust-testing`/`kotlin-testing`), `review` (`security-review`), `improve`
   (`agent-architecture-audit`); `remember` deliberately declined (the zylos envelope already solves
   a different problem than ECC's generic cross-harness memory vault); `verify`'s candidate
   (`verification-loop`) is JS-toolchain-specific and doesn't apply. Also found `loop-design-check`
   — built specifically to review autonomous agent loops for exactly this project's own failure
   class (a silent process gap producing a wrong result, no visible error — precisely what the
   same-day deploy race was) — the single most relevant unadopted skill to "The Development System"
   as a whole, not any one stage. Audit only; none of these are wired into the pipeline yet — real,
   separate follow-up work, sized by the operator.
9. **A `devsystem.process_improve` role** (§4.3) — **first slice done** (`CADS-devsystem@57f2ca9`):
   `process_annotations(spec, state)`, a new process-level dimension alongside `preflight_annotations`
   (needs the live `PipelineSpec`, not just history) — flags a run with 3+ real successful
   iterations that has never declared a `devsystem.review` role, since gap #2's own mandatory
   review gate is silently a no-op until `review` is declared. Verified live against both a real
   positive (a fresh test run, correctly flagged) and negative (the real `webconference-android`
   run, which already declared `review` in iteration 8 — correctly shows no risk). Still open: this
   is one mechanical check, not a real `devsystem.process_improve` *role* a filler could bid on and
   actively propose process changes through — that's the fuller version of this gap, not claimed
   done here.
   **Second slice done** (`CADS-devsystem@3331013`): the second worked example §4.3's own text
   names -- "this run's check-ins are too sparse" -- now has a real check too:
   `checkin_every == 0 || checkin_every >= max_iterations` flags a mandatory cadence that can never
   actually fire before the run's own hard iteration ceiling does. Found a real, more concrete bug
   investigating this one: `iterations_until_checkin` hardcoded `0` for the `checkin_every == 0`
   case, actively misrepresenting "disabled" as "due right now" and permanently false-flagging such
   a run's `needs_attention` in the Runs list for a reason that was never real -- fixed at the root
   (report the real ceiling distance), not just papered over with the new risk annotation. Re-
   verified live against the exact run used to prove both bugs: three correct verdicts where there
   were none or wrong ones before, no resubmission needed.

This ranking is a proposal, not a decision — the operator leads (§4.3).
