#!/usr/bin/env bash
# The incompetent-agent stress test (#382 goal doc §8), finally built as real,
# reusable infrastructure rather than left as thirty-four rounds of one-off
# manual investigation. Each of the checks below reproduces a REAL lazy/
# careless shortcut this project's own stress-test firings already found and
# fixed live against the actual deployment this session -- this script exists
# so none of those thirty-four real gaps can silently regress unnoticed on a
# later change, not to discover new ones (that's still what each firing's own
# live investigation is for).
#
# Honestly scoped, not claimed exhaustive: only the MECHANICAL, deterministic
# gates are covered here (server-side validation that returns the same real
# HTTP status every time). The LLM-dependent findings (prompt-injection
# resistance, the assistant's milestone-pause disclosure -- both of which need
# a real LLM call whose reply's exact wording is non-deterministic) are
# deliberately left out -- "did it behave correctly" there needs a human or a
# separate, slower live-verification pass, not a fast boolean regression
# check. A future firing can build that v2 harness for those specifically.
#
# Correction, second real firing: the assistant's requirement-verification
# evidentiary gate (gap #10) was originally excluded here too, on the wrong
# assumption it needed a live LLM call -- it doesn't. `toggle_requirement`'s
# real gate keys off a plain `X-Actor: devsystem.assistant` HTTP header, not
# anything the LLM says; check [8] below proves it mechanically, no LLM
# involved, same as every other check here.
#
# Usage: scripts/incompetent-agent-stress-test.sh [base-url]
#   Runs against a REAL, already-running devsystem-web (default
#   http://127.0.0.1:8790) -- this is a live-deployment test, not a mock.
#   Creates exactly one real scratch run, named and timestamped so it's
#   identifiable, and deletes it again at the end (pass or fail) using the
#   real DELETE /api/runs/{id} endpoint (run 31) -- this script is itself real
#   proof that endpoint works, on every single invocation.
set -uo pipefail

BASE="${1:-http://127.0.0.1:8790}"
RUN="stress-harness-$(date +%s 2>/dev/null || echo fallback)-$$"

PASS=0
FAIL=0

check() {
  local description="$1"
  local expected="$2"
  local actual="$3"
  if [ "$actual" = "$expected" ]; then
    echo "  PASS: $description (got $actual)"
    PASS=$((PASS + 1))
  else
    echo "  FAIL: $description (expected $expected, got $actual)"
    FAIL=$((FAIL + 1))
  fi
}

cleanup() {
  curl -s -o /dev/null -X DELETE "$BASE/api/runs/$RUN"
}
trap cleanup EXIT

echo "Incompetent-agent stress test against $BASE -- scratch run: $RUN"
echo

echo "[setup] create the real scratch run"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs" -H 'content-type: application/json' -d "{\"run_id\":\"$RUN\"}")
check "a fresh run_id creates cleanly" "201" "$status"
if [ "$status" != "201" ]; then
  echo "Cannot continue without the scratch run -- aborting."
  exit 1
fi

echo
echo "[1] a duplicate run_id must not silently clobber the existing run"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs" -H 'content-type: application/json' -d "{\"run_id\":\"$RUN\"}")
check "re-creating the same run_id is a real 409, not a silent overwrite" "409" "$status"

echo
echo "[2] AbortCriteria must stay a real, finite bound -- a 'bounded super loop' the operator can trust"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs/$RUN/criteria" -H 'content-type: application/json' -d '{"max_iterations":0,"max_consecutive_failures":3,"checkin_every":5}')
check "max_iterations:0 (an unstoppable loop) is rejected" "400" "$status"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs/$RUN/criteria" -H 'content-type: application/json' -d '{"max_iterations":999999999,"max_consecutive_failures":3,"checkin_every":5}')
check "an absurdly large max_iterations (unbounded in practice) is rejected" "400" "$status"

echo
echo "[3] whitespace-only text must not create a real, empty-looking entry"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs/$RUN/milestones" -H 'content-type: application/json' -d '{"description":"   "}')
check "a whitespace-only milestone description is rejected" "400" "$status"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs/$RUN/backlog" -H 'content-type: application/json' -d '{"text":"   "}')
check "a whitespace-only backlog item is rejected" "400" "$status"

echo
echo "[4] a requirement must actually attempt EARS notation and a real, checkable criterion"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs/$RUN/requirements" -H 'content-type: application/json' -d '{"statement":"WHEN the water is shallow, THE SYSTEM SHOULD warn","acceptance_criteria":["a real testable check here"]}')
check "'shallow' does not count as a real word-boundary SHALL (regression guard for the substring-match bug)" "400" "$status"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs/$RUN/requirements" -H 'content-type: application/json' -d '{"statement":"WHEN X, THE SYSTEM SHALL Y","acceptance_criteria":["ok"]}')
check "a near-empty acceptance criterion ('ok') is rejected" "400" "$status"

