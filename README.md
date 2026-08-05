# CADS-devsystem

Coordination repo for **The Development System** — a demo of AI agents (and,
via the same mechanism, human reviewers and paid external partners) running a
real software-development pipeline on top of CADS-Tunnel's crew-auction
primitives. Full proposal: [CADS-Tunnel#382](https://github.com/scimbe/CADS-Tunnel/issues/382).

This repo holds pipeline definitions, role contracts, and project docs. The
target software actually being built by a pipeline run lives in its own,
separate repo (per the proposal's own separation of concerns) — nothing about
the software gets built *in* this repo.

**Live control panel: [devsystem-demo.bunsenbrenner.org](https://devsystem-demo.bunsenbrenner.org/)**
— a real, interactive GUI (`web/`, see Status below), not a static status
page, served over a real CADS-Tunnel Browser-Plane tunnel (own Keycloak
account, own `admin_provision_tunnel`-tracked subdomain, own agent/origin
pair; source in [`demo-site/`](demo-site/)). Currently Gelb-tier (shared
wildcard cert) — will move to `tls` + a real cert once promoted to Grün.

## Status (2026-08-05)

Driven as an ongoing loop effort, one real, tested increment at a time — not
a single large push, and explicitly **not** a static, pre-declared pipeline:
every stage below except `plan` entered the live spec through a real
`StageProposal` a role-filler iteration raised for itself, not upfront
design. Landed so far:

- ✅ **`RequiredRole`/`convene()` generalized** beyond the flappy-demo crew
  fixtures (`ServiceType::Custom`, CADS-Tunnel `v0.4.13`) — a pipeline
  designer can declare any service type without a CADS-Tunnel core release.
  Closes #180 as a side effect.
- ✅ **This coordination repo**, with a real `PipelineSpec` for all seven
  stages (`pipeline/`) and a hermetically-tested proof that `convene()`
  clears a real auction for a devsystem-declared role — see
  [`pipeline/src/lib.rs`](pipeline/src/lib.rs), continuously verified
  on every push via [`.github/workflows/pipeline-ci.yml`](.github/workflows/pipeline-ci.yml).
- ✅ **The `plan` stage's human-review gate**: ECC ([affaan-m/ECC](https://github.com/affaan-m/ECC),
  MIT, `npm i -g ecc-universal`) is a real, public, harness-agnostic package —
  its `ecc-plan-canvas` CLI is installed and used for real, not just verified
  standalone. See [`docs/plan-stage.md`](docs/plan-stage.md).
- ✅ **The self-optimization mechanism** (`pipeline/src/runner.rs`):
  `StageProposal`/`apply_proposal` mutate a *live* `PipelineSpec` mid-run;
  `AbortCriteria`/`should_checkin`/`should_abort` bound the "super loop" —
  proven not just by unit tests but by a real run's mandatory check-in
  genuinely firing (`RunOutcome::CheckinDue`), not a manual pause.
- ✅ **`devsystem.remember`'s first piece** (`pipeline/src/envelope.rs`): the
  zylos envelope (documented since the first commit, unimplemented until
  now) is real code — every iteration appends its `EnvelopeRecord` to
  `runs/<run_id>/memory.jsonl`, JSONL shaped for a future mem0/Qdrant load
  without reshaping.
- ✅ **`devsystem.improve`'s first piece** (`pipeline/src/improve.rs`):
  `stalled_stages()` mechanically finds proposals live in the spec with no
  iteration run as that stage yet — surfaced automatically in every
  check-in artifact.
- ✅ **A real, interactive control GUI** (`web/`) — not the static status page
  this section used to describe. A real axum backend
  ([`web/src/main.rs`](web/src/main.rs)) + a vanilla-JS/no-build-step
  frontend ([`web/static/index.html`](web/static/index.html)), deployed live
  at [devsystem-demo.bunsenbrenner.org](https://devsystem-demo.bunsenbrenner.org/):
  lists/creates runs, submits real iterations and check-ins, drives the
  auction (quick-submit/direct-accept/fill-mode), manages milestones,
  backlog, structured requirements (EARS-style statements + per-criterion
  acceptance tracking + real cross-iteration traceability), custom panels,
  RAG-indexed docs, and self-service GitHub issue proposals — every one of
  these already went through a real "propose, human approves" gate where the
  action genuinely warrants it.
- ✅ **`devsystem.assistant`** — a real, swappable LLM role (any
  non-interactive CLI via `CT_LLM_CMD`) with narrow, real write access to
  milestones/backlog/requirements/`repo_url`/new-run-creation, and a
  "propose, human approves" path for custom panels, new pipeline stages, and
  self-healing GitHub issues. Deliberately does **not** have a generic
  "submit iteration" action — an iteration is a role-filler's real, verified
  output; letting advisory chat fabricate one would corrupt a run's own
  honest record.
- ✅ **A real GitHub-issue channel agent** (`pipeline/src/bin/github_issue_channel_*`,
  `scripts/github_issue_agent_serve_loop.sh`) — moves the GitHub-posting
  credential off this (resource-constrained) host and behind a real
  Agent-Fabric channel, with its own on-disk dedup memory. Fully built and
  live-verified end to end (including a real respawn-loop wrapper solving a
  real discovered bug: `ct-agent` mints a fresh listen cert on every
  restart); not yet cut into production traffic pending a real hosting
  decision.
- ✅ **Flagship proof, real and building**: [`CADS-webconference-android`](https://github.com/scimbe/CADS-webconference-android)
  has a real Kotlin/Gradle scaffold, a hermetically-verified signed debug
  APK, a real UniFFI + `cargo-ndk` native bridge with real Noise_IK
  public-key generation wired into `MainActivity`, and a real GitHub Actions
  CI workflow with several consecutive green runs.
- 🔓 **The auction mechanism genuinely works end to end, self-tested**: [CADS-Tunnel#382](https://github.com/scimbe/CADS-Tunnel/issues/382)'s
  onboarding thread ([CADS-Tunnel#388](https://github.com/scimbe/CADS-Tunnel/issues/388))
  has real Keycloak accounts, real ed25519-signed `CapacityOffer`s, and real
  HTTP calls against the live deployment bidding on this run's roles —
  correction, 2026-08-05: those identities (`labor-setup.com`, `bob-1`, and
  8 others) are operator-created test personas exercising the mechanism, not
  independent third parties. Earlier wording here overstated this as
  external market validation; it's real end-to-end verification of the
  auction primitives, not real outside demand.
- ⏸ **`runs/webconference-android/`'s spec is currently plan-only on
  purpose**: reset by the operator for a fresh re-test of the whole setup
  flow. A real, already-verified stage proposal to re-add
  `devsystem.android_native_bridge` (the real, CI-green native-bridge work
  above) is queued and awaiting the operator's own approval, not
  auto-applied.
- ❌ Not started: mem0/Qdrant actually loading `memory.jsonl` (the log format
  is ready; nothing consumes it yet), the resource/SaaS/agent catalog, the
  local git server, and automatic (LLM-judged) acceptance-criteria
  verification — all real, explicitly deferred design questions, not
  guessed at.

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
