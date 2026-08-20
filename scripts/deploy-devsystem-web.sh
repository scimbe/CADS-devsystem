#!/usr/bin/env bash
# Real, reproducible devsystem-web deploy -- this container has never had a
# tracked deploy script (only ad-hoc `docker run` invocations), which is
# exactly how TWO real regressions shipped silently, both found and fixed
# 2026-08-05 while deploying an unrelated devsystem_assistant change, not by
# design:
#   1. A redeploy at some point dropped the `-p 127.0.0.1:8790:8790` port
#      publish, breaking devsystem_assistant's own outbound calls back to
#      this API (its --serve process's api_base is the fixed
#      http://127.0.0.1:8790, no service discovery).
#   2. This script's own FIRST version (committed, then immediately proven
#      wrong by a real live Playwright walkthrough) still missed
#      `--add-host=host.docker.internal:host-gateway` -- without it,
#      `host.docker.internal` never resolves inside the container at all, so
#      DEVSYSTEM_ASSISTANT_URL (the OTHER direction: this container calling
#      OUT to the assistant bridge) silently can't connect either. Both
#      directions of the same integration were broken by two different
#      missing flags -- verified live via the actual GUI, not assumed fixed
#      from a curl check on only one direction.
#
# Usage: scripts/deploy-devsystem-web.sh [--no-cache] [image-tag]
#   Builds devsystem-web:latest (or [image-tag] if given) from this repo's
#   own web/Dockerfile, then stops/removes/recreates the real container with
#   its full, real production config. Pass --no-cache to force a clean
#   rebuild -- see the real incident below (2026-08-07) for exactly when
#   that's needed: the BuildKit cache mount can go silently stale even for a
#   real deploy through this script, not just an ad-hoc scratch build.
#
# Real env vars this needs: the github-issue-channel-relay noise keys under
# ~/.local/var/keys/, the ct-agent binary at ~/alice-host/ct-agent. If any of
# these paths don't exist on a fresh host, create/provision them first --
# this script does not fabricate placeholders for real credentials.
#
# Real incident, 2026-08-05: this script used to *require*
# CT_CHANNEL_NOISE_KEY/CT_CHANNEL_PEER_NOISE_KEY already exported in the
# caller's shell, with no fallback -- and it stopped/removed the OLD
# container unconditionally before ever checking whether the new one could
# actually start. Run from a shell that hadn't exported them, it took
# production down for real (the old container gone, the new one refusing to
# start on the unset `:?` check) until caught and fixed live. Now: read the
# real key material straight from its known on-disk location by default (the
# env vars still override, for a host where the keys live elsewhere), and
# check it exists *before* touching the running container at all.
#
# Real incident, 2026-08-05 (later the same day): with three separate
# autonomous loops now capable of firing this script around the same time,
# two concurrent `docker build`s raced on web/Dockerfile's own BuildKit cache
# mount (`--mount=type=cache,target=/work/target`, cargo's incremental build
# cache). Both builds exited 0 -- but the cache mount, shared and mutated by
# both concurrent cargo invocations, left the resulting binary genuinely
# stale (missing real, already-committed source changes) with no error at
# all. Confirmed directly: `docker exec devsystem-web strings ... | grep` for
# a distinctive string from the latest commit came back empty. A `flock` on
# this script's own path serializes real invocations -- a second concurrent
# run waits for the first to finish rather than racing its cache mount.
#
# Real incident, 2026-08-07: the flock above only serializes concurrent
# invocations of THIS script -- it does nothing to protect the shared cache
# mount from an unrelated ad-hoc `docker build -f web/Dockerfile` running at
# the same time (see web/Dockerfile's own doc comment). This happened for
# real: a genuine, already-committed, already-tested fix
# (`duplicate_of_last_iteration`'s idempotency guard, CADS-devsystem@3afdbd2)
# silently never reached a real deploy through this exact script -- the
# incompetent-agent stress harness caught it live, a `409` that should have
# fired came back `200` instead, on a container whose binary mtime looked
# perfectly recent. No error anywhere; the only way to know was to actually
# exercise the real behavior post-deploy, which nothing here did before this
# fix. Two real, complementary responses, not just documentation this time:
# `--no-cache` support below (for recovering from an already-poisoned cache,
# used to fix this exact incident), and a real behavioral smoke test after
# the container comes up (below) that would have caught this the moment it
# happened, rather than an unrelated future firing noticing by accident.
#
# Real gap found live 2026-08-07: the smoke test above proves ONE specific
# behavior matches source -- it genuinely caught one real incident, but a
# regular (non-scratch) run of THIS script served a binary missing a
# completely different, unrelated feature
# (`checkin_cadence_effectively_disabled`) while that one smoke-tested
# behavior still worked fine, only caught by the full stress harness
# afterward, not this script. Chasing individual behavioral proxies one at a
# time doesn't scale. General fix: the image now bakes in its own build-time
# `git rev-parse HEAD` (`GIT_SHA` build-arg -> `DEVSYSTEM_GIT_SHA` ->
# `GET /api/version`, see `version`'s own doc comment in web/src/main.rs) and
# this script compares it against the real, current source right after the
# container comes up -- catches ANY staleness, not just whichever behavior a
# smoke test happens to exercise.
set -euo pipefail

