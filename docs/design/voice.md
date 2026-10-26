# Read aloud

Status: implemented on `explore/voice-read-aloud`. Settings → Chat holds the engine, voice, speed, Listen, and (except on the phone) a speech server. Read aloud sits under finished chat replies and settled Code prose. Listen speaks the next finished reply while that view is on screen. Esc stops speech before it cancels a turn. The Mac app uses `say`; Windows, browsers, and Android use `speechSynthesis` when it lists voices; Linux defaults to the in-page voice. A speech server is `POST /v1/audio/speech` from the process that holds the key, and the page plays the clip. This is the whole feature. There is no voice mode past it: no dictation, no hands-free turn-taking, and no Whisper or Piper runtime in the app.

## Goal

Click a control under a finished reply and hear that reply. Click again, or start another one, and it stops. The words stay on the device the person is using. No provider, API key, or account is involved.

The button is the same on every surface, and it speaks on every surface. A missing system voice is not a reason to hide it.

That is the whole feature. Listen, on the composer, speaks each new finished reply while that view is on screen. It does not listen to the microphone.

## Where it has to work

These are the builds in `.github/workflows/build.yml` and the install docs. Sound comes out of the device showing the UI. A session hosted on another computer is already on this screen, so playback never travels over the mesh and never runs on the host.

| Surface | What renders the UI | What ships | Default voice |
|---|---|---|---|
| macOS app | WKWebView | `.dmg`, Apple silicon (`macos-latest`) | The Mac's own voices, through `say`. They are part of the OS. |
| Windows app | WebView2 | `.exe` and `.msi`, x86_64 | WebView2 `speechSynthesis`. Windows 10 and 11 desktop installs include OneCore voices. |
| Linux app | WebKitGTK 4.1 | `.deb` (Ubuntu 24.04 / Debian 13), `.rpm` (Fedora, openSUSE), AppImage, Arch. glibc 2.39 or newer. CI builds x86_64. | The bundled synthesizer. The packages do not depend on a speech engine, and WebKitGTK's `speechSynthesis` is not the Linux path. |
| `patchbayd` | The user's browser | Daemon packages for Linux, Windows, and macOS, including a machine with no display | The browser's `speechSynthesis` when it lists voices. Otherwise the bundled synthesizer. The daemon itself stays silent. |
| Patchbay Remote | Android System WebView | arm64 APK, `minSdkVersion` 26. x86_64 is the emulator build. | Android `speechSynthesis` when the phone has a TTS voice installed. Otherwise the bundled synthesizer. |

`src-tauri/src/web.rs` already serves `.wasm` as `application/wasm`, so a synthesizer shipped in the frontend loads in the desktop windows and in a browser on `patchbayd`.

Preferences do not sync. The mesh table list in `src-tauri/src/mesh/sync.rs` has no `settings` row. A voice chosen on a Mac stays on that Mac, which is what we want: voice ids are not portable, and a phone driving a Linux session speaks with the phone's engine.

## What already exists

The action rows are the only place the control has to land. Chat and Code share one UI across the desktop, the browser, and the phone.

- Chat replies: `Message` in `src/chat/ChatView.tsx`. After the stream ends (`!live`), the row has Copy Markdown, Copy as plain text (`body.current.innerText`), Regenerate, and the branch switcher. The row is invisible until hover, unless the message has siblings.
- Code prose: `AgentEntry` in `src/code/CodeView.tsx`. Same hover row, with only Copy Markdown. Thoughts, tool cards, diffs, errors, and user bubbles do not get it.
- Code text arrives as ACP `agent_message_chunk` and grows the last `{ kind: "agent" }` entry in `appendText` (`src/lib/store.ts`) for as long as the turn is busy. A bubble is finished when a later entry closes it, or when the session goes idle.
- Preferences are one JSON document, `src-tauri/src/prefs.rs`. New fields with `#[serde(default)]` need no migration.
- Desktop-only commands live in `src-tauri/src/desktop.rs`. Shared commands go through `api.rs` and also serve `patchbayd`. Playback of a system voice on macOS can be desktop-only. Everything else lives in the page, because the phone and the browser have to speak too.
- Grok Build vendors a voice crate, `vendor/grok-build/crates/codegen/xai-grok-voice`. Its own header says what it is: dictation only. The mic streams to xAI speech-to-text and the transcript lands in the Grok pager's prompt box. The user still presses Enter. There is no text-to-speech, it needs an xAI bearer token, and Patchbay never calls it. It cannot read a Claude, OpenRouter, or Ollama reply, and it should not be bent into that.

## The control

