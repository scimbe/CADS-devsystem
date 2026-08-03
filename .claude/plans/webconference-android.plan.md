# Check-in: `webconference-android` -- iteration 4

## Run summary

4 iteration(s) so far, 5 role(s) currently in the live spec.

- iteration 1 (`devsystem.implement`, ok): Inspected the scaffold (CADS-webconference-android@4ea9b88) and CADS-Tunnel's crates: MainActivity is a placeholder TextView, matching the f...
- iteration 2 (`devsystem.test`, ok): The run's spec had no devsystem.test role yet, and the Android repo had zero tests -- assembleDebug only proved the scaffold compiles, not t...
- iteration 3 (`devsystem.verify`, ok): Verification had only ever run manually, by hand, on the production host inside a locally pulled mingc/android-build-box -- no continuous ch...
- iteration 4 (`devsystem.review`, ok): Ran a real review pass over the scaffold (not just the newest commit): AndroidManifest.xml, MainActivity.kt, build.gradle.kts, and the new C...

**Stage:** `devsystem.review`

## What this stage found

Ran a real review pass over the scaffold (not just the newest commit): AndroidManifest.xml, MainActivity.kt, build.gradle.kts, and the new CI workflow. Two genuine findings, both fixed: (1) android:allowBackup="true" (the default) on a project whose real purpose is a Noise_IK/Agent-Fabric client -- cheap to close off now, before any real session/key material exists, rather than after. (2) MainActivity set padding with raw pixel ints, not density-aware -- moved to dp values in a new dimens.xml. Verified both fixes hermetically (testDebugUnitTest 1/1, assembleDebug) and confirmed the real CI run (30859108660) passed end to end. Commit CADS-webconference-android@484ea56.

## Proposals

### `devsystem.review`

- **Proposed by:** `devsystem.review`
- **Tag / units:** `review` / 1
- **Existing service to reuse:** none -- a new service must be built or provided

The review stage is now real for this run: a genuine pass over the existing code found and fixed a real (if currently low-stakes) security issue and a real correctness issue, both verified hermetically and against a real CI run rather than assumed fixed.

## Stages added to the live spec so far

- `devsystem.android_native_bridge`
- `devsystem.test`
- `devsystem.verify`
- `devsystem.review`

## Stalled stages (devsystem.improve)

Proposed and live in the spec, but no iteration has run *as* these stages yet -- likely blocked on a pending human decision:

- `devsystem.android_native_bridge`

## Decision needed

Reply `approve` to accept this iteration's proposals as-is and let the next iteration proceed, or `request-changes` with your answer/direction (this canvas live-reloads on `--reply`).
