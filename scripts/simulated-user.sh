#!/usr/bin/env bash
# Real headless-browser "simulated user" testing, containerized (see
# simulated-user.Dockerfile's own doc comment for why: this host has no sudo, so
# installing Chrome/its OS deps directly on it can never succeed).
#
# Usage: scripts/simulated-user.sh <script.js> [output-dir]
#   <script.js>   a real Playwright script (CommonJS, `require('playwright')`) --
#                 write screenshots/output to /work/output/... inside the script.
#   [output-dir]  host directory screenshots land in; defaults to the script's own
#                 directory.
#
# First run: docker build -f scripts/simulated-user.Dockerfile -t ct-playwright-runner:latest scripts/
set -euo pipefail

SCRIPT="${1:?usage: simulated-user.sh <script.js> [output-dir]}"
OUTDIR="${2:-$(dirname "$SCRIPT")}"
mkdir -p "$OUTDIR"

docker run --rm --network host \
  -v "$(cd "$(dirname "$SCRIPT")" && pwd)":/work/scripts \
  -v "$(cd "$OUTDIR" && pwd)":/work/output \
  ct-playwright-runner:latest "/work/scripts/$(basename "$SCRIPT")"
