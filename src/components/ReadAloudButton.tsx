import { useContext, useEffect, useSyncExternalStore } from "react";
import { SpeakerIcon, StopIcon } from "./icons";
import { primeAudio, speakText, speakingNow, stopSpeech, subscribeSpeaking } from "../lib/speech";
import { ViewActive } from "../lib/shortcuts";
import { useStore } from "../lib/store";

export function useSpeakingId(): string | null {
  return useSyncExternalStore(subscribeSpeaking, speakingNow, speakingNow);
}

/** Stop playback when this chat or Code view is no longer the one on screen. */
export function useStopSpeechWhenHidden() {
  const active = useContext(ViewActive);
  useEffect(() => {
    if (!active) stopSpeech();
  }, [active]);
}

export function ReadAloudButton({ id, text }: { id: string; text: string }) {
  const speaking = useSpeakingId() === id;
  const setToast = useStore((s) => s.setToast);
  return (
    <button
      type="button"
      className="flex h-7 w-7 items-center justify-center rounded-md hover:bg-panel-2 hover:text-fg"
      onClick={() => {
        primeAudio();
        if (speaking) {
          stopSpeech();
          return;
        }
        speakText(text, id).catch((e) => setToast(e instanceof Error ? e.message : "Could not read this aloud"));
      }}
      title={speaking ? "Stop reading" : "Read aloud"}
      aria-label={speaking ? "Stop reading" : "Read aloud"}
    >
      {speaking ? <StopIcon size={14} /> : <SpeakerIcon size={14} />}
    </button>
  );
}

/** Composer control for "speak each new reply". The click is what lets the page play audio later. */
export function ListenButton() {
  const on = useStore((s) => s.prefs.readAloudListen);
  const update = useStore((s) => s.updatePrefs);
  const setToast = useStore((s) => s.setToast);
  return (
    <button
      type="button"
      className={`rounded-md px-2 py-1 text-xs ${on ? "bg-accent/15 text-fg" : "text-muted hover:text-fg"}`}
      aria-pressed={on}
      title="Read each new reply in this window aloud"
      onClick={() => {
        primeAudio();
        void update({ readAloudListen: !on }).then((err) => err && setToast(err));
      }}
    >
      {on ? "Listening" : "Listen"}
    </button>
  );
}
