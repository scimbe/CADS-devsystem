# Android/native-bridge build service — requirements for labor-setup.com

Handed off for external implementation (operator's own decision, 2026-08-05): this host
(4 CPU / 7.6GB / no swap, already running a real fleet — control plane, edge, Keycloak,
devsystem-web, multiple demo origins/agents) has repeatedly failed to cross-compile
`native-bridge/` locally. Rather than keep working around that on this box, the ask is to
run the real build service somewhere else entirely, reachable the same way every other
distributed piece of this project already is: a real Agent-Fabric channel.

## What exists today

- **`native-bridge/`** (in [`CADS-webconference-android`](https://github.com/scimbe/CADS-webconference-android)):
  a real UniFFI + `cargo-ndk` Rust crate. It now exports a working direct peer-to-peer
  Noise_IK channel session (`dial_channel_direct`/`bind_channel_listener`/`ChannelSession`,
  built on `ct_common::a2a`) plus the earlier keypair-generation and text-message wire-format
  pieces — see that repo's own `docs/channel-join-options.md` and README Status section for
  the real, current, non-overclaimed state.
- **The real, already-hit failure mode**: local `cargo-ndk` cross-compilation for both Android
  ABIs (`arm64-v8a`, `x86_64`) has been OOM-killed on this host multiple times — confirmed even
  at `-j1`/`codegen-units=1`. The workaround in use today is indirect: `verify-native-bridge`'s
  CI job (`.github/workflows/` in the Android repo) now uploads its freshly built `.so`/Kotlin
  artifacts (`if: always()`), and a human/agent downloads them via `gh run download` and commits
  them by hand. It works, but it means every native-bridge change needs a round-trip through
  GitHub Actions rather than a real, direct build.
- **`devsystem.android_native_bridge`** is already a real, live role in the
  `webconference-android` run's `PipelineSpec` (approved 2026-08-05, see
  [`pipeline/src/lib.rs`](../pipeline/src/lib.rs) and `runs/webconference-android/spec.json`) —
  a real `ServiceType::Custom` role any real bidder can win via `convene()`, exactly like every
  other stage in this project.
- **The real channel pattern already proven this session**: `pipeline/src/bin/github_issue_channel_client.rs`
  + `github_issue_channel_handler.rs` (#48) — dial a channel, send one real request, get one real
  reply, over a real Noise-encrypted session, with a respawn loop keeping the accept side
  reachable. This is the template for "a real service, running somewhere else, reachable from
  this host over a channel" — not a new pattern to invent.

## What's being asked for

A real, containerized Android/NDK build service, running on **your own infrastructure** (not
this host), that:

1. Has the actual heavy toolchain installed once — Android SDK + NDK (r27d, matching what
   `native-bridge`'s CI already pins — check the Android repo's workflow file for the exact
   version rather than guessing) + Rust + `cargo-ndk` — so cross-compilation runs at real,
   uncapped resource limits instead of getting OOM-killed on a 7.6GB host.
2. Is reachable from `devsystem-web`/this host over a real Agent-Fabric channel (the same
   `ct-agent channel` primitives, `ct_common::a2a`/`ct_common::channel` — whatever you judge is
   the right layer, direct-address like #48's respawn-loop pattern, or an actual auction win of
   `devsystem.android_native_bridge` and real `devsystem_iterate`/`devsystem_iterate --remote`
   submissions — state which and why).
3. Accepts a real build request (which commit/ref of `CADS-webconference-android` to build) and
   returns real results: either the compiled `.so`/regenerated Kotlin binding artifacts
   themselves, or — probably cleaner — commits and pushes them directly to the Android repo
   itself (matching how the CI-artifact-download workaround already ends today, just automated
   and moved off this host entirely).

## Real constraints the implementation must respect

- **No new heavy infra on this host.** The whole point is moving the resource cost off this
  box — don't reintroduce it as a second local container that just relocates the same OOM risk.
- **Real credential/access needed, must be stated, never assumed.** Whatever's needed to push
  to `CADS-webconference-android` (a scoped token, an SSH deploy key, or relaying through the
  same channel-agent pattern #48 already uses for GitHub writes) — say exactly what, the operator
  will provision it. Don't silently assume push access exists.
- **Preserve the existing verification bar.** Every artifact this service produces must be
  verified the same way every native-bridge change already has been this session: real
  `cargo test` (hermetic, `RUSTFLAGS=-D warnings`), a real symbol diff (`nm -D --defined-only`)
  confirming the new build's exports match what the source actually declares, and real CI
  (`verify-native-bridge`) passing on whatever gets pushed — not a claimed-but-unverified build.
- **Preserve the channel-based architecture, don't bypass it.** The point isn't just "a faster
  build machine" — it's a real demonstration that a devsystem role can be filled by a genuinely
  separate party over the real channel/auction primitives, the same thing this whole project
  (#382) is about. A build script that just SSHes in and runs commands defeats that purpose;
  state clearly how requests actually flow over a real channel.
- **Hermetic test coverage for the service itself**, matching every other piece of this
  project — if it's a new binary/handler (mirroring `github_issue_channel_handler.rs`'s shape),
  it needs its own real tests, not just "trust the build worked."

## What is explicitly out of scope for this request

- Changing `native-bridge/`'s own Rust code or the channel-session work already landed there.
- Broker-mediated channel registration, NAT traversal, or relay fallback for the *messaging*
  feature itself (that's tracked separately, see `docs/channel-join-options.md` in the Android
  repo) — this request is purely about *where the build runs*, not the messaging feature's own
  remaining scope.
- Any change to this host's own resource allocation — the fix here is moving work off it, not
  giving it more headroom.

## Deliverable

A real, running build service (wherever you host it) plus a PR or direct commits against
`scimbe/CADS-webconference-android` (and, if the channel-relay side needs new code on this
end, a PR against `scimbe/CADS-devsystem` too) that:

1. States explicitly which channel-connection shape was chosen (direct-address respawn-loop,
   auction-won role, or something else) and why.
2. Demonstrates a real, end-to-end round trip: a build request sent from this host's side over
   a real channel, a real cross-compile happening on your infrastructure (not this host), and
   real resulting artifacts landing back in the Android repo — verified live, the same
   "prove it against the real thing" standard this entire project has been held to, not just
   unit-tested in isolation.
3. Documents exactly what credential/access this host's side needs configured to talk to the
   service (channel address, peer key, cert file — whatever the real contract ends up being).
4. Confirms real CI (`verify-native-bridge`, `build-and-test`) goes green on whatever gets
   pushed as a result.