A labeled button, not another 28px icon. Claude's is a speaker plus the words "Read aloud", sitting with the timestamp. Patchbay's row is icon-only, so a text button will be the wide one on the left of that row. The same component is used in `ChatView` and `AgentEntry`, so the phone and the browser get it with no second implementation.

| State | Label | What a click does |
|---|---|---|
| Idle | Read aloud | Speak this reply. Anything already speaking stops. |
| This reply is speaking | Stop | Stop. The row stays visible, hover or not, so Stop can be reached. |
| Another reply is speaking | Read aloud | Move playback to this reply. |
| The engine failed | Read aloud | Toast the error, the way a failed copy does. The button stays. |

Hidden while a chat message is `live`, and hidden on a Code agent entry that is still the growing tail of a busy session. An empty reply and a reply that is only an error stay quiet. User bubbles stay quiet in this version.

One playback at a time, in this window. Switching chats or starting a new reply stops speech. Esc (already "stop the reply") stops speech first and the agent turn second only if nothing is speaking, so Listen does not cancel the agent by accident. On the phone, leaving the app stops speech. The mesh keep-alive is for the connection, and it does not keep the speaker going under a locked screen.

The click is the user gesture. Browsers and both webviews refuse audio that starts on its own, so the first utterance of a session has to come from this button or from the Listen toggle. That is the same rule on macOS, Windows, Linux, Android, and a browser tab.

`Mod+Shift+C` already copies the last reply. A shortcut for the last reply can wait. The phone has no keyboard shortcut row. Add a desktop shortcut to `SHORTCUTS` in `src/lib/shortcuts.ts` only in a change that leaves the button working without it.

## What is spoken

Prose, in the order it appears. The string is not the markdown source and not a dump of `innerText`. The same string is handed to every engine, so a reply sounds like the same words on a phone and on a desktop.

- Speak paragraph text, list items, and link labels. Drop the URL.
- A fenced block of a few lines is spoken, prefixed with "code". A long one, and anything that is a diff, is replaced with "code block" so a session full of patches does not read syntax for a minute.
- Mermaid, images, and tables are skipped. The chat already renders those in `Markdown`.
- Reasoning and `{ kind: "thought" }` are never spoken. Tool cards are never spoken. A permission card is not read aloud in the first version. If Listen exists later, speech pauses while a card is up and a short "needs your approval" is enough.
- Chunk on paragraph and sentence boundaries. One giant utterance is how macOS speech cuts out, how Android sometimes never fires the end event, and how Stop feels late. The queue lives in the page. Stop drops the rest of the chunks and cancels whatever is playing now.

A shared `spokenText(markdown)` is the source for Chat and Code. Unit-test it on a reply with a fence, a link, and a paragraph. No playback in the test. A second test drives the chunk queue with a fake engine: start, stop mid-queue, and a failure that surfaces as a toast. Those two tests run in CI and cover every platform, because the platform code is only the thing that turns a chunk into audio.

## Engines

Speak locally. A cloud voice sounds more like Claude and uploads the reply. That fights the reason Patchbay keeps conversations on disk. Leave it as a later, opt-in preference for someone who already pays that provider.

Two engines, one queue. The page picks at click time. It never hides the button.

### Bundled, on every surface

A small offline synthesizer compiled to WASM, in the espeak-ng class. It returns PCM. The page plays that with Web Audio, which WKWebView, WebView2, WebKitGTK 4.1, Android System WebView, and ordinary browsers all have.

This is the engine that makes the feature real where the OS has nothing ready:

- Linux `.deb`, `.rpm`, AppImage, and Arch. AppImage in particular cannot assume `spd-say` or espeak is installed beside it. The desktop deb and rpm scripts do not pull a speech package in either. `scripts/package-daemon.sh` depends on libc and libdbus only.
- `patchbayd` opened from another computer, including over an SSH tunnel. The closet server has no speakers. The laptop's browser does. If that browser lists no voices, the WASM engine still speaks.
- A phone whose TTS data was never downloaded.
- A Windows image whose OneCore voices were removed.

Keep the WASM build single-threaded. A threaded one needs cross-origin isolation headers, and neither the Tauri windows nor `patchbayd` send those. English voice data on the order of a few megabytes belongs in the frontend bundle, which the APK and the daemon UI already ship. Other languages can follow the same way. They are not required for the first version.

Quality is plainly synthetic. That is acceptable as the floor, and it is the Linux default. It is the wrong default on a Mac, where the system voice is sitting right there.

### System, where it is actually installed

Used when a probe says the voice is really there. The probe is "at least one voice came back", with a short wait for `voiceschanged`. It does not speak a sample on startup.

