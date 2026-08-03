# CADS-devsystem

Coordination repo for **The Development System** — a demo of AI agents (and,
via the same mechanism, human reviewers and paid external partners) running a
real software-development pipeline on top of CADS-Tunnel's crew-auction
primitives. Full proposal: [CADS-Tunnel#382](https://github.com/scimbe/CADS-Tunnel/issues/382).

This repo holds pipeline definitions, role contracts, and project docs. The
target software actually being built by a pipeline run lives in its own,
separate repo (per the proposal's own separation of concerns) — nothing about
the software gets built *in* this repo.

## Status (2026-08-03)

Driven as an ongoing loop effort, one real, tested increment at a time — not
a single large push. Landed so far:

- ✅ **`RequiredRole`/`convene()` generalized** beyond the flappy-demo crew
  fixtures (`ServiceType::Custom`, CADS-Tunnel `v0.4.13`) — a pipeline
  designer can declare any service type without a CADS-Tunnel core release.
  Closes #180 as a side effect.
- ✅ **This coordination repo**, with a real `PipelineSpec` for all seven
  stages (`pipeline/`) and a hermetically-tested proof that `convene()`
  clears a real auction for a devsystem-declared role — see
  [`pipeline/src/lib.rs`](pipeline/src/lib.rs).
- 🚧 **Wiring the `plan` stage only** (`plan_only_spec` in `pipeline/`), per
  the proposal's own suggested sequencing — before committing to the full
  seven-stage build.
- ❌ Not started: `test`/`implement`/`review`/`verify`/`remember`/`improve`,
  the target Android repo, mem0+Qdrant memory backend, the landing-page
  Kanban.

### Open question: the Plan Canvas dependency

The proposal's `plan` stage says to reuse "ECC's Plan Canvas"
(`commands/plan-canvas.md`, `scripts/plan-canvas.js`) directly, plus ECC
skills like `android-clean-architecture` and `tdd-workflow` for later stages.
**None of these exist in the environment driving this loop** — only the
generic [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills)
set is installed (`~/.claude/skills/`), which has no Plan Canvas equivalent.
Either:

1. ECC lives somewhere else (a different agent's environment, a private
   toolkit) and needs to be pointed at/shared, or
2. It needs to be built from scratch here — a real scope addition to this
   plan, not a "reuse" as originally proposed.

Flagged on #382 rather than guessed at. Until resolved, the `plan` stage's
human-review gate will be a minimal, self-contained substitute (a blocking
review page, not a "ECC Plan Canvas"-branded one) if built before this
resolves.

## What's reused from CADS-Tunnel/ct-agent, unchanged

Nothing about agent-to-agent plumbing is reinvented here:

- **Agent Fabric channels** — every role-filler (an agent, a wrapped CLI
  tool, a human reviewer, a paid external partner) joins as a channel member
  via `ct-agent channel init/register/grant`.
- **AgentCard + `/registry/agents`** — self-service discovery; any agent
  self-registers with role tags matching a pipeline stage.
- **Crew auction** (`PipelineSpec`/`CapacityOffer`/`convene()`,
  `ct_common::pipeline`) — see [`pipeline/`](pipeline/) in this repo for the
  actual spec. Fails closed (`UnfilledRole`) exactly like every other
  CADS-Tunnel pipeline — no partial runs.
- **Escrow settlement** (`Hold`/`UsageReceipt`, `ct_common::settlement`) —
  real ed25519-signed escrow. This is where a paid external partner (e.g. a
  device-farm bidding on `AndroidInstrumentedTest` runs) plugs in; internal
  agents fill roles at price 0 through the identical mechanism.

## Pipeline stages

`plan → test → implement → review → verify → remember → improve`, each a
`RequiredRole` with a `ServiceType::Custom` service name (see
[`pipeline/src/lib.rs`](pipeline/src/lib.rs) for the exact names) and a
zylos-style input/output envelope (see [`docs/role-contracts.md`](docs/role-contracts.md)).

## Coordination model

One GitHub Issue per pipeline run — the live coordination surface, same
pattern used across CADS-Tunnel's own agent-driven work. A run's issue
tracks: which stage is active, who holds each role (from the real auction),
pending human-in-the-loop gates, and links to the target repo's actual
changes.
