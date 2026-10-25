# Android native read-aloud coverage

Validation date: October 25, 2026. Scope: the native Android read-aloud update requested
after robotic playback on a Pixel. This covers the TTS implementation and its APK integration;
it does not certify every Patchbay screen on every Android version or vendor device.

## Version boundaries

- **Install floor: Android 8.0, API 26.** Both the app and platform plugin declare
  `minSdk = 26`; the built APK also declares 26. This update does not raise that floor.
  Android 7.1 and earlier remain outside Patchbay's declared support.
- **Target: Android 17, API 37.** The app and built APK target 37. The plugin compiles
  against 36, which is sufficient for the APIs it uses; compile SDK is not the install floor.
- **No declared upper install boundary.** The APK has no `maxSdkVersion`. Targeting 37
  does not certify future Android releases. They require regression testing when available.
- **APK architecture:** the debug artifact built for this change contains `arm64-v8a`
  only. A 32-bit-only phone or x86 emulator cannot install that artifact even when its Android
  version meets the minimum. This is a packaging boundary, not a TTS API requirement.

## API audit

| Calls used by the implementation | Android API introduced |
| --- | --- |
| `TextToSpeech`, default-engine construction, `stop`, `shutdown`, speech rate | 4 |
| `UtteranceProgressListener`, completion and basic error callbacks | 15 |
| `TextToSpeech.getMaxSpeechInputLength` | 18 |
| `Voice`, voice enumeration/selection, network and installation features, `speak(CharSequence, int, Bundle, String)`, audio attributes, detailed error callback | 21 |
| `UtteranceProgressListener.onStop` | 23 |

The newest TTS API used is API 23, below the app's API 26 minimum. No new runtime
permission is requested for TTS. The Android 11/API 30 package-visibility boundary requires
an `android.intent.action.TTS_SERVICE` intent under `<queries>`. It is present in the plugin
manifest, the merged app manifest, and the actual debug APK.

