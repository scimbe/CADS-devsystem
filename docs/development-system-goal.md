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
| **Vertragsgemäße / Sachmangelfreie Leistung** (contract-conforming, defect-free delivery) | Satisfies every declared acceptance criterion, zero known open defects at delivery | Requirements/acceptance-criteria machinery exists; nothing blocks marking work "done" with open, known defects |
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

## Summary: the highest-leverage real gaps, ranked

1. **Provenance on `Requirement`** (§3, blocks §4.2 and §4.4) — LLM-authored vs. user-authored,
   per requirement and per acceptance criterion.
2. **A mandatory quality gate** before an iteration counts as done (§5) — review/dependency-freshness/
   defect-free-at-delivery, not just "a role that exists if someone fills it".
3. **A unified decision-basis view** (§4.2) — requirements + constraints + the actual chat/docs
   that produced them, in one place.
4. **A real requirements export** (§4.4) — a downloadable document, not just a JSON blob.
5. **ECC skills catalog audit** (§2) — beyond `ecc-plan-canvas`, for spec-authoring and
   step-decomposition.
6. **A `devsystem.process_improve` role** (§4.3) — self-optimizing the process itself, not just
   the stage list.

This ranking is a proposal, not a decision — the operator leads (§4.3).
