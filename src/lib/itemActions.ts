// What you can do with a chat or Code session from its row: the desktop's right-click menu and
// the phone's hold-to-open sheet share these lists.
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import type { ItemAction } from "../components/ItemMenu";
import { isWeb } from "./platform";
import { copyText } from "../components/CopyButton";
import { api, errorText, type CodeSession, type Conversation } from "./ipc";
import { useStore } from "./store";

/** Run an action, turning a failure into a toast. */
function attempt(f: () => Promise<unknown>): () => void {
  return () => {
    f().catch((e) => useStore.getState().setToast(errorText(e)));
  };
}

/** Rename through a dialog: the phone's way, where rows have no inline editor. */
export async function promptRename(title: string): Promise<string | null> {
  const r = await useStore.getState().askConfirm({
    title: "Rename",
    body: "",
    confirmLabel: "Rename",
    input: { value: title, label: "New name" },
  });
  return r.ok && r.text && r.text !== title ? r.text : null;
}

export function chatActions(
  c: Conversation,
  { rename, desktop }: { rename: () => void; desktop: boolean },
): ItemAction[] {
  const st = useStore.getState;
  const archived = c.archivedAt != null;
  const actions: ItemAction[] = [
    { label: "Rename", onSelect: rename },
    {
      label: c.pinned ? "Unpin" : "Pin",
      onSelect: attempt(async () => {
        await api.updateConversation(c.id, { pinned: !c.pinned });
        await st().refreshConversations();
      }),
    },
    {
      label: archived ? "Restore from archive" : "Archive",
      onSelect: attempt(async () => {
        await api.updateConversation(c.id, { archived: !archived });
        if (!archived && st().activeConversationId === c.id) st().selectConversation(null);
        await st().refreshConversations();
        st().setToast(archived ? `Restored "${c.title}"` : `Archived "${c.title}"`);
      }),
    },
  ];
  // Phones can't delete (docs/design/mobile.md); archiving is the undoable way to clear one out.
  if (desktop) {
    actions.push({
      label: "Delete…",
      danger: true,
      onSelect: attempt(async () => {
        const { ok } = await st().askConfirm({
          title: "Delete conversation?",
          body: `"${c.title}" and all of its messages will be permanently deleted.`,
          confirmLabel: "Delete",
          danger: true,
        });
        if (!ok) return;
        await api.deleteConversation(c.id);
        if (st().activeConversationId === c.id) st().selectConversation(null);
        await st().refreshConversations();
      }),
    });
  }
  return actions;
}

/** Archive a Code session, or restore it if it's archived. */
export function toggleSessionArchive(s: CodeSession): () => void {
  const st = useStore.getState;
  const archived = s.archivedAt != null;
  return attempt(async () => {
    // Archiving stops the agent too, wherever it runs.
    await api.updateCodeSession(s.id, { archived: !archived });
    if (!archived && st().activeCodeSessionId === s.id) st().selectCodeSession(null);
    await st().refreshCodeSessions();
    st().setToast(archived ? `Restored "${s.title}"` : `Archived "${s.title}"`);
  });
}

export function sessionActions(
  s: CodeSession,
  { rename, desktop, running, local }: { rename: () => void; desktop: boolean; running: boolean; local: boolean },
): ItemAction[] {
  const st = useStore.getState;
  const archived = s.archivedAt != null;
  const actions: ItemAction[] = [
    { label: "Rename", onSelect: rename },
    {
      label: "Auto-retitle",
      onSelect: attempt(async () => {
        const title = await api.codeAutoTitle(s.id);
        await st().refreshCodeSessions();
        st().setToast(`Renamed to "${title}"`);
      }),
    },
    {
      label: s.pinned ? "Unpin" : "Pin",
      onSelect: attempt(async () => {
        await api.updateCodeSession(s.id, { pinned: !s.pinned });
        await st().refreshCodeSessions();
      }),
    },
  ];
  if (running) {
    actions.push({
      label: "Stop agent",
      onSelect: attempt(async () => {
        await api.codeStop(s.id);
        st().setToast("Agent stopped. It starts again when you send a message.");
      }),
    });
  }
  actions.push({ label: archived ? "Restore from archive" : "Archive", onSelect: toggleSessionArchive(s) });
  actions.push({ label: "Session info…", onSelect: () => st().setSessionInfoId(s.id) });
  actions.push({
    label: "Copy session ID",
    onSelect: attempt(async () => {
      // The live id (the agent's last `sessionReady`) beats the saved one, which can lag.
      const id = st().code[s.id]?.acpSessionId ?? s.acpSessionId;
      if (!id) {
        st().setToast("No agent session ID yet: the agent hasn't started this session.");
        return;
      }
      if (await copyText(id)) st().setToast("Copied the agent session ID");
    }),
  });
  actions.push({
    label: "Copy folder path",
    onSelect: attempt(async () => {
      if (await copyText(s.cwd)) st().setToast("Copied the folder path");
    }),
  });
  if (desktop && local && !isWeb()) {
    actions.push({ label: "Show in file manager", onSelect: attempt(() => revealItemInDir(s.cwd)) });
  }
  if (desktop) {
    actions.push({
      label: "Remove…",
      danger: true,
      onSelect: attempt(async () => {
        const { ok } = await st().askConfirm({
          title: "Remove session?",
          body: `"${s.title}" will be removed from Patchbay. Files in ${s.cwd} are not touched.`,
          confirmLabel: "Remove",
          danger: true,
        });
        if (!ok) return;
        await api.deleteCodeSession(s.id);
        if (st().activeCodeSessionId === s.id) st().selectCodeSession(null);
        await st().refreshCodeSessions();
      }),
    });
  }
  return actions;
}