LOCK_FILE="/tmp/deploy-devsystem-web.lock"
exec 9>"$LOCK_FILE"
if ! flock -n 9; then
  echo "Another deploy-devsystem-web.sh is already running -- waiting for it to finish ..."
  flock 9
fi

NO_CACHE_FLAG=()
if [ "${1:-}" = "--no-cache" ]; then
  NO_CACHE_FLAG=(--no-cache)
  shift
fi
IMAGE_TAG="${1:-devsystem-web:latest}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KEYS_DIR="/home/becke/.local/var/keys"

if [ -z "${CT_CHANNEL_NOISE_KEY:-}" ]; then
  CT_CHANNEL_NOISE_KEY="$(cat "$KEYS_DIR/devsystem-web-issue-channel-noise.key" 2>/dev/null || true)"
fi
if [ -z "${CT_CHANNEL_PEER_NOISE_KEY:-}" ]; then
  CT_CHANNEL_PEER_NOISE_KEY="$(cat "$KEYS_DIR/github-issue-agent-handler-noise.pub" 2>/dev/null || true)"
fi
if [ -z "$CT_CHANNEL_NOISE_KEY" ] || [ -z "$CT_CHANNEL_PEER_NOISE_KEY" ]; then
  echo "Missing real channel key material -- checked \$CT_CHANNEL_NOISE_KEY/\$CT_CHANNEL_PEER_NOISE_KEY env vars and $KEYS_DIR/{devsystem-web-issue-channel-noise.key,github-issue-agent-handler-noise.pub}. Not touching the running container." >&2
  exit 1
fi

CURRENT_GIT_SHA="$(cd "$ROOT" && git rev-parse HEAD 2>/dev/null || echo unknown)"

# Real incident, 2026-08-07: this host's own root filesystem was already at
# 100% (33MB free on 72G) before a real hermetic `cargo test` run even
# started, and a separate build through this exact script later ran 3-5x its
# normal duration while the same disk kept draining -- the same disk this
# host's other live services (this project's own control-plane, several demo
# tunnels) depend on. Both times the problem was only noticed mid-build,
# after minutes were already spent and the disk was already worse. A real,
# cheap precondition instead: refuse to even start the build below 2GB free,
# with a clear, actionable message -- failing in under a second beats
# failing (or silently degrading everything else on the host) minutes in.
MIN_FREE_KB=$((2 * 1024 * 1024))
FREE_KB="$(df -Pk "$ROOT" | awk 'NR==2 {print $4}')"
if [ -n "$FREE_KB" ] && [ "$FREE_KB" -lt "$MIN_FREE_KB" ]; then
  FREE_HUMAN="$(df -Ph "$ROOT" | awk 'NR==2 {print $4}')"
  echo "Only $FREE_HUMAN free on this host -- refusing to start a real docker build below a 2GB floor (a real incident already tied a slow/failed build here to a fully-drained disk affecting other live services on this host). Free real space first (e.g. \`docker image prune -f\`, \`docker builder prune -f\`) and re-run." >&2
  exit 1
