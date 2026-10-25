import { useEffect, useState } from "react";
import { Choice, Deferred, Row } from "./PreferencesPage";
import { Toggle } from "../components/Toggle";
import { input } from "./ProviderPage";
import { primeAudio, speakText, systemVoices, type SpeechVoice } from "../lib/speech";
import { isAndroidApp, isRemoteApp } from "../lib/platform";
import { useStore } from "../lib/store";

/**
 * Reading voice for this device. The phone has no provider keys, so it does not offer a
 * speech server. A browser on patchbayd does: the daemon holds the key and sends audio back.
 */
export function ReadAloudSettings() {
  const prefs = useStore((s) => s.prefs);
  const providers = useStore((s) => s.providers);
  const models = useStore((s) => s.models);
  const update = useStore((s) => s.updatePrefs);
  const setToast = useStore((s) => s.setToast);
  const [voiceError, setVoiceError] = useState<string | null>(null);
  const [voices, setVoices] = useState<SpeechVoice[] | null>(null);
  const set = async <K extends keyof typeof prefs>(k: K, v: (typeof prefs)[K]) => {
    const err = await update({ [k]: v });
    if (err) setToast(err);
  };

  useEffect(() => {
    let gone = false;
    setVoiceError(null);
    systemVoices().then((v) => {
      if (!gone) setVoices(v);
    }).catch((error) => {
      if (!gone) {
        setVoices([]);
        setVoiceError(String(error));
      }
    });
    return () => {
      gone = true;
    };
  }, [prefs.readAloudEngine]);

  const speechProviders = providers.filter((p) => p.kind === "openai_chat");
  const showServer = prefs.readAloudEngine === "server" && !isRemoteApp();

  return (
    <>
      <Row
        label="Reading voice"
        hint={
          isAndroidApp()
            ? "Automatic uses Android’s text-to-speech engine. Manage its default voice and downloaded voice data in Android Settings. Network voices may send text to the engine’s provider."
            : isRemoteApp()
            ? "Speaks a reply when you press Read aloud. Automatic uses this phone's voice when it has one, and a built-in voice otherwise."
            : "Speaks a reply when you press Read aloud. Automatic uses this computer's voice where it has one, and a built-in voice otherwise. Linux uses the built-in voice unless you pick another engine."
        }
      >
        <Choice
          value={prefs.readAloudEngine}
          onChange={(v) => set("readAloudEngine", v)}
          options={[
            { value: "auto", label: "Automatic" },
            { value: "system", label: "System" },
            { value: "builtin", label: "Built-in" },
            ...(!isRemoteApp() ? [{ value: "server" as const, label: "Server" }] : []),
          ]}
        />
      </Row>
      {voiceError && <p role="alert" className="py-2 text-xs text-muted">{voiceError}</p>}
      {prefs.readAloudEngine !== "server" && voices && voices.length > 1 && (
        <Row label="Voice" hint="Voices installed on this device.">
          <select
            className={`${input} w-56`}
            aria-label="Voice"
            value={prefs.readAloudVoice}
            onChange={(e) => set("readAloudVoice", e.target.value)}
          >
            <option value="">Default</option>
            {voices.map((v) => (
              <option key={v.id || v.name} value={v.id}>
                {v.name}
              </option>
            ))}
          </select>
        </Row>
      )}
      {prefs.readAloudEngine === "system" && voices && voices.length === 0 && (
        <p className="py-2 text-xs text-muted">No system voice showed up on this device. Choose Built-in, or install one and come back.</p>
      )}
      <Row label="Reading speed">
        <Choice
          value={prefs.readAloudRate}
          onChange={(v) => set("readAloudRate", v)}
          options={[
            { value: "slower", label: "Slower" },
            { value: "normal", label: "Normal" },
            { value: "faster", label: "Faster" },
          ]}
        />
      </Row>
      <Row label="Listen to new replies" hint="Speak new replies while that chat or session is the one on screen. A chat reads the finished reply. A Code session reads each message as the agent finishes it, so you hear the steps along the way.">
        <Toggle label="Listen to new replies" checked={prefs.readAloudListen} onChange={(v) => set("readAloudListen", v)} />
      </Row>
      {!isRemoteApp() && (
        <Row
          label="Smart speech"
          hint="A model you choose rewrites each reply for the ear before it is read: file names, code and numbers sound natural, and long passages are tightened. The reply text is sent to that model, a few paragraphs per request. If it fails, the reply is read as written. Not used on the phone."
        >
          <select
            className={`${input} w-56`}
            aria-label="Smart speech model"
            value={models.some((m) => m.id === prefs.readAloudRewriteModel) ? prefs.readAloudRewriteModel : ""}
            onChange={(e) => set("readAloudRewriteModel", e.target.value)}
          >
            <option value="">Off</option>
            {models.map((m) => (
              <option key={m.id} value={m.id}>
                {m.displayName}
              </option>
            ))}
          </select>
        </Row>
      )}
      {showServer && (
        <>
          <Row
            label="Speech server"
            hint={
              isWebHint()
                ? "The computer running Patchbay calls this address and the browser plays the audio. localhost is that computer, not the one where this page is open. The reply text is sent there."
                : "An OpenAI-compatible speech endpoint, or a provider you already saved. The reply text is sent there. Ollama and Anthropic chats are not speech servers."
            }
          >
            <select
              className={`${input} w-56`}
              aria-label="Speech provider"
              value={prefs.readAloudServerProviderId}
              onChange={(e) => {
                const id = e.target.value;
                const provider = speechProviders.find((p) => p.id === id);
                void update({
                  readAloudServerProviderId: id,
                  readAloudServerUrl: provider?.baseUrl ?? prefs.readAloudServerUrl,
                }).then((err) => err && setToast(err));
              }}
            >
              <option value="">URL only, no key</option>
              {speechProviders.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
          </Row>
          {!prefs.readAloudServerProviderId && (
            <Row label="Server URL" hint="Base URL, for example http://127.0.0.1:8880/v1">
              <Deferred
                value={prefs.readAloudServerUrl}
                placeholder="https://api.openai.com/v1"
                mono
                width="w-64"
                onCommit={(v) => update({ readAloudServerUrl: v.trim() })}
              />
            </Row>
          )}
          <Row label="Speech model" hint="Empty uses tts-1.">
            <Deferred
              value={prefs.readAloudServerModel}
              placeholder="tts-1"
              mono
              width="w-40"
              onCommit={(v) => update({ readAloudServerModel: v.trim() })}
            />
          </Row>
          <Row label="Server voice" hint="Empty uses alloy.">
            <Deferred
              value={prefs.readAloudServerVoice}
              placeholder="alloy"
              mono
              width="w-40"
              onCommit={(v) => update({ readAloudServerVoice: v.trim() })}
            />
          </Row>
        </>
      )}
      <div className="flex justify-end py-2">
        <button
          type="button"
          className="rounded-md border border-border px-2.5 py-1 text-xs text-muted hover:text-fg"
          onClick={() => {
            primeAudio();
            speakText("This is Patchbay.", "settings-test").catch((e) =>
              setToast(e instanceof Error ? e.message : "Could not read this aloud"),
            );
          }}
        >
          Test voice
        </button>
      </div>
    </>
  );
}

function isWebHint(): boolean {
  return typeof window !== "undefined" && (window as { __PATCHBAY_WEB__?: boolean }).__PATCHBAY_WEB__ === true;
}