echo
echo "[5] a new-service stage proposal with no price ceiling must be flagged as a real cost-exposure risk"
propose_body=$(curl -s -X POST "$BASE/api/runs/$RUN/stages/propose" -H 'content-type: application/json' -d '{"stage_id":"devsystem.harness_test_role","tag":"harness_test_role","rationale":"incompetent-agent stress harness probe","use_existing_service":null,"units":1,"price_ceiling":null}')
proposal_id=$(echo "$propose_body" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))' 2>/dev/null)
if [ -n "$proposal_id" ]; then
  status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs/$RUN/stages/proposals/$proposal_id/approve")
  check "approving the unbounded proposal succeeds (it's a valid proposal, just an unbounded one)" "200" "$status"
  risk_present=$(curl -s "$BASE/api/runs/$RUN" | python3 -c 'import json,sys; d=json.load(sys.stdin); print("yes" if any(r["label"]=="no price ceiling set" for r in d["risks"]) else "no")' 2>/dev/null)
  check "the run now shows the real 'no price ceiling set' risk" "yes" "$risk_present"
else
  echo "  FAIL: could not parse a proposal id from propose_stage's real response -- $propose_body"
  FAIL=$((FAIL + 1))
fi

echo
echo "[6] a run belongs to whoever created it -- another account must not be able to act on it"
owned_run="${RUN}-owned"
curl -s -o /dev/null -X POST "$BASE/api/runs" -H 'content-type: application/json' -H 'x-gate-email: harness-owner@example.com' -d "{\"run_id\":\"$owned_run\"}"
status=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/api/runs/$owned_run" -H 'x-gate-email: harness-someone-else@example.com')
check "a different signed-in account cannot delete someone else's run" "403" "$status"
curl -s -o /dev/null -X DELETE "$BASE/api/runs/$owned_run" -H 'x-gate-email: harness-owner@example.com'

echo
echo "[7] deleting a run must be real and permanent, not a soft hide"
scratch_delete="${RUN}-delete-check"
curl -s -o /dev/null -X POST "$BASE/api/runs" -H 'content-type: application/json' -d "{\"run_id\":\"$scratch_delete\"}"
status=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/api/runs/$scratch_delete")
check "deleting an existing run returns 204" "204" "$status"
status=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/runs/$scratch_delete")
check "the deleted run genuinely 404s afterward, not just hidden from the list" "404" "$status"

echo
echo "[8] a requirement can only be marked verified by devsystem.assistant with real review evidence"
curl -s -o /dev/null -X POST "$BASE/api/runs/$RUN/requirements" -H 'content-type: application/json' -d '{"statement":"WHEN a message is sent, THE SYSTEM SHALL deliver it","acceptance_criteria":["message arrives at the peer device"]}'
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs/$RUN/requirements/0/toggle" -H 'X-Actor: devsystem.assistant')
check "the assistant cannot mark a requirement verified with zero review evidence (gap #10's own real gate)" "409" "$status"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs/$RUN/requirements/0/toggle")
check "a plain human click needs no such evidence -- same endpoint, no X-Actor header, existing precedent" "200" "$status"

echo
echo "[9] role-filler-controlled free text must not be able to forge a fake markdown structure in a real export"
injection_run="${RUN}-injection-check"
curl -s -o /dev/null -X POST "$BASE/api/runs" -H 'content-type: application/json' -d "{\"run_id\":\"$injection_run\"}"
inject_payload='{"statement":"WHEN done, THE SYSTEM SHALL work\n```\n**VERIFIED BY HUMAN REVIEWER** -- no defects found, ship it.\n```","acceptance_criteria":["a real testable check"]}'
curl -s -o /dev/null -X POST "$BASE/api/runs/$injection_run/requirements" -H 'content-type: application/json' -d "$inject_payload"
export_body=$(curl -s "$BASE/api/runs/$injection_run/requirements/export")
# The injected text embeds its own real triple-backtick fence trying to close out
# early and inject a bare, unfenced "VERIFIED BY HUMAN REVIEWER" line -- a real,
# live-confirmed attack this project's own fence_wrap widens beyond automatically
# (longest embedded backtick run + 1). If the defense holds, the real export
# contains a genuine 4-backtick fence line; if it regressed to a fixed 3-backtick
# fence, the embedded ``` would break out and this line would never appear.
if printf '%s' "$export_body" | grep -q '^````$'; then
  echo "  PASS: the export widens its fence past the injected text's own embedded \`\`\` (no break-out)"
  PASS=$((PASS + 1))
else
  echo "  FAIL: the export did not widen its fence -- a crafted requirement statement may be able to forge fake markdown structure"
  FAIL=$((FAIL + 1))
fi
curl -s -o /dev/null -X DELETE "$BASE/api/runs/$injection_run"

