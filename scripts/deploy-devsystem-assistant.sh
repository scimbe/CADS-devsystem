#!/usr/bin/env bash
# Real, reproducible devsystem_assistant --serve deploy -- the other half of
# the same fragility class deploy-devsystem-web.sh closed for devsystem-web
# itself: this process has only ever been rebuilt/restarted by hand (a raw
# `crontab @reboot` line is the only tracked trace of it existing at all).
# Same real risk as before -- an ad-hoc rebuild silently regressing something
# (wrong listen address, stale binary, key file in the wrong CWD) with nothing
# to catch it.
#
# Usage: scripts/deploy-devsystem-assistant.sh [listen-addr] [api-base-url]
#   Defaults match the real crontab @reboot entry as of 2026-08-05:
#   listen-addr=172.17.0.1:8791 (the docker0 bridge gateway -- containers on
#   the default bridge reach it here; devsystem-web reaches it via
#   host.docker.internal, which resolves to this same address when the
#   container has --add-host=host.docker.internal:host-gateway, see
#   deploy-devsystem-web.sh), api-base-url=http://127.0.0.1:8790
#   (devsystem-web's own published port).
#
# Builds a real release binary hermetically (rust:1-slim, same Docker-volume
# build cache every other hermetic build in this repo already uses), extracts
# it from the named volume (release artifacts live in a volume, not a host
# bind-mount -- ct-devsystem-pipeline-target), backs up the currently deployed
# binary (timestamped, never silently overwritten without a rollback path),
# replaces it, and restarts the real running process from this repo's own
# root (its persisted signing key, devsystem-assistant-agent.key, is
# CWD-relative -- see DEVSYSTEM_ASSISTANT_KEY_FILE to override).
set -euo pipefail

LISTEN_ADDR="${1:-172.17.0.1:8791}"
API_BASE="${2:-http://127.0.0.1:8790}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="$HOME/.local/bin"
LOG_DIR="$HOME/.local/var/log"
VOLUME="ct-devsystem-pipeline-target"
CARGO_REGISTRY_VOLUME="ct-devsystem-cargo-registry"

echo "Building devsystem_assistant (release, hermetic) ..."
docker run --rm -v "$ROOT":/work -v "$CARGO_REGISTRY_VOLUME":/usr/local/cargo/registry -v "$VOLUME":/work/pipeline/target \
  -w /work/pipeline -e RUSTFLAGS='-D warnings' rust:1-slim \
  bash -c "cargo build --release --bin devsystem_assistant"

echo "Extracting the real binary out of the build volume ..."
docker run --rm -v "$VOLUME":/target -v "$ROOT/pipeline":/host alpine:latest \
  sh -c "cp /target/release/devsystem_assistant /host/devsystem_assistant.new && chown $(id -u):$(id -g) /host/devsystem_assistant.new"
chmod +x "$ROOT/pipeline/devsystem_assistant.new"

mkdir -p "$BIN_DIR" "$LOG_DIR"
if [ -f "$BIN_DIR/devsystem_assistant" ]; then
  BACKUP="$BIN_DIR/devsystem_assistant.bak-$(date +%s)"
  cp "$BIN_DIR/devsystem_assistant" "$BACKUP"
  echo "Backed up the previously deployed binary to $BACKUP"
fi
mv "$ROOT/pipeline/devsystem_assistant.new" "$BIN_DIR/devsystem_assistant"
chmod +x "$BIN_DIR/devsystem_assistant"

echo "Stopping any currently running devsystem_assistant --serve process ..."
OLD_PID="$(pgrep -f "^$BIN_DIR/devsystem_assistant --serve" || true)"
if [ -n "$OLD_PID" ]; then
  kill "$OLD_PID"
  sleep 1
fi

CURRENT_GIT_SHA="$(cd "$ROOT" && git rev-parse HEAD 2>/dev/null || echo unknown)"

echo "Starting devsystem_assistant --serve $LISTEN_ADDR $API_BASE (CWD=$ROOT) ..."
cd "$ROOT"
DEVSYSTEM_GIT_SHA="$CURRENT_GIT_SHA" nohup setsid "$BIN_DIR/devsystem_assistant" --serve "$LISTEN_ADDR" "$API_BASE" \
  > "$LOG_DIR/devsystem_assistant.log" 2>&1 < /dev/null &
disown
sleep 2

NEW_PID="$(pgrep -f "^$BIN_DIR/devsystem_assistant --serve" || true)"
if [ -z "$NEW_PID" ]; then
  echo "devsystem_assistant did not stay running -- check $LOG_DIR/devsystem_assistant.log" >&2
  exit 1
fi
echo "devsystem_assistant is running (pid $NEW_PID)"

# A real request against the live process -- a deliberately malformed one
# (empty run_id) so this costs no real LLM spend, but still proves the HTTP
# server itself is really answering, not just that the process forked.
STATUS="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 -X POST "http://$LISTEN_ADDR/ask" -H 'content-type: application/json' -d '{}' || echo "000")"
if [ "$STATUS" != "400" ]; then
  echo "WARNING: expected HTTP 400 for a malformed /ask request, got $STATUS -- the process is running but may not be answering correctly." >&2
else
  echo "Real HTTP round trip confirmed (400 for a malformed request, as expected)."
fi

# Real gap found live 2026-08-07 (#382 goal doc §8): the check above proves
# the process forked and answers SOME request, not that it's actually
# running THIS repo's current source -- the exact same class of gap
# deploy-devsystem-web.sh's own git-SHA verification closed for the other
# real deploy path. This binary isn't baked into a Docker image (no ARG/ENV
# to bake in), so the real current SHA is passed straight through as a
# process env var above instead, and checked here against what the actually
# running process reports.
DEPLOYED_GIT_SHA="$(curl -sS --max-time 5 "http://$LISTEN_ADDR/version" 2>/dev/null | python3 -c 'import sys,json; print(json.load(sys.stdin).get("git_sha","unknown"))' 2>/dev/null || echo unknown)"
if [ "$DEPLOYED_GIT_SHA" != "$CURRENT_GIT_SHA" ]; then
  echo "GIT SHA MISMATCH: the running process reports build SHA '$DEPLOYED_GIT_SHA', but the real current source is '$CURRENT_GIT_SHA'." >&2
  exit 1
fi
echo "Git SHA verified: running process matches real current source ($CURRENT_GIT_SHA)."

echo ""
echo "Reminder: this does NOT update the crontab @reboot entry -- if listen-addr/api-base changed, update it too:"
echo "  crontab -e"
