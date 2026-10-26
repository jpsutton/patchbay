# Remediation plan: Android read-aloud review follow-up

## Metadata

- ID: `android-tts-review`
- Status: awaiting-review
- Created: 2026-10-25
- Updated: 2026-10-25
- Owner: Codex; David Frazer owns physical-phone acceptance
- Delegated to: none
- Source: User selected 1A and 2A after review of b79c8c9.

## Issue description

Run both speech regression suites in CI, refresh the Android engine and default voice on
resume, and provide an updated APK for physical-phone acceptance before opening a PR.

## Context and analysis

b79c8c9 adds native Android TTS after 990253a fixes read-aloud labels and timestamp alignment.
The native engine/default voice stay cached. Existing CI does not run the new speech suites.
Independent review passed 144 native tests, frontend speech routing, lint and the mobile build.

## Scope

### In scope

- Speech suites in GitHub Actions and regressions for resume refresh.
- Native refresh after the active passage, preserving pending initialization and cancellation.
- Updated arm64 debug APK, emulator validation and physical-phone handoff.

### Out of scope

- PR creation or merge before the phone check is accepted.
- Repairing the separately documented 16 KB native-library alignment issue.
- Altering the user's local audio recordings or other untracked artifacts.

## Acceptance criteria

- [x] CI invokes the frontend speech test and native controller tests/lint.
- [x] Resume reconnects the native engine and uses the new default without restarting the app.
- [x] Active playback and pending initialization survive refresh; destroyed engines stay closed.
- [x] Frontend checks, native matrix and emulator checks pass; a regression fails with refresh disabled.
- [x] An exact-revision APK is delivered with checksum and installation instructions.
- [x] Physical checks verify Stop, voice and speed selection; user confirms physical-phone voice quality.
- [x] No PR is opened before physical-phone acceptance.

## Plan

- [x] Step 1 — Implement CI wiring and lifecycle refresh with regression cases.
- [x] Step 2 — Run automated gates, build and verify the native APK in the emulator.
- [x] Step 3 — Commit/push and deliver the APK; record the user's phone results.

## Verification strategy

Run speech.selftest.mjs, typecheck/builds, Android controller tests and lint. Inspect XML
counts and exercise refresh with and without the implementation. Run native emulator
scenarios and validate resume through the app lifecycle. Record the APK checksum and exact
revision. Physical acceptance requires recorded device results and the user's listening confirmation; both are now recorded below.

## Execution history

| Date | Agent | Action | Result | Evidence |
|---|---|---|---|---|
| 2026-10-25 | Codex | Recorded selected options 1A and 2A | Implementation active; PR held | User request |
| 2026-10-25 | Codex | Added resume refresh and CI jobs | 192 native tests pass; lint, frontend routing, types, mobile build and actionlint pass | Gradle XML; /tmp/patchbay-tts-resume-tests.log |
| 2026-10-25 | Codex | Disabled refresh, then restored it | 36 expected failures in 48 cases; restored suite has 192 passes | /tmp/patchbay-tts-refresh-negative.log |
| 2026-10-25 | Codex | Verified candidate APK checksum and v2 debug signature; inspected adb inventory for physical acceptance | APK preflight passed; only emulator connected and user confirms phone absent. No installation or settings changes; all five physical checks and physical listening confirmation remain pending | `artifacts/native-tts-followup/physical-acceptance-20261025T214148Z/receipt.md` |
| 2026-10-25 | Codex | Identified the physical Pixel 10 Pro XL (Android 17/API 37, 4 KB pages); verified candidate checksum and both APK signing certificates | Signing mismatch: installed release signer `94c52e27…` differs from candidate debug signer `c0b00684…`. Stopped before installation; no app data, mesh, settings or conversations changed. All five physical checks and listening confirmation remain pending | `artifacts/native-tts-followup/physical-acceptance-20261025T232641Z/receipt.md` |
| 2026-10-25 | Codex | User reported uninstalling the release and authorized installing the custom build; confirmed package absence, checksum, successful physical install and launch | Installed APK hash matches the a2b64ca artifact. Pixel reaches Join your mesh after dismissing recorded 16 KB warning. Signing conflict cleared by user action; pairing and all five physical tests/listening confirmation remain pending | `artifacts/native-tts-followup/physical-install-20261025T233601Z/receipt.md` |
| 2026-10-25 | Codex | Ran all five checks on the physical Pixel using the exact a2b64ca APK; restored app preferences, system TTS keys and stay-awake setting | Five checks passed. User listened to the physical recording and confirmed: “Yes, it sounds natural.” | `docs/validation/android-native-tts-physical.md`; local `artifacts/native-tts-followup/physical-tests-20261025T235142Z/acceptance-results.json` |
| 2026-10-25 | Codex | Diagnosed Droidify reinstall failures after the user removed the custom build | Custom APK remained in Private space. Android logged signing mismatch and an older-version attempt. User removed the remaining copy and confirmed marketplace installation worked | Local `artifacts/native-tts-followup/droidify-install-20261026T001408Z/receipt.md`; user confirmation |

## Handoff / blockers

- Status: awaiting-review; physical acceptance passed.
- Next action: Review the implementation and the observations below. Do not open a PR until the user requests one.
- Physical evidence: `docs/validation/android-native-tts-physical.md` records all five results and human listening acceptance. Recordings, APKs, screenshots, preferences and raw device logs remain local under `artifacts/native-tts-followup/` and are excluded from this commit.
- Pre-test native shutdown: PID 11238 emitted a destroyed-mutex error before acceptance. Not reproduced during acceptance PID 15865; cause remains untriaged.
- Disposable session cleanup: stop/archive requests timed out during gateway connectivity problems. Outcome is unconfirmed; only the disposable session was targeted.
- Existing 16 KB ELF-alignment limitation remains out of scope. Physical testing used a 4 KB-page phone.
- Hosted Actions: workflow linted and commands exercised locally; hosted execution has not been verified in this acceptance record.
- User returned to the marketplace release after acceptance; it does not include this branch's unreleased fix.

## Review

- Reviewer: user for physical voice quality; Codex for recorded physical functional checks
- Verdict: physical acceptance passed; broader implementation review remains pending
- Evidence: `docs/validation/android-native-tts-physical.md`. Earlier emulator and simulated-framework checks remain separate evidence. The user explicitly confirmed the physical Pixel recording sounds natural.
