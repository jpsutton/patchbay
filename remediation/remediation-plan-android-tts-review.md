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
- [ ] Frontend checks, native matrix and emulator checks pass; a regression fails with refresh disabled.
- [ ] An exact-revision APK is delivered with checksum and installation instructions.
- [ ] User confirms physical-phone voice quality, Stop, voice and speed selection.
- [ ] No PR is opened before physical-phone acceptance.

## Plan

- [x] Step 1 — Implement CI wiring and lifecycle refresh with regression cases.
- [ ] Step 2 — Run automated gates, build and verify the native APK in the emulator.
- [ ] Step 3 — Commit/push and deliver the APK; await the user's phone results.

## Verification strategy

Run speech.selftest.mjs, typecheck/builds, Android controller tests and lint. Inspect XML
counts and exercise refresh with and without the implementation. Run native emulator
scenarios and validate resume through the app lifecycle. Record the APK checksum and exact
revision. Physical acceptance remains pending until the user reports device results.

## Execution history

| Date | Agent | Action | Result | Evidence |
|---|---|---|---|---|
| 2026-10-25 | Codex | Recorded selected options 1A and 2A | Implementation active; PR held | User request |
| 2026-10-25 | Codex | Added resume refresh and CI jobs | 192 native tests pass; lint, frontend routing, types, mobile build and actionlint pass | Gradle XML; /tmp/patchbay-tts-resume-tests.log |
| 2026-10-25 | Codex | Disabled refresh, then restored it | 36 expected failures in 48 cases; restored suite has 192 passes | /tmp/patchbay-tts-refresh-negative.log |


## Handoff / blockers

- Status: none
- Next action: Complete exact-revision emulator verification and deliver the APK for the user's physical-phone check.
- Artifact receipt: local `artifacts/native-tts-followup/receipt.md` records revision, checksum and final automated results.
- Physical acceptance: David Frazer must confirm voice quality, Stop, voice and speed selection before a PR opens.
- Hosted Actions: workflow linted and commands exercised locally; hosted run remains pending.

## Review

- Reviewer: user for physical-phone acceptance
- Verdict: pending
- Evidence: To be recorded after implementation and device validation.
