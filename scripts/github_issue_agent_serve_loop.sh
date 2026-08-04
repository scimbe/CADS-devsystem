#!/bin/sh
# github_issue_agent_serve_loop.sh -- keeps github_issue_channel_handler
# reachable over a real, direct-address Agent-Fabric channel (#48 slice 5).
#
# Real, verified reason this loop exists at all: a direct-address
# `ct-agent channel --serve` accept-side process serves exactly ONE session
# and then exits (confirmed against the docs' own correction, and against
# this repo's own live testing -- see github_issue_channel_client.rs's
# module doc). Without something restarting it, the handler would answer
# exactly one real request ever. This is that "something" -- a minimal
# respawn loop, matching this ecosystem's existing convention of a plain
# shell script as the deployment artifact (handler-alice.sh, run-demo.sh in
# the sibling CADS-a2a-demo/CADS-auction-demo repos), not a systemd unit or
# supervisor process this repo has no other precedent for.
#
# Second real, verified reason CERT_PUBLISH_PATH exists: ct-agent generates
# a FRESH listen cert on every single process start, even reusing the exact
# same CT_CHANNEL_NOISE_KEY identity (verified directly: started the same
# identity twice, diffed the two printed CT_CHANNEL_PEER_CERT values -- they
# were completely different). A caller configured with one hardcoded cert
# would work for exactly one session, then fail every call after the first
# real restart. This loop atomically publishes the real, current cert to
# CERT_PUBLISH_PATH every time it restarts; github_issue_channel_client's own
# CT_CHANNEL_PEER_CERT_FILE reads it fresh immediately before every dial.
#
# Required env:
#   CT_AGENT_BIN                    path to the real ct-agent binary
#   CT_CHANNEL_ADDR                 e.g. 127.0.0.1:19710
#   CT_CHANNEL_NOISE_KEY             this identity's own private key
#   CT_CHANNEL_PEER_NOISE_KEY        the initiator's public key
#   GITHUB_ISSUE_AGENT_HANDLER_BIN   path to github_issue_channel_handler
#   CERT_PUBLISH_PATH                where the current cert is published
# Optional (passed straight through to the handler, same names it already
# reads -- see github_issue_channel_handler.rs's own doc comment):
#   GITHUB_ISSUE_AGENT_TOKEN, GITHUB_ISSUE_AGENT_MEMORY_PATH
#
# Usage: ./github_issue_agent_serve_loop.sh
# Stop with a real signal (Ctrl-C / SIGTERM) -- there is deliberately no
# built-in iteration cap; this is meant to run for as long as the operator
# wants the agent reachable, same as any other long-lived service in this
# ecosystem.

set -u

: "${CT_AGENT_BIN:?required}"
: "${CT_CHANNEL_ADDR:?required}"
: "${CT_CHANNEL_NOISE_KEY:?required}"
: "${CT_CHANNEL_PEER_NOISE_KEY:?required}"
: "${GITHUB_ISSUE_AGENT_HANDLER_BIN:?required}"
: "${CERT_PUBLISH_PATH:?required}"

echo "github_issue_agent_serve_loop: starting -- publishing certs to $CERT_PUBLISH_PATH" >&2

while true; do
  LOG=$(mktemp)

  CT_CHANNEL_ROLE=accept \
    CT_CHANNEL_ADDR="$CT_CHANNEL_ADDR" \
    CT_CHANNEL_NOISE_KEY="$CT_CHANNEL_NOISE_KEY" \
    CT_CHANNEL_PEER_NOISE_KEY="$CT_CHANNEL_PEER_NOISE_KEY" \
    CT_CHANNEL_SERVE=1 \
    CT_AGENT_SERVICE_HANDLER_CMD="$GITHUB_ISSUE_AGENT_HANDLER_BIN" \
    CT_AGENT_SERVICES=text_generation \
    "$CT_AGENT_BIN" channel >"$LOG" 2>&1 &
  PID=$!

  # Bounded poll for the real cert line ct-agent prints on startup -- 5s at
  # 100ms steps is generous relative to how fast it actually appears in
  # every real run so far, without looping forever if something's wrong.
  i=0
  CERT=""
  while [ "$i" -lt 50 ]; do
    CERT=$(grep -o 'CT_CHANNEL_PEER_CERT=[0-9a-f]*' "$LOG" 2>/dev/null | head -n 1 | cut -d= -f2)
    [ -n "$CERT" ] && break
    sleep 0.1
    i=$((i + 1))
  done

  if [ -n "$CERT" ]; then
    # write-then-rename: the same atomicity discipline established in
    # github_issue_channel_client.rs's own tests -- a caller reading
    # CERT_PUBLISH_PATH mid-write must never see a truncated value.
    printf '%s' "$CERT" >"$CERT_PUBLISH_PATH.tmp" && mv "$CERT_PUBLISH_PATH.tmp" "$CERT_PUBLISH_PATH"
    echo "github_issue_agent_serve_loop: published a fresh cert, serving one real session" >&2
  else
    echo "github_issue_agent_serve_loop: WARNING -- ct-agent printed no cert within 5s; last log:" >&2
    cat "$LOG" >&2
  fi

  wait "$PID"
  rm -f "$LOG"
  echo "github_issue_agent_serve_loop: session ended, restarting" >&2
done
