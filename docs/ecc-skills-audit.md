# ECC skills catalog audit (#382 goal doc §2, gap #8)

This project uses exactly one surface of `ecc-universal` today: `ecc-plan-canvas`, for the human
check-in channel (see [devsystem_checkin.rs](https://github.com/scimbe/CADS-devsystem/blob/main/pipeline/src/bin/devsystem_checkin.rs)
and the docs site's [Review a mandatory check-in](https://scimbe.github.io/CADS-devsystem-docs/how-to/review-a-checkin/)).
The catalog has 280 skills total (`~/.npm-global/lib/node_modules/ecc-universal/skills/`) — this is
a real audit against them, grepped and read directly, not assumed from names alone. Scope: does a
real skill fit a real pipeline stage or a real gap this project already has, honestly, including
"no, this doesn't apply" where that's the true answer.

## Per-pipeline-stage findings

| Stage | Candidate | Real fit? | Why |
|---|---|---|---|
| `plan` | `architecture-decision-records` | **Yes, concrete** | The `zylos envelope`'s `constraints` field already functions as an informal decision record threaded stage-to-stage (`role-contracts.md`). This skill formalizes exactly that pattern (context, alternatives, rationale) — a real candidate to structure `devsystem.plan`'s own output more rigorously, not a new concept being forced in. |
| `implement` | `android-clean-architecture` | **Yes, concrete** | Directly names Room/SQLDelight/Ktor and UseCase/Repository patterns — `CADS-webconference-android`'s `MessageStore` (plain `SQLiteOpenHelper`, a real, stated substitution *away* from Room to avoid a new toolchain surface, per iteration 5's own history) is exactly the kind of decision this skill's guidance would weigh in on. Worth consulting for the next real Android implement iteration, not retroactively re-litigating what's already shipped and tested. |
| `test` | `rust-testing`, `kotlin-testing` | **Yes, concrete, both sides** | This project is genuinely split: `rust-testing` for `pipeline`/`web` (this repo, its own hermetic gate), `kotlin-testing` for the Android app (Kotest/MockK/Kover, Robolectric already in use). Both name the exact real toolchains this project already runs, not generic advice. |
| `verify` | `verification-loop` | **No, honest negative** | Its own phases are `npm run build`/`pnpm build`/JS-toolchain-specific — this project has no JS build step anywhere in its actual gate. Not a fit as written; flagged rather than force-adopted. |
| `review` | `security-review` | **Yes, concrete** | "adding authentication, handling user input, working with secrets, creating API endpoints" describes `web/src/main.rs` almost exactly — a real, direct candidate to ground `devsystem.review`'s actual checklist, and it's the one row of §5's quality table (a.R.d.T. / defect-free delivery) this project has the least mechanical coverage of today. |
| `improve` | `agent-architecture-audit` | **Yes, strong** | "Diagnostic workflow for agent systems that hide failures behind wrapper layers, stale memory, retry loops" — this is a description of exactly the failure class this project's own `devsystem.improve`/self-optimization loop is vulnerable to, and exactly what the same-day deploy-race incident (a silent process gap, wrong result, no visible error) actually was. Real, well-justified fit. |
| `remember` | `unified-memory` (ECC Memory Vault) | **No, considered and declined** | `role-contracts.md` already states this directly: "No such canonical envelope exists upstream in mem0 or ECC; this is a new layer." The zylos envelope is purpose-built for this project's specific stage-to-stage contract (`task`/`key_findings`/`constraints`/`output_format`); ECC's vault is a *different*, generic cross-harness (Claude/Codex/Hermes/Cursor/OpenCode) context-sharing layer solving a different problem. Not a gap — a real, already-made decision, re-confirmed here rather than silently revisited. |

## Beyond the 7 stages: two real, unscoped findings

- **`loop-design-check`** — "review [a loop] for the ways loops go wrong: spinning and burning
  tokens, Goodhart-gaming the verifier, running a wrong answer to completion... decidability,
  boundaries, fallback, judge independence, keep-judgment-with-the-human." This isn't about any one
  stage — it's about **this whole project's own super-loop** (`AbortCriteria`, `should_abort`,
  `should_checkin`). The single most directly relevant skill in the entire catalog to "The
  Development System" as a concept, not adopted yet. Concrete next step: run this run's own loop
  design (max_iterations/max_consecutive_failures/checkin_every, the review gate, the stage-proposal
  queue) through this skill's own review checklist as a real, dedicated increment — not today's, but
  flagged here as the clear next candidate.
- **`plankton-code-quality`** — write-time auto-formatting/linting/Claude-powered fixes via hooks.
  A real, concrete mechanism for §5's "Kunstgerecht" (idiomatic code) row, which today has zero
  mechanical enforcement (noted honestly in the goal doc's own §5 table). Worth a real trial on one
  repo before deciding to adopt broadly — not evaluated deeply enough here to call it a firm yes.

## What this audit does NOT claim

This is a fit assessment, not an adoption. None of the "yes" rows above have been wired into the
actual pipeline yet — `devsystem.review`'s real checklist doesn't reference `security-review` today,
`devsystem.plan`'s output isn't yet ADR-structured. That's real follow-up work, sized and prioritized
by the operator, not implied as already done by this document's existence.