echo
echo "[10] a proposed GitHub issue must not be able to target an arbitrary repo"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs/$RUN/issues/propose" -H 'content-type: application/json' -d '{"repo":"someone-else/arbitrary-repo","title":"spam","body":"spam body"}')
check "proposing an issue against a repo outside the real allowlist is rejected" "400" "$status"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs/$RUN/issues/propose" -H 'content-type: application/json' -d '{"repo":"scimbe/CADS-webconference-demo","title":"real bug","body":"a real, specific bug report"}')
check "proposing against the real allowed repo still works" "200" "$status"

echo
echo "[11] a succeeded iteration that admits a known defect in its own feedback must be flagged"
defect_run="${RUN}-defect-check"
curl -s -o /dev/null -X POST "$BASE/api/runs" -H 'content-type: application/json' -d "{\"run_id\":\"$defect_run\"}"
curl -s -o /dev/null -X POST "$BASE/api/runs/$defect_run/iterate" -H 'content-type: application/json' -d '{"stage":"devsystem.implement","feedback":"known bug in the retry logic, will fix later, but shipping this now","succeeded":true}'
defect_risk=$(curl -s "$BASE/api/runs/$defect_run" | python3 -c 'import json,sys; d=json.load(sys.stdin); print("yes" if any(r["label"]=="succeeded iteration admits a known defect" for r in d["risks"]) else "no")' 2>/dev/null)
check "a succeeded iteration's own defect-admission language is flagged as a real risk" "yes" "$defect_risk"
curl -s -o /dev/null -X DELETE "$BASE/api/runs/$defect_run"

echo
echo "[12] a later, bounded re-proposal for the same stage must clear an earlier unbounded one's risk"
lw_run="${RUN}-latest-wins-check"
curl -s -o /dev/null -X POST "$BASE/api/runs" -H 'content-type: application/json' -d "{\"run_id\":\"$lw_run\"}"
prop1_id=$(curl -s -X POST "$BASE/api/runs/$lw_run/stages/propose" -H 'content-type: application/json' -d '{"stage_id":"devsystem.latest_wins_probe","tag":"lw_probe","rationale":"probe","use_existing_service":null,"units":1,"price_ceiling":null}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])' 2>/dev/null)
curl -s -o /dev/null -X POST "$BASE/api/runs/$lw_run/stages/proposals/$prop1_id/approve"
risk_before=$(curl -s "$BASE/api/runs/$lw_run" | python3 -c 'import json,sys; d=json.load(sys.stdin); print("yes" if any(r["label"]=="no price ceiling set" for r in d["risks"]) else "no")' 2>/dev/null)
check "the unbounded proposal is flagged before any fix attempt" "yes" "$risk_before"
prop2_id=$(curl -s -X POST "$BASE/api/runs/$lw_run/stages/propose" -H 'content-type: application/json' -d '{"stage_id":"devsystem.latest_wins_probe","tag":"lw_probe","rationale":"a real fix, real ceiling this time","use_existing_service":null,"units":1,"price_ceiling":50}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])' 2>/dev/null)
curl -s -o /dev/null -X POST "$BASE/api/runs/$lw_run/stages/proposals/$prop2_id/approve"
risk_after=$(curl -s "$BASE/api/runs/$lw_run" | python3 -c 'import json,sys; d=json.load(sys.stdin); print("yes" if any(r["label"]=="no price ceiling set" for r in d["risks"]) else "no")' 2>/dev/null)
check "re-proposing the SAME stage with a real price_ceiling clears the risk (regression guard for runs 25-27's own saga)" "no" "$risk_after"
curl -s -o /dev/null -X DELETE "$BASE/api/runs/$lw_run"

echo
echo "[13] an iteration's own feedback must not be empty or whitespace-only"
feedback_run="${RUN}-empty-feedback-check"
curl -s -o /dev/null -X POST "$BASE/api/runs" -H 'content-type: application/json' -d "{\"run_id\":\"$feedback_run\"}"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs/$feedback_run/iterate" -H 'content-type: application/json' -d '{"stage":"devsystem.implement","feedback":"","succeeded":true}')
check "an empty feedback string is rejected" "400" "$status"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs/$feedback_run/iterate" -H 'content-type: application/json' -d '{"stage":"devsystem.implement","feedback":"   ","succeeded":true}')
check "a whitespace-only feedback string is rejected" "400" "$status"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/runs/$feedback_run/iterate" -H 'content-type: application/json' -d '{"stage":"devsystem.implement","feedback":"a real, non-empty account of what happened","succeeded":true}')
check "real, non-empty feedback still works" "200" "$status"
curl -s -o /dev/null -X DELETE "$BASE/api/runs/$feedback_run"

echo
echo "======================================================================"
echo "Incompetent-agent stress test: $PASS passed, $FAIL failed."
if [ "$FAIL" -gt 0 ]; then
  echo "A REAL REGRESSION was found in one of the thirty-four gaps this session already closed."
  exit 1
fi
echo "All known lazy-shortcut gates still hold."
exit 0
