# Android native TTS physical acceptance

Physical acceptance passed on October 25, 2026. The user listened to audio captured from
the physical Pixel and answered **“Yes, it sounds natural.”** This confirmation applies
to the physical recording, separately from earlier emulator listening results.

## Tested build and device

- Source revision: `a2b64ca193a1415889f6b7c638bb14d19287f3a0`.
- APK SHA-256: `e59a37192c2b2f4a70752d7929699e85d738bde5fb81202c74dd08478ad9b9c9`.
- Device: physical Pixel 10 Pro XL, Android 17/API 37, arm64, 4096-byte pages.
- Patchbay Remote 0.13.1 (13001), `dev.patchbay.remote`, debug signer.
- Candidate checksum matched SHA256SUMS and the installed APK. The release signer differed;
  installation proceeded only after the user uninstalled the release and authorized the
  custom build. The agent did not uninstall or clear application data.

## Observed results

| Check | Physical observation |
| --- | --- |
| Automatic engine, Default voice | Reply completed in 9.858 seconds; active playback control observed. |
| Stop and replacement reply | Stop UI observed within 100 ms. New reply completed in 9.873 seconds; old reply stayed stopped, including an additional 2.5-second observation. |
| Installed voice and speeds | `en-us-x-tpf-local`: Slower 12.415 s, Normal 11.264 s, Faster 9.913 s for the same reply. All completed in the expected duration order. |
| Default follows system changes | Changed Android TTS language from system default (effective en-US) to English (UK). Subsequent synthesis used en-GB without restarting Patchbay. Only Google TTS was installed; switching between two installed engines was not tested. |
| Return during playback | Home/return preserved active playback. Native logs show reconnect after the current passage, before the next synthesis request; no Stop during passages. Long reply completed in 42.458 seconds. |

PID 15865 remained unchanged through acceptance. Android Settings taps used current UI-tree
bounds. Patchbay's WebView lacked accessible child nodes, so app controls were exercised
through semantic DOM interactions over the physical WebView debugging connection.
UI completion timings measure observed playback state, rather than precise acoustic duration.

Audio came from the physical device's output through scrcpy, captured locally as FLAC.
The listening MP3 was trimmed and amplified; speed and pitch were unchanged. Neither
recording was uploaded. Automated checks establish behavior; the user's listening response
establishes the reported voice-quality acceptance.

## Preservation and limitations

App preferences matched the saved baseline exactly after testing. Android TTS engine,
language, rate and pitch keys were restored exactly, including the pre-existing stale
engine key. USB stay-awake was restored from temporary value 2 to original value 0.
Only a new disposable Code session was used; existing conversations were not edited.

- A native destroyed-mutex error occurred in pre-test PID 11238. It was not reproduced
  during acceptance; its cause remains untriaged.
- Gateway connectivity became intermittent. Requests to stop/archive the disposable
  session timed out, so cleanup outcome is unconfirmed.
- The existing 16 KB native-library alignment issue remains outside this fix. This phone
  uses 4 KB pages; the debug compatibility warning was recorded and dismissed once.
- These observations cover this physical model/API only. Earlier emulator and simulated
  framework coverage is documented separately in the original handoff receipt.

## Local evidence

Raw evidence is retained locally, outside the committed receipt:

- `artifacts/native-tts-followup/physical-tests-20261025T235142Z/` contains
  `acceptance-results.json`, five case result files, lifecycle/log excerpts, screenshots,
  restoration checks, pre/post-test crash logs, and the original/listening recordings.
- `artifacts/native-tts-followup/physical-install-20261025T233601Z/` contains
  checksum, signing, installation and launch evidence.
- `artifacts/native-tts-followup/receipt.md` contains earlier automated and emulator results:
  192 native tests passed; disabling refresh produced 36 expected failures in 48 cases;
  frontend checks, lint and six emulator scenarios passed. Those checks were not rerun
  for this documentation-only update. Hosted Actions execution remains unverified here.

## Return to marketplace release

After testing, the user removed the custom app from the main profile. Droidify still failed:
Android retained the exact custom APK in Private space. Package-manager state showed it
installed only for Private space, and its SHA-256 matched the test APK above. Installer
logs showed `INSTALL_FAILED_UPDATE_INCOMPATIBLE`; an attempt with version code 13000
also produced `INSTALL_FAILED_VERSION_DOWNGRADE` against retained 13001.

The agent diagnosed this without removing data. The user removed the remaining private
copy, retried Droidify, and confirmed: **“That fixed it.”** Local diagnosis evidence is in
`artifacts/native-tts-followup/droidify-install-20261026T001408Z/` (UTC directory date).
The phone is now on the marketplace release; this branch's native TTS fix is unreleased.
No PR, merge, release publication, or recording upload was performed.
