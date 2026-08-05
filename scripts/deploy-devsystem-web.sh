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
# Usage: scripts/deploy-devsystem-web.sh [image-tag]
#   Builds devsystem-web:latest (or [image-tag] if given) from this repo's
#   own web/Dockerfile, then stops/removes/recreates the real container with
#   its full, real production config.
#
# Real env vars this expects to already exist on the host (never baked into
# the image or this script): the github-issue-channel-relay noise keys/cert
# under ~/.local/var/, the ct-agent binary at ~/alice-host/ct-agent. If any
# of these paths don't exist on a fresh host, create/provision them first --
# this script does not fabricate placeholders for real credentials.
set -euo pipefail

IMAGE_TAG="${1:-devsystem-web:latest}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "Building $IMAGE_TAG from $ROOT/web/Dockerfile ..."
docker build -f "$ROOT/web/Dockerfile" -t "$IMAGE_TAG" "$ROOT"

echo "Stopping/removing any existing devsystem-web container ..."
docker stop devsystem-web >/dev/null 2>&1 || true
docker rm devsystem-web >/dev/null 2>&1 || true

echo "Starting devsystem-web ..."
docker run -d --name devsystem-web \
  --network ct-selfhost_default \
  --add-host=host.docker.internal:host-gateway \
  --restart unless-stopped \
  -p 127.0.0.1:8790:8790 \
  -e CT_CHANNEL_NOISE_KEY="${CT_CHANNEL_NOISE_KEY:?set CT_CHANNEL_NOISE_KEY -- this container's own real channel identity}" \
  -e CT_CHANNEL_PEER_NOISE_KEY="${CT_CHANNEL_PEER_NOISE_KEY:?set CT_CHANNEL_PEER_NOISE_KEY -- github_issue_channel_handler's real public key}" \
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
