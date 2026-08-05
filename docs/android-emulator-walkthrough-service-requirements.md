# Android emulator/device walkthrough service — requirements for labor-setup.com

Handed off for external implementation (operator's own decision, 2026-08-05), same shape as the
[build-service handoff](android-build-service-requirements.md): this host has no Android
emulator/SDK/`adb` tooling and no `sudo` to install any (confirmed directly — `which adb
emulator avdmanager` all empty, `npx playwright install chrome` failed on `sudo: a terminal is
required to authenticate`). Real on-device verification of the messaging app has never actually
happened as a result — every claim about the app's UX so far has been about the *compiled
artifact* (a real `app-debug.apk`, real `libnative_bridge.so` linked in, `nm`-verified symbols),
never about *watching it run*. That gap is real and is what this request closes.

## What exists today

- **A real, compiled `app-debug.apk`** (`org.bunsenbrenner.webconference`, `versionName
  0.1.0-scaffold`) — confirmed via the APK's actual zip contents, not just the build log: has
  `classes.dex`, a real `AndroidManifest.xml`, and `libnative_bridge.so` linked for both
  `arm64-v8a` and `x86_64`.
- **The messaging feature it's meant to exercise**: real Noise_IK keypair generation, a real
  direct peer-to-peer channel session (`dial_channel_direct`/`bind_channel_listener`), and a real
  wire format for text messages (`send_text`/`recv_text`), all wired into `MainActivity` via
  UniFFI — see `CADS-webconference-android`'s own README Status section for the current,
  non-overclaimed state.
- **The run's own declared milestone** (`runs/webconference-android/state.json`, live on
  `devsystem-demo.bunsenbrenner.org`): *"M1: 1:1 Text-Messaging end-to-end: Android-Client sendet
  und empfängt Textnachrichten über einen CADS-Tunnel Channel (Noise_IK), Verlauf lokal
  persistiert"* — not yet achieved, and can only honestly be marked achieved once someone has
  actually watched two real app instances exchange a message.
- **A `devsystem.android_emulator_test` role tag already exists** in this system (added on a
  separate verification run, currently stalled/unfilled) — the naming precedent is already set,
  this request formalizes it as a real, biddable role on the `webconference-android` run itself.
- **The M2M report-back path now works, live** (CADS-devsystem#7/#12, CADS-Tunnel PR #390):
  `devsystem_iterate --remote` can report real results back through `devsystem-demo
  .bunsenbrenner.org`'s login gate using a real Keycloak `client_credentials` bearer token — no
  more "blocked on gate access" excuse for report-back, the same mechanism issue #12's build
  service used to confirm this hand-off.

## What's being asked for

A real, containerized Android emulator service, running on **your own infrastructure** (not this
host), that:

1. Runs a real, headless Android emulator (recommend the official `emulator` binary from
   `cmdline-tools` + an `arm64-v8a` system image, matching the APK's own linked ABI --
   `-no-window -no-audio`, GPU mode your call depending on what your host's virtualization story
   looks like -- see the open question below) inside Docker.
2. Installs the real APK (built by your own build service from #12/PR #9 -- reuse that pipeline
   rather than a separate build step) and drives a **real, scripted walkthrough**: launch the
   app, and -- this is the actual point, not just "app opens" -- run **two emulator instances**
   that establish a real direct channel session and exchange a real text message, proving the
   run's own M1 milestone for real.
3. Captures real screenshots at each meaningful step (launch, keypair generated, channel
   connected, message sent on device A, message received on device B) and a real `adb logcat`
   excerpt covering the exchange -- not a curated/cherry-picked log, the real one.
4. Reports back over the same M2M mechanism #12 already proved: `devsystem_iterate --remote`
   against `devsystem-demo.bunsenbrenner.org`, authenticated with a dedicated service-account
   credential (see Credentials below -- deliberately *not* the same identity this session's own
   CLI uses, so your access is independently revocable/auditable).

## Real open question, flagging rather than guessing

**UI automation tool.** Two real options, both would work, genuinely don't have a strong reason
to prefer one:
- **Maestro** (mobile.dev) -- a single static binary, YAML flow files, built-in `takeScreenshot`
  command, drives over `adb` under the hood. Lower setup cost, no Node/Appium server to run.
- **Raw `adb shell input tap/text` + `adb exec-out screencap`**, scripted directly -- zero extra
  tooling beyond `adb` itself (which the emulator already needs), but you write the tap
  coordinates/waits by hand instead of a declarative flow file.

Pick whichever fits your infra better and say which, same as the embedding-provider flag in #7.

**Virtualization**: does your host expose `/dev/kvm` (nested virtualization) to the container?
Real hardware acceleration makes this fast and reliable; software rendering
(`-gpu swiftshader_indirect`) works but is slow and worth knowing about up front rather than
discovering mid-implementation. State what you actually have.

## Credentials

A dedicated Keycloak M2M service account (`client_credentials` grant, same mechanism as #12) will
be provisioned for this service specifically -- separate from any other identity, so it's
independently revocable. Say when you're ready and it'll be created and allow-listed on
`devsystem-demo.bunsenbrenner.org`'s login gate; the operator relays `client_id`/`client_secret`
out of band, never through this issue thread.

## Real constraints the implementation must respect

- **No new heavy infra on this host.** Same rule as #12 -- the whole point is running this
  somewhere with real resources, not adding a second thing that OOMs a 7.6GB box.
- **Real screenshots land as real files.** Commit them into `CADS-webconference-android`
  (propose `docs/emulator-walkthrough/` alongside a short markdown writeup, mirroring exactly how
  `CADS-devsystem-docs`' own tutorial screenshots are real files in a real repo, not uploaded
  through a new binary-upload API this system doesn't have) -- unless you have a real reason to
  prefer something else, say so.
- **No fabricated success.** If the two-device message exchange doesn't actually work on the
  first real attempt, that's a real, useful finding -- report it honestly via
  `devsystem_iterate --remote` with `succeeded: false` and real logs, don't retry until it looks
  good and only report the good run.
- **Hermetic test coverage for any new driver/glue code** you write (the Maestro flow file or
  the adb-scripting glue itself), matching every other piece of this project.

## What is explicitly out of scope for this request

- Changing `native-bridge/`'s or `MainActivity`'s own code -- this is verification, not feature
  work. If the walkthrough surfaces a real bug, report it (issue or `devsystem_iterate` proposal)
  rather than silently patching around it.
- Play Store submission (M2 on the run's own milestone list) -- that's real, separate, later work.
- Broker-mediated channel registration/NAT traversal for the messaging feature itself (tracked in
  the Android repo's own `docs/channel-join-options.md`) -- this request assumes direct-address
  channel setup between the two emulator instances is enough to prove the milestone.

## Deliverable

A real, running emulator service (wherever you host it) plus:

1. A PR or direct commits to `scimbe/CADS-webconference-android` with real screenshots + a real
   walkthrough writeup under `docs/emulator-walkthrough/`.
2. A real `devsystem_iterate --remote` submission against the `webconference-android` run
   reporting the actual outcome (`succeeded: true` only if the two-device exchange genuinely
   worked) -- this is what lets the run's own M1 milestone be marked achieved for real, not
   asserted.
3. States which UI-automation tool and which virtualization mode were actually used, and why.
