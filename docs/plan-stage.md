# The `plan` stage: Plan Canvas integration

The `plan` stage's `RequiredRole` (`plan_only_spec()` in
[`../pipeline/src/lib.rs`](../pipeline/src/lib.rs)) is filled by whichever
agent/role-filler wins the auction for `devsystem.plan`. Its human-in-the-loop
gate is [ECC](https://github.com/affaan-m/ECC)'s `ecc-plan-canvas` — reused
directly, per the original proposal, not rebuilt.

## Where ECC actually is

Public GitHub repo, MIT-licensed, 237k★: **github.com/affaan-m/ECC**. Not
present in this loop's environment by default (only the generic
[addyosmani/agent-skills](https://github.com/addyosmani/agent-skills) set
ships here) — install via npm:

```bash
npm install -g ecc-universal
```

This provides the `ecc-plan-canvas` binary (among others: `ecc`,
`ecc-control-pane`, `ecc-install`, `ecc-memory-mcp`). Verified in this
environment: `ecc-plan-canvas open <file>` starts a real loopback HTTP
server (`127.0.0.1:4517`) and serves a real review page for a `.plan.md`
artifact; `end` shuts the session down cleanly. No further environment setup
needed — it is genuinely harness/model-agnostic (plain CLI + JSON).

## The gate, concretely

1. The `plan`-role filler writes its plan artifact — the zylos envelope's
   `key_findings` rendered as markdown — to `.claude/plans/<run_id>.plan.md`
   in the coordination repo's working copy for that run.
2. `ecc-plan-canvas open .claude/plans/<run_id>.plan.md` opens it for the
   human (Chef, per the proposal's terminology) to review in a browser.
3. `ecc-plan-canvas await .claude/plans/<run_id>.plan.md` blocks until a
   verdict arrives:
   - `approve` → the plan is CONFIRMED. The `RoleAssignment` for `plan`
     resolves successfully; the pipeline is clear to move to `test`.
   - `request-changes` → the filler revises the artifact (canvas
     live-reloads), replies via `--reply`, and keeps `await`ing.
4. `ecc-plan-canvas end <file>` once review concludes either way.

This maps directly onto the zylos envelope: the plan artifact **is** the
`key_findings` output; the human verdict is what actually gates advancing to
the next `RequiredRole` in the pipeline, not just the role-filler's own
self-assessment.

## Pre-flight risk annotation (proposal §5, not yet built)

The proposal also asks for a lightweight review agent that pre-populates
canvas annotations for known risk patterns ("touches auth," "no test stage
before implement," "external-partner role with no price ceiling") before a
human ever opens the canvas. Plan Canvas's `await --reply` and annotation
JSON shape support this (an agent could seed the session with `chat`/
`annotation` items before the human looks), but the seeding mechanism itself
is not implemented here yet — next slice.

## What's still open

- No bunsenbrenner-branded theme yet (proposal §5.1) — ECC ships its own
  dark theme with an accent palette; re-skinning it (if desired) is a CSS
  override, not a fork, since the canvas serves local artifact files as-is.
- A real run (`runs/webconference-android/`) now drives this CLI mechanism
  repeatedly for real (`devsystem_checkin`, every periodic check-in served
  and confirmed via the actual loopback page) — no longer isolated. What's
  still open: the specific `plan`-role-filler cycle described in steps 1-4
  above (a filler writes a plan, a human `approve`s or `request-changes`es
  it to gate advancing to `test`) hasn't run for this project yet — every
  check-in delivered so far has been the runner's periodic/mandatory
  check-in (`pipeline/src/checkin.rs`), a related but distinct use of the
  same canvas mechanism, not a `plan`-stage role-filler's own gate.
