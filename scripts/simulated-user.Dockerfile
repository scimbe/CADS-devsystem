# Real headless-browser "simulated user" testing, containerized -- this host has no
# sudo, so `npx playwright install chrome`/`install-deps` can never succeed directly
# on it (confirmed: fails on "sudo: a terminal is required to authenticate"). The
# official Playwright image already has Chromium + every OS dependency baked in by
# Microsoft, so running inside it sidesteps the missing-sudo problem entirely instead
# of working around it on the host.
#
# Pinned to the exact `playwright` npm version matching this image tag's bundled
# browser revision -- a mismatched npm/browser pair is a real, silent failure mode
# (works, but against a stale/incompatible browser build), not just a style nit.
FROM mcr.microsoft.com/playwright:v1.55.0-noble

WORKDIR /work
RUN npm init -y >/dev/null && npm install playwright@1.55.0

# Scripts and output are bind-mounted at run time (see simulated-user.sh) -- this
# image only carries the pinned runtime, never the actual test scripts, so the same
# image works for any real walkthrough script pointed at it.
ENTRYPOINT ["node"]
