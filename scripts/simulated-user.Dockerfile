# Real headless-browser "simulated user" testing, containerized -- this host has no
# sudo, so `npx playwright install chrome`/`install-deps` can never succeed directly
# on it (confirmed: fails on "sudo: a terminal is required to authenticate"). The
# official Playwright image already has Chromium + every OS dependency baked in by
# Microsoft, so running inside it sidesteps the missing-sudo problem entirely instead
# of working around it on the host.
#
# Was pinned to an exact `playwright` npm version matching this image tag's
# bundled browser revision -- a mismatched npm/browser pair is a real, silent
# failure mode (works, but against a stale/incompatible browser build), not
# just a style nit.
#
# Real drift found live, 2026-08-06: that pin (`playwright@1.55.0`) stopped
# matching -- `v1.55.0-noble` now resolves to a digest actually bundling
# chromium-1187, not the chromium-1140 that npm's `1.55.0` expects (`Executable
# doesn't exist at /ms-playwright/chromium-1140/...`). Chasing a replacement
# fixed version number is chasing a moving target -- MCR tags apparently get
# repatched in place (browser security updates) without a version bump, so any
# hardcoded npm pin can drift again the same way. Real fix instead: still
# install a real, deterministic npm version (`npm ci`-style reproducibility
# matters more here than ever, given the tag already proved unreliable), but
# then run `playwright install chromium` -- we're root during this build (no
# sudo needed, unlike on the host itself) -- so whatever browser revision this
# npm version actually expects gets fetched for real, instead of assuming it's
# already sitting in this image under the right name.
FROM mcr.microsoft.com/playwright:v1.55.0-noble

WORKDIR /work
RUN npm init -y >/dev/null && npm install playwright@1.62.1 && npx playwright install chromium

# Scripts and output are bind-mounted at run time (see simulated-user.sh) -- this
# image only carries the pinned runtime, never the actual test scripts, so the same
# image works for any real walkthrough script pointed at it.
ENTRYPOINT ["node"]