fi

echo "Building $IMAGE_TAG from $ROOT/web/Dockerfile ...${NO_CACHE_FLAG:+ (--no-cache)}"
docker build "${NO_CACHE_FLAG[@]}" --build-arg "GIT_SHA=$CURRENT_GIT_SHA" -f "$ROOT/web/Dockerfile" -t "$IMAGE_TAG" "$ROOT"

echo "Stopping/removing any existing devsystem-web container ..."
docker stop devsystem-web >/dev/null 2>&1 || true
docker rm devsystem-web >/dev/null 2>&1 || true

echo "Starting devsystem-web ..."
# #74: this container shares the production ct-selfhost_default network on a
# swap-less host -- an unbounded container here is the same risk class
# CADS-Tunnel#600 closed for examples/help-site. Real usage is ~5MB; 128m/32m/
# 0.5cpu is generous headroom, matching #600's precedent exactly.
docker run -d --name devsystem-web \
  --network ct-selfhost_default \
  --add-host=host.docker.internal:host-gateway \
  --restart unless-stopped \
  --memory=128m --memory-swap=128m --memory-reservation=32m --cpus=0.5 \
  -p 127.0.0.1:8790:8790 \
  -e CT_CHANNEL_NOISE_KEY="$CT_CHANNEL_NOISE_KEY" \
  -e CT_CHANNEL_PEER_NOISE_KEY="$CT_CHANNEL_PEER_NOISE_KEY" \
  -e CT_CHANNEL_PEER_CERT_FILE=/app/github-issue-agent-cert/current-cert.txt \
  -e DEVSYSTEM_STATIC_DIR=/app/web/static \
  -e DEVSYSTEM_RUNS_DIR=/app/runs \
  -e DEVSYSTEM_ASSISTANT_URL=http://host.docker.internal:8791 \
  -e CT_AGENT_BIN=/app/ct-agent \
  -e CT_CHANNEL_ADDR=172.17.0.1:19710 \
  -e ISSUE_CHANNEL_CLIENT_BIN=/app/github_issue_channel_client \
  -v /home/becke/.local/var/github-issue-agent-cert:/app/github-issue-agent-cert:ro \
  -v "$ROOT/runs":/app/runs \
  -v /home/becke/.local/var/keys/devsystem-web-issue-channel-noise.key:/tmp/dw-noise.key:ro \
  -v /home/becke/alice-host/ct-agent:/app/ct-agent:ro \
  "$IMAGE_TAG"

echo "Waiting for a real 200 from the container's own published port ..."
UP=0
for _ in $(seq 1 30); do
  if curl -sS --max-time 2 -o /dev/null -w '' http://127.0.0.1:8790/api/runs 2>/dev/null; then
    UP=1
    break
  fi
  sleep 1
done
if [ "$UP" -ne 1 ]; then
  echo "devsystem-web did not answer http://127.0.0.1:8790/api/runs within 30s -- check: docker logs devsystem-web" >&2
  exit 1
fi
echo "devsystem-web is up: http://127.0.0.1:8790"

# Real gap found live 2026-08-07: the behavioral smoke test below proves ONE
# specific behavior matches source -- it already caught one real staleness
# incident, but a DIFFERENT, unrelated stale binary (missing
# checkin_cadence_effectively_disabled while duplicate_of_last_iteration
# still worked fine) would have passed it clean. Comparing the running
# binary's own baked-in build SHA against the real, current source directly
# catches ANY staleness, not just whichever one behavior happens to be
# checked -- general, not another specific-behavior proxy to maintain.
DEPLOYED_GIT_SHA="$(curl -sS --max-time 5 http://127.0.0.1:8790/api/version 2>/dev/null | python3 -c 'import sys,json; print(json.load(sys.stdin).get("git_sha","unknown"))' 2>/dev/null || echo unknown)"
if [ "$DEPLOYED_GIT_SHA" != "$CURRENT_GIT_SHA" ]; then
  echo "GIT SHA MISMATCH: the running container reports build SHA '$DEPLOYED_GIT_SHA', but the real current source is '$CURRENT_GIT_SHA'." >&2
  echo "This binary does not match this repo's actual source -- try: scripts/deploy-devsystem-web.sh --no-cache" >&2
  exit 1
