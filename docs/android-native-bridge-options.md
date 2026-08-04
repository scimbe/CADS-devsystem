# Android native bridge: research for the `devsystem.android_native_bridge` decision

> **Resolved (2026-08-04): UniFFI, per this doc's own research lean below.**
> Real work has landed on [`CADS-webconference-android`](https://github.com/scimbe/CADS-webconference-android):
> a real `native-bridge/` UniFFI + `cargo-ndk` crate, with real Noise_IK
> public-key generation wired into `MainActivity` (not a stub/mock call),
> and several consecutive green Android CI runs building/verifying it. The
> `devsystem.android_native_bridge` role/stage itself is currently a
> pending proposal in `runs/webconference-android/` (the run's own spec was
> separately reset by the operator for a fresh setup-flow re-test, unrelated
> to this technical decision) -- see the main [README](../README.md)'s
> Status section for the run's current real state. The research below is
> kept as-is for the record; it's what the decision was actually based on,
> not retroactively rewritten.

Background: iteration 1 of the `webconference-android` run found that CADS-Tunnel's
Noise_IK handshake (`crates/client/src/noise.rs`) and Agent-Fabric/channel primitives
(`crates/common`) are pure Rust, with only a `wasm-bindgen` browser path today — no
Android/JNI path exists anywhere in this ecosystem. That iteration raised
`devsystem.android_native_bridge` as a real proposal, framed on
[CADS-Tunnel#382](https://github.com/scimbe/CADS-Tunnel/issues/382) as a choice
between "`cargo-ndk` or `UniFFI`". **This doc does not decide that question** —
implementation is still correctly blocked pending a real human answer — it exists to
make that answer faster once someone looks, by actually researching both options
(not assuming) and correcting a framing error found along the way.

## Correction: this isn't really an either/or choice

`cargo-ndk` and `UniFFI` solve **different problems** and are commonly used
**together**, not as alternatives at the same layer:

| Layer | Tool | What it does |
|---|---|---|
| Cross-compilation | `cargo-ndk` | Cross-compiles a Rust crate to Android's target triples (`aarch64-linux-android` etc.), handles NDK toolchain/linker setup. Does **not** generate any Kotlin/JNI bindings itself. |
| Binding generation | hand-written JNI, **or** `UniFFI` | Exposes the compiled Rust functions to Kotlin as actual callable APIs. |

So the real decision is **hand-written JNI bindings vs. UniFFI-generated bindings**
— `cargo-ndk` (or an equivalent cross-compilation setup) is needed either way for the
`.so` build step. Worth restating on the issue when this gets picked back up, since
the original framing suggested a single either/or pick.

## Option A: hand-written JNI bindings + `cargo-ndk`

- `cargo-ndk` (bbqsrc/cargo-ndk, 953★, 86 forks, actively maintained, MSRV 1.86,
  CI present): purely the compile/link step — `cargo ndk` cross-compiles, handles
  NDK env setup, outputs `.so`s in the layout Gradle expects.
- Everything else — the actual JNI function signatures (`Java_org_..._nativeMethod`
  naming convention, `JNIEnv`/`jobject` marshaling, error propagation across the FFI
  boundary) — is written by hand, typically via the `jni` crate.
- **Pro**: no extra dependency, no codegen step to trust, full control over exactly
  what's exposed and how.
- **Con**: every function signature, every argument marshaled across the FFI
  boundary, is hand-written, unverified-by-tooling surface. For a Noise_IK handshake
  specifically, that means hand-marshaling key material and handshake state across
  the JNI boundary by hand — a real place for a subtle bug to hide.
- **Con**: no async story out of the box. The channel-join + handshake flow this
  project needs is very likely async (ct-client depends on `quinn`/`tokio`) — hand-
  written JNI would need a separate, hand-rolled bridge from Rust async to a Kotlin
  callback or `suspend fun`, on top of everything else above.

## Option B: `UniFFI`-generated bindings (still cross-compiled via `cargo-ndk`)

- Mozilla project (mozilla/uniffi-rs), 4.8k★, 322 forks, 2,223 commits, 262 open
  issues, active Matrix community — genuinely active, not abandoned.
- Maturity: "ready for production use" per its own docs, pre-1.0 but used
  extensively in Firefox for Android/iOS/desktop — real production track record for
  exactly this kind of Rust-core, multi-platform-binding use case.
- You declare the Rust interface (proc-macros or a UDL file); UniFFI generates the
  Kotlin bindings **and** the FFI marshaling code — the hand-written-JNI surface
  area above mostly goes away.
- **Directly relevant**: UniFFI has real async support — `async fn` in Rust maps to
  `suspend fun` in Kotlin (confirmed against its own docs, not assumed), which
  matches a channel-join/handshake flow much better than Option A's callback-only
  story. One documented limitation: no built-in cancellation — would need a manual
  cancellation flag/channel either way, same as Option A.
- **Con**: an added dependency + codegen step in the build; a small learning curve
  for this repo's first time using it.

## Framing for whoever makes the call

Given this project's own stated stakes for this decision ("real security stakes —
Noise_IK handshake correctness", from the iteration that first raised this) — the
research leans toward UniFFI reducing hand-written-FFI-boundary surface area for
exactly the code (key material, handshake state) where that surface area is most
dangerous, plus a real async story matching the actual channel-join flow. That's a
lean from research, **not a decision already made** — cost (new dependency, learning
curve) and control (full hand-written control) are real, legitimate reasons someone
could still prefer Option A, and the actual call stays with the human per the
original checkpoint on #382.

## If/when this gets answered

Whichever option: `cargo-ndk` still needs adding either way for cross-compilation.
Next real steps once a decision lands: pin `cargo-ndk` (and `uniffi` if chosen) at
specific versions, a minimal end-to-end spike (compile+call one trivial Rust
function from Kotlin) verified hermetically before touching the real Noise_IK code,
then the actual bridge work.
