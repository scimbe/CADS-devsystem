# Check-in: `webconference-android` -- iteration 1

**Stage:** `devsystem.implement`

## What this stage found

Inspected the scaffold (CADS-webconference-android@4ea9b88) and CADS-Tunnel's crates: MainActivity is a placeholder TextView, matching the flagship goal (a real Agent-Fabric channel-join + Noise_IK handshake + WebRTC client, matching ct-agent-wasm's browser behavior). Checked what's actually reusable: the Noise_IK handshake (crates/client/src/noise.rs) and the Agent-Fabric channel/service primitives (crates/common) are pure Rust; crates/common only exposes a wasm-bindgen ('js' feature) path for browsers today (see Cargo.toml comment: 'wasm-bindgen, not a syscall'). ct-agent itself lives in its own repo (scimbe/ct-agent). There is no existing Android/JNI path anywhere in this ecosystem. Reimplementing Noise_IK and channel-session logic natively in Kotlin would duplicate and risk diverging from the already-audited Rust implementation used by every other client (ct-agent, ct-client, the wasm browser path) -- a real correctness/security risk, not a style preference.

## Proposals

### `devsystem.android_native_bridge`

- **Proposed by:** `devsystem.implement`
- **Tag / units:** `android_native_bridge` / 1
- **Existing service to reuse:** none -- a new service must be built or provided

The Android client needs Agent-Fabric channel-join + Noise_IK, both already implemented in Rust (crates/client/src/noise.rs, crates/common). Proposing a new stage/service: build a JNI bridge (cargo-ndk or UniFFI) that compiles ct-client's/ct-common's existing Rust crypto+session code for android-arm64/x86_64 and exposes it to Kotlin directly, instead of reimplementing Noise_IK by hand in Kotlin. This mirrors the existing wasm-bindgen browser path (same Rust core, different target) rather than inventing a second, divergent implementation. This is an architecture-defining decision (which library/toolchain, how the JNI surface is shaped) with real security stakes (Noise_IK handshake correctness) -- it should get a human check-in before implementation work starts, not proceed unsupervised.

## Stages added to the live spec so far

- `devsystem.android_native_bridge`

## Decision needed

Reply `approve` to accept this iteration's proposals as-is and let the next iteration proceed, or `request-changes` with your answer/direction (this canvas live-reloads on `--reply`).