fi
echo "Git SHA verified: running container matches real current source ($CURRENT_GIT_SHA)."

# The OTHER direction (this container -> devsystem_assistant --serve, if one is
# running on the host) -- a real, unauthenticated status probe, not just "the
# port answers." This is exactly the direction that broke silently on
# 2026-08-05 (missing --add-host), so it's checked here, not assumed working
# just because /api/runs did.
STATUS_BODY="$(curl -sS --max-time 5 http://127.0.0.1:8790/api/assistant/status 2>/dev/null || true)"
CONFIGURED="$(echo "$STATUS_BODY" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("configured"))' 2>/dev/null || echo "unknown")"
REACHABLE="$(echo "$STATUS_BODY" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("reachable"))' 2>/dev/null || echo "unknown")"
if [ "$CONFIGURED" = "True" ] && [ "$REACHABLE" != "True" ]; then
  echo "WARNING: devsystem.assistant is configured but NOT reachable from this container -- check devsystem_assistant --serve is running and --add-host=host.docker.internal:host-gateway took effect (getent hosts host.docker.internal inside the container)." >&2
elif [ "$CONFIGURED" = "True" ]; then
  echo "devsystem.assistant bridge: reachable"
else
  echo "devsystem.assistant bridge: not configured on this deployment (DEVSYSTEM_ASSISTANT_URL unset) -- expected if none is meant to run here"
fi

# Real behavioral smoke test (2026-08-07 incident, see the doc comment above
# the flock): "the port answers" and "the assistant bridge is reachable"
# both check connectivity, not that the actual compiled behavior matches this
# repo's own source -- exactly the gap that let a genuine, silently-stale
# binary pass both checks above while a real, already-committed idempotency
# fix was completely absent from it. This exercises one real, cheap,
# self-contained gate end to end (the byte-identical-resubmission guard,
# `duplicate_of_last_iteration`) against the container that's actually now
# running, using a real scratch run it creates and deletes itself -- if this
# ever regresses again, silently or otherwise, THIS deploy fails loudly
# instead of a later, unrelated firing discovering it by accident.
SMOKE_RUN="deploy-smoke-$(date +%s 2>/dev/null || echo fallback)-$$"
curl -sS -o /dev/null -X POST http://127.0.0.1:8790/api/runs -H 'content-type: application/json' -d "{\"run_id\":\"$SMOKE_RUN\"}"
FIRST_STATUS="$(curl -sS -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:8790/api/runs/$SMOKE_RUN/iterate" -H 'content-type: application/json' \
  -d '{"stage":"devsystem.plan","feedback":"deploy smoke test iteration","succeeded":true}')"
DUP_STATUS="$(curl -sS -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:8790/api/runs/$SMOKE_RUN/iterate" -H 'content-type: application/json' \
  -d '{"stage":"devsystem.plan","feedback":"deploy smoke test iteration","succeeded":true}')"
curl -sS -o /dev/null -X DELETE "http://127.0.0.1:8790/api/runs/$SMOKE_RUN"
if [ "$FIRST_STATUS" != "200" ] || [ "$DUP_STATUS" != "409" ]; then
  echo "SMOKE TEST FAILED: the byte-identical-resubmission guard did not behave as this repo's own source says it should (first iteration: got $FIRST_STATUS, expected 200; duplicate resubmission: got $DUP_STATUS, expected 409)." >&2
  echo "This deployed binary likely does not match this repo's actual source -- try: scripts/deploy-devsystem-web.sh --no-cache" >&2
  exit 1
fi
echo "Smoke test passed: the byte-identical-resubmission guard behaves correctly on the running container."
