# Check-in: `webconference-android` -- iteration 10

## Run summary

10 iteration(s) so far, 7 role(s) currently in the live spec.

- iteration 1 (`devsystem.implement`, ok): Inspected the scaffold (CADS-webconference-android@4ea9b88) and CADS-Tunnel's crates: MainActivity is a placeholder TextView, matching the f...
- iteration 2 (`devsystem.test`, ok): The run's spec had no devsystem.test role yet, and the Android repo had zero tests -- assembleDebug only proved the scaffold compiles, not t...
- iteration 3 (`devsystem.verify`, ok): Verification had only ever run manually, by hand, on the production host inside a locally pulled mingc/android-build-box -- no continuous ch...
- iteration 4 (`devsystem.review`, ok): Ran a real review pass over the scaffold (not just the newest commit): AndroidManifest.xml, MainActivity.kt, build.gradle.kts, and the new C...
- iteration 5 (`devsystem.improve`, ok): The run's spec had no devsystem.improve role yet, and the stalled_stages() detection mechanism built two commits ago (65d0c26) had never act...
- iteration 6 (`devsystem.implement`, ok): The check-in delivered on iteration 5 (mandatory checkin_every cadence) blocked the run pending a real human decision on devsystem.android_n...
- iteration 7 (`devsystem.implement`, ok): Wired the toolchain-spike artifacts from iteration 6 (UniFFI-generated native_bridge.kt, cross-compiled libnative_bridge.so for arm64-v8a/x8...
- iteration 8 (`devsystem.improve`, ok): real API smoke test: submitted via the new devsystem-web GUI backend, not the CLI -- proving the browser-driven path actually mutates the li...
- iteration 9 (`devsystem.review`, ok): Real Playwright UI test: submitted through the actual rendered page, not curl -- proving a human can drive the pipeline through this GUI.
- iteration 10 (`devsystem.web_gui`, ok): web/ (the GUI backend serving devsystem-demo.bunsenbrenner.org) had grown through 9+ real feature commits across many loop firings with zero...

**Stage:** `devsystem.web_gui`

## What this stage found

web/ (the GUI backend serving devsystem-demo.bunsenbrenner.org) had grown through 9+ real feature commits across many loop firings with zero automated tests of its own -- every prior verification was external (curl, Playwright), never committed as regression protection, while pipeline/ carries 43 real unit tests. Refactored the inline axum Router construction out of main() into api_router(AppState) and added 5 real integration tests driven via tower::ServiceExt::oneshot against the exact same router main() serves: empty-dir list, create (success + 409 duplicate + 400 invalid chars), get 404 for a nonexistent run, and a full iterate round-trip asserting state.json actually changes on disk. Verified hermetically in rust:1-slim with RUSTFLAGS=-D warnings (clean build, 5/5 passed), committed as 0f50e70, CI green on push. No route/handler logic changed -- test-only commit, no redeploy needed.

## Proposals

None this iteration.

## Stages added to the live spec so far

- `devsystem.android_native_bridge`
- `devsystem.test`
- `devsystem.verify`
- `devsystem.review`
- `devsystem.improve`
- `devsystem.android_native_build_ci`

## Stalled stages (devsystem.improve)

Proposed and live in the spec, but no iteration has run *as* these stages yet -- likely blocked on a pending human decision:

- `devsystem.android_native_bridge`
- `devsystem.android_native_build_ci`

## Decision needed

Reply `approve` to accept this iteration's proposals as-is and let the next iteration proceed, or `request-changes` with your answer/direction (this canvas live-reloads on `--reply`).