Official references: [TextToSpeech](https://developer.android.com/reference/android/speech/tts/TextToSpeech),
[Voice](https://developer.android.com/reference/android/speech/tts/Voice),
[UtteranceProgressListener](https://developer.android.com/reference/android/speech/tts/UtteranceProgressListener),
[Engine features](https://developer.android.com/reference/android/speech/tts/TextToSpeech.Engine),
and [SDK manifest semantics](https://developer.android.com/guide/topics/manifest/uses-sdk-element).

## Packaging issue: 16 KB-page devices

The checked APK is not ready to claim native 16 KB-page compatibility. Inspection of
`lib/arm64-v8a/libpatchbay_remote_lib.so` **extracted from the actual APK** found every
ELF `LOAD` segment aligned to `0x1000` (4 KB), below the required `0x4000` (16 KB).
`zipalign -c -P 16 4` passed for the APK. ZIP alignment and ELF alignment are separate checks;
the successful ZIP check does not clear the failing ELF check.

Android supports 16 KB-page devices beginning with Android 15/API 35. This can limit the
whole app before read-aloud runs. Some devices offer backward compatibility mode, but that
is not a substitute for a rebuilt and tested library. The emulator used for playback has 4 KB
pages. The native TTS update adds Kotlin code and reuses the Rust bridge; it adds no extra
native library. This validation does not establish when the packaging issue was introduced.

Required follow-up: rebuild the Rust native library with 16 KB-compatible linker settings,
verify every packaged native library and APK alignment, then install and exercise the app on
a 16 KB-page emulator/device. See Android's [16 KB page-size guidance](https://developer.android.com/guide/practices/page-sizes).

Checked artifact: `app-universal-debug.apk`, version 0.13.1 (13001), SHA-256
`e16888fb2b87bf5ac9f83bb3cde6702bd53fa55097bd7cc2a446ab87b9797e9b`.

## Automated checks

**Final result: 144 tests passed; zero failed, errored or skipped.** Twelve cases ran at
all twelve integer API levels from 26 through 37. The XML result was checked both for its
aggregate counters and for exactly twelve cases per API; an empty or partial run does not
meet this result. This matrix covers the native controller in simulated frameworks.

| Android version | API | Native controller checks |
| --- | --- | --- |
| 8.0 | 26 | 12 passed |
| 8.1 | 27 | 12 passed |
| 9 | 28 | 12 passed |
| 10 | 29 | 12 passed |
| 11 | 30 | 12 passed |
| 12 | 31 | 12 passed |
| 12L | 32 | 12 passed |
| 13 | 33 | 12 passed |
| 14 | 34 | 12 passed |
| 15 | 35 | 12 passed |
| 16 | 36 | 12 passed |
| 17 | 37 | 12 passed |

Android Lint completed with **0 errors and 3 dependency-update warnings**, with no
`NewApi` or `InlinedApi` issues. Source API review and actual APK manifest inspection were
performed in addition to lint. The test runner uses Robolectric 4.17 and JDK 21. Its JVM
has a 2 GB heap and an explicit internal file-descriptor export needed by the API 36+
framework model; these settings affect only host-side tests, not app memory or permissions.


The native test class runs the same behavioral cases on each declared API in its `@Config`
list. Robolectric uses Android framework jars with a controllable speech engine; these tests
exercise Patchbay's controller but do not synthesize speech with vendor voice data.

Cases cover initialization and timeout, installed voice filtering, network voice labels,
selected/default voice and speed, waiting for playback completion, cancellation before and
after initialization, native stop callbacks, late callbacks, engine rejection, missing voices,
playback errors and timeout, and lifecycle shutdown.

Run from `remote/src-tauri/gen/android` after the first Tauri Android build:

```sh
./gradlew :tauri-plugin-remote-platform:testDebugUnitTest :tauri-plugin-remote-platform:lintDebug
```

The reports are under `remote/plugin/android/build/test-results/testDebugUnitTest/` and
`remote/plugin/android/build/reports/`. The frontend route and queue regression check is:

```sh
node src/lib/speech.selftest.mjs
```

## Device integration evidence

The unchanged production implementation was installed and tested on the local arm64
Android 16/API 36 emulator using its Google TTS engine. Invoking the app's native IPC
commands through its WebView found 17 voices. An English phrase completed after about
2.925 seconds; Stop settled an in-progress utterance in about 22 milliseconds. A nonexistent
voice returned an explicit error. Logcat confirmed connection to `com.google.android.tts`
and synthesis with its embedded voice. This verifies the WebView → Rust → Kotlin → Android
engine path; it is not a subjective listening assessment.

A subsequent digital recording captured the same native playback path through the emulator's
`streamAudio` endpoint at 48 kHz, mono, signed 16-bit PCM. The 10.41-second sample had only
silence trimming and volume normalization; its speed and pitch were unchanged. The user
listened to that recording and confirmed that it sounded normal. The generated MP3/WAV
artifacts remain local and are not part of the source commit.

## Remaining limits

- Physical Pixel playback, including the user's installed Android version and selected voice,
  still needs confirmation with the updated APK. The original robotic recording predates
  the native implementation and does not validate it.
- Only API 36 has actual emulator engine playback evidence. Simulated API runs do not
  substitute for full app installation, WebView compatibility, or real engine playback on
  API 26–35 or 37. No API 37 system image was listed by the configured SDK repository.
- Voice availability, quality, language data and network requirements depend on the selected
  engine and installed voices. Google and Samsung engines are not interchangeable test evidence.
  An Android version alone cannot guarantee a natural voice. Missing engines/data and timeouts
  are visible errors; Built-in remains an explicit alternative.
- Background, locked-screen, Bluetooth and vendor battery-management behavior are outside
  this foreground TTS validation. Future Android versions, OEM variants and 16 KB-page devices
  have not been certified by this check.
