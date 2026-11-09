import { useContext, useEffect, useSyncExternalStore } from "react";
import { BroadcastIcon, SpeakerIcon, StopIcon } from "./icons";
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

/**
 * Turn "listen to new replies" on or off. Off also stops what is being read and anything queued
 * behind it, at once, rather than letting a long reply play out. Returns an error to show.
 */
export async function setListen(on: boolean): Promise<string | null> {
  if (!on) stopSpeech();
  const err = await useStore.getState().updatePrefs({ readAloudListen: on });
  // A reply that finished while the change was on its way may have been queued in the meantime.
  if (!on && !err) stopSpeech();
  return err;
}

/** Composer control for "speak each new reply". The click is what lets the page play audio later. */
export function ListenButton() {
  const on = useStore((s) => s.prefs.readAloudListen);
  const setToast = useStore((s) => s.setToast);
  // One preference for every chat and session, but only the view on screen reads its replies.
  const label = on
    ? "Reading new replies aloud while this view is open. Click to stop"
    : "Read each new reply aloud while this view is open";
  return (
    <button
      type="button"
      className={`flex h-7 w-7 items-center justify-center rounded-md ${on ? "bg-accent/15 text-fg" : "text-muted hover:bg-panel-2 hover:text-fg"}`}
      aria-pressed={on}
      aria-label={label}
      title={label}
      onClick={() => {
        primeAudio();
        void setListen(!on).then((err) => err && setToast(err));
      }}
    >
      {on ? <BroadcastIcon size={14} /> : <SpeakerIcon size={14} />}
    </button>
  );
}