| Surface | System engine | When it is the default |
|---|---|---|
| macOS app | `AVSpeechSynthesizer` via a desktop-only command in `desktop.rs`. WKWebView's own `speechSynthesis` is a poor default: the voice list starts empty and long utterances stop early. | Always. Voices ship with the OS. The bundled engine is the fallback if the command fails. |
| Windows app | `speechSynthesis` in WebView2. | When `getVoices()` is non-empty, which it is on a normal Windows 10 or 11 desktop. |
| Linux app | None in the first version. speech-dispatcher is a settings choice later, after a probe that runs a real short utterance, because a voice list that never speaks is worse than espeak. | Never. Bundled is the default. |
| `patchbayd` browser | That browser's `speechSynthesis` (Chrome, Firefox, Safari). | When the browser lists voices. Safari on a Mac will. Firefox on a minimal Linux desktop often will not, and then the bundled engine runs. |
| Android app | Native `android.speech.tts.TextToSpeech` through the Remote platform plugin. | Automatic and System use Android’s default engine after initialization. Missing voice data and engine errors are visible. Built-in remains an explicit choice. |

`say` on macOS is enough to prove the button and not enough to ship. Killing the process is a blunt Stop, and there is no reliable end event.

Speed is three steps (slower, normal, faster), mapped inside each engine. espeak's rate scale and `AVSpeechUtterance`'s rate scale are not the same number. The preference stores the step, not a raw rate.

## Settings

On the Chat section of `PreferencesPage`. Stored in the preferences JSON on this machine. The mesh does not sync `settings`, so a voice chosen here stays on this device. The phone does not write the desktop's preferences. Its own System and Built-in choices stay in the webview.

The first version is a short list. A row appears only when this device can use it:

- Engine. Automatic, plus System when a probe found voices, plus Built-in always. Automatic is the table in Engines.
- Voice, for that engine. Empty means the engine's default.
- Speed. Slower, normal, faster. Default normal.
- Listen for new replies in new chats and sessions. Default off.

An explicit engine that fails toasts the error and stops. It does not quietly switch to Built-in. Automatic can fall back on other platforms. On Android, Automatic reports native engine failures; Built-in must be selected explicitly.

No new database column. The per-conversation Listen switch can live in the window until it is worth saving. It does not go in a synced table.

## Pointing at another model or provider

This is a later setting on the same page, still per device, still only the rows that device can run. It does not change the button.

A local Whisper model does not belong in this list. Whisper, whisper.cpp, and MLX Whisper turn speech into text. They cannot read a reply, and dictation is not part of this feature. The way to point at a local speaking model is a speech server that implements `POST /v1/audio/speech`.

What people actually have, and what the row should accept:

| Thing on the device | Reads replies? | How the setting points at it |
|---|---|---|
| macOS / Windows / Android system voices | Yes | The System engine. The voice menu is the OS list. |
| Piper voice file (`.onnx` plus its `.json`), Kokoro, or a similar local TTS model | Yes, if something else is already serving it | No file path and no runtime in the app. Run it as a speech server and paste that URL. |
| A local server that implements OpenAI's `POST /v1/audio/speech` (Kokoro FastAPI, Speaches, LocalAI, and others) | Yes | A base URL on this device. This is the practical "point at a local model" control, because Patchbay does not have to ship a neural runtime. |
| An OpenAI provider already saved in Settings, or another OpenAI-compatible endpoint that really serves `/v1/audio/speech` | Yes | Pick that provider. The key stays in the keychain. Model and voice are fields (`tts-1`, `gpt-4o-mini-tts`, `alloy`, and whatever the server calls its voices). |
| Whisper, or an Ollama / LM Studio / Anthropic chat provider | No | Ollama, LM Studio, and Anthropic are chat endpoints. They do not grow a speech API by being in the provider list. Leave them out of the voice dropdown. |

One extra engine value covers the URL and the saved provider, because they are the same HTTP call. Call it a speech server. Fields: base URL, model, voice name, and where the key comes from (none, or an existing provider id). A Test button sends one short line and plays the audio. Saving does not call the network. The hint under the fields says that the reply text is sent to that address.

Choosing a saved provider fills the URL and the key. It does not create a second secret. Only `openai_chat` providers are offered. `openai_responses` and `anthropic` are not speech endpoints. A local server with no key is just a URL, so it does not have to be a chat provider at all.

Which devices show the row:

- Mac, Windows, and Linux apps. Yes. `localhost` is this computer. A file path, when that exists, is a path on this computer.
- `patchbayd`. The preferences and the keys live on the daemon. Synthesis runs there, and the audio bytes come back to the browser to play. A file path is a path on the server, not on the laptop. A URL of `127.0.0.1` is the server. The page should say that, because it is the easy mistake when the browser is on another machine.
- Phone. System and Built-in only, for the first cut of this setting. The phone holds no provider keys and no model files. Using the gateway's speech server would mean the gateway synthesizes and streams audio to the phone. That can wait, and the configuration would still be edited on the computer that has the key or the file.

