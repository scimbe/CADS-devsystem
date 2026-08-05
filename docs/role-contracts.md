# Role contracts: the zylos envelope

Every pipeline stage's role-filler receives and produces a **zylos envelope**
— the per-task result record this system layers on top of a plain
key/value memory store (mem0 + Qdrant, not yet wired — see the README's
status section). No such canonical envelope exists upstream in mem0 or ECC;
this is a new layer, borrowing ECC's own unreviewed-vault vs.
governed-canonical-artifact split for the `trust` field.

## Schema

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "zylos envelope v1",
  "type": "object",
  "required": ["task", "key_findings", "constraints", "output_format"],
  "properties": {
    "task": {
      "type": "string",
      "description": "What this stage was asked to do, in the role-filler's own words -- not copied verbatim from the pipeline spec, so a reviewer can tell the filler actually understood the ask."
    },
    "key_findings": {
      "type": "array",
      "items": { "type": "string" },
      "description": "The stage's actual output content -- a plan's decisions, a review's findings, a test run's failures, etc. Shape varies per stage; this field is always present, never empty on success."
    },
    "constraints": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Anything the NEXT stage must respect because of this one -- e.g. plan's constraints flow into implement's input."
    },
    "output_format": {
      "type": "string",
      "description": "How key_findings is shaped (e.g. \"markdown\", \"diff\", \"json:ct_common::pipeline::RoleAssignment[]\") -- lets a downstream stage parse without guessing."
    }
  }
}
```

## Metadata (mem0 filterable fields, not part of the envelope payload itself)

| Field | Meaning |
|---|---|
| `run_id` | The GitHub Issue number/slug this run is tracked under. |
| `stage` | One of `plan/test/implement/review/verify/remember/improve`. |
| `role` | The `RequiredRole.tag` that produced this record. |
| `trust` | `unreviewed` (default, on write) or `governed` (promoted after a human review — borrowed from ECC's vault/governed-artifact split). |

`mem0.search(filters={"run_id": ..., "role": "implement"})` gives exactly
"only your own scope" — no code-review history leaking into the research
agent's context, no literature list leaking into the coding agent's.

## Stage → `ServiceType::Custom` name (see [`pipeline/src/lib.rs`](../pipeline/src/lib.rs))

| Stage | Service name | Role tag |
|---|---|---|
| plan | `devsystem.plan` | `plan` |
| test | `devsystem.test` | `test` |
| implement | `devsystem.implement` | `implement` |
| review | `devsystem.review` | `review` |
| verify | `devsystem.verify` | `verify` |
| remember | `devsystem.remember` | `remember` |
| improve | `devsystem.improve` | `improve` |

`plan`'s gate (`ecc-plan-canvas`) is verified and real, reused across every
run's check-ins. `remember` has its envelope-writing mechanism live (every
iteration appends to `memory.jsonl`, including real per-iteration
requirement-traceability lines when an iteration claims to address one —
see `pipeline/src/envelope.rs`) but has not yet run as its *own* stage the
way the others have. Which of `implement`/`test`/`verify`/`review`/`improve`
have real recorded iterations is genuinely per-run, not fixed — see a
given run's own state via the GUI or `GET /api/runs/{id}` rather than
assuming from this doc (`runs/webconference-android/`, this project's own
flagship run, was reset by the operator on 2026-08-04 for a fresh
setup-flow re-test; `android_native_bridge` is currently a real, queued
stage *proposal* there, not live in the spec — see the README's Status
section for its current, real state, not this doc, which would otherwise
drift stale exactly like this paragraph itself just did).
