import { create } from "zustand";
import { useStore, type Mode } from "./store";

/** A place the user can go back to: a tab plus the chat or session open in it. */
interface Loc {
  mode: Mode;
  id: string | null;
}

const MAX = 100;

interface HistoryState {
  back: Loc[];
  forward: Loc[];
}

export const useHistory = create<HistoryState>(() => ({ back: [], forward: [] }));

function here(): Loc {
  const s = useStore.getState();
  return { mode: s.mode, id: s.mode === "chat" ? s.activeConversationId : s.activeCodeSessionId };
}

const same = (a: Loc, b: Loc) => a.mode === b.mode && a.id === b.id;

/** A location still exists unless its chat or session was deleted. */
function exists(l: Loc): boolean {
  if (!l.id) return true;
  const s = useStore.getState();
  return l.mode === "chat" ? s.conversations.some((c) => c.id === l.id) : s.codeSessions.some((c) => c.id === l.id);
}

let jumping = false;
let last = here();

// Every selection or tab change becomes a history step, wherever it came from (sidebar, palette, shortcuts).
useStore.subscribe(() => {
  const now = here();
  if (same(now, last)) return;
  if (!jumping) {
    const prev = last;
    useHistory.setState((h) => ({ back: [...h.back, prev].slice(-MAX), forward: [] }));
  }
  last = now;
});

function go(l: Loc) {
  jumping = true;
  try {
    const s = useStore.getState();
    if (l.mode === "chat") s.selectConversation(l.id);
    else s.selectCodeSession(l.id);
    s.setMode(l.mode);
  } finally {
    jumping = false;
  }
}

function step(from: "back" | "forward") {
  const to = from === "back" ? "forward" : "back";
  const h = useHistory.getState();
  const stack = [...h[from]];
  const cur = here();
  let target: Loc | undefined;
  while ((target = stack.pop())) if (exists(target) && !same(target, cur)) break;
  if (!target) {
    useHistory.setState({ [from]: stack } as Partial<HistoryState>);
    return;
  }
  useHistory.setState({ [from]: stack, [to]: [...h[to], cur].slice(-MAX) } as Partial<HistoryState>);
  go(target);
}

export const goBack = () => step("back");
export const goForward = () => step("forward");

/** Forget everything, e.g. after startup restores the last tab. */
export function resetHistory() {
  last = here();
  useHistory.setState({ back: [], forward: [] });
}