Built-in stays on the device. Android system voices marked “network” may send text to the selected engine’s provider; downloaded voices can work offline. A speech server sends text to its configured endpoint. Listen uses whatever engine is selected, so a speech server is in the path of every new reply while Listen is on. The Test button is how you find out the URL is wrong before that happens.

## Not a voice mode

Read aloud is the feature. Listen speaks each new finished reply while that conversation is the one on screen. It does not speak the backlog when you open an old chat. It does not speak while the view is in the background. In a Code session it speaks finished agent prose only, never thoughts or tool lines. Turning Listen on does not start a microphone. The click is the gesture that unlocks audio.

Dictation and hands-free conversation are not planned. No mic button, no silence-to-send, no barge-in, and no transcription model. Do not wire `xai-grok-voice` in. It only transcribes for an xAI account, and it is not a reading voice.

Listen is a composer control, saved with the other read-aloud preferences on this device, default off.

## Files a first version touches

- `src/lib/spokenText.ts` — markdown to the words that are spoken, with tests.
- `src/lib/speech.ts` — the queue, the pick-order above, Web Audio for the bundled engine, `speechSynthesis` for Windows and the browser, native `TextToSpeech` for Android, one in-flight id, stop. A fake engine stands in for tests.
- The WASM synthesizer and its voice data, loaded by that module. Single-threaded.
- `src/components/icons.tsx` — a speaker and a stop square, same 24-grid as the other icons. The button shows the word too.
- `src/chat/ChatView.tsx` and `src/code/CodeView.tsx` — the button on finished assistant prose.
- `src/lib/shortcuts.ts` — Esc stops speech on desktop.
- `src-tauri/src/desktop.rs` and a small macOS `speech` module — `speak`, `stop_speech`, `list_speech_voices`. Compiled out of the web and Android builds. `isWeb()` and `isRemoteApp()` never call it.
- `src-tauri/src/prefs.rs` and `src/settings/PreferencesPage.tsx` — voice, speed, default Listen, on the desktop.

No Linux package dependency. No mesh change.

Android uses the existing Remote platform plugin to call `TextToSpeech` directly. Automatic
and System wait for engine initialization and list the phone’s installed voices. Empty voice
uses the engine’s default. Voice and speed are applied per utterance; completion and errors
settle the shared queue, Stop cancels current and pending speech, and the engine shuts down
when the plugin is destroyed. Initialization and playback have bounded timeouts. Android
engine failures are shown instead of silently switching to the formant fallback. Built-in
remains an explicit choice. The manifest declares the TTS service query for Android 11+.
The Pixel playback check is still required to assess perceived voice quality.

## Checked by hand before calling it done

CI cannot hear the speaker. Each of these is one reply, then Stop during the second sentence:

- macOS `.dmg`. System voice, and the bundled voice with the system command forced off.
- Windows `.exe`. A normal desktop, and a note if `getVoices()` was empty and the bundled voice took over.
- Ubuntu 24.04 `.deb` with no espeak and no speech-dispatcher installed. The bundled voice has to be the one that speaks.
- The AppImage on that same machine.
- Fedora `.rpm`, same expectation.
- `patchbayd` on a Linux box, opened in Chrome on a laptop through an SSH tunnel. Sound from the laptop. Nothing from the server.
- The same page in Firefox with no system voices, so the bundled engine is the one that runs.
- The arm64 APK on a phone that has a TTS voice, and on one that does not (or with TTS data removed). Both speak. The emulator is not enough. Its audio path is optional.

## Open

- Which WASM build of espeak-ng (or a peer of that size and license) we vendor, and the exact byte size it adds to the APK and the daemon UI.
- A long macOS reply, chunked, reaches the end, and Stop cuts the current chunk immediately.
- Verify Android native engine completion, Stop, and missing voice data on a physical phone.
- Whether Listen survives restart. Memory is enough until someone asks.
- Whether the latest reply's action row should stay visible without hover, the way Claude keeps Read aloud on screen. The rest of Patchbay hides that row. Forcing it for the latest message makes the control findable and changes the chrome. Try it on the latest message only.
- speech-dispatcher as an optional Linux system voice, behind a probe that really speaks. Not part of the first version, and not a package dependency.
- A Piper file path, a Whisper path, dictation, and hands-free turn-taking. Dropped. A local speaking model is a speech server URL, which is already a setting.
