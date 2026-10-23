import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { isPermissionGranted, requestPermission } from "@tauri-apps/plugin-notification";
import { api, errorText, type CodeEnvelope, type CodeSession, type MeshChatEvent, type MeshMember, type MeshStatus } from "../lib/ipc";
import { codeEventArrived, memberName, memberOnline, useStore } from "../lib/store";
import { ChatView } from "../chat/ChatView";
import { CodeView } from "../code/CodeView";
import { ConfirmDialog } from "../components/ConfirmDialog";
import { SessionInfoDialog } from "../code/SessionInfoDialog";
import { ActionSheet, useHold, type ItemAction } from "../components/ItemMenu";
import { chatActions, promptRename, sessionActions } from "../lib/itemActions";
import { NewSessionSheet } from "./NewSession";
import { ChatNodePicker } from "./ChatNodePicker";
import { fmtShort } from "../lib/format";
import { ChatIcon, ChevronLeftIcon, CodeIcon, MonitorIcon, PlusIcon, SettingsIcon, StarIcon, XIcon } from "../components/icons";
import { Dot } from "../components/Dot";

/** The phone backend's own state (see crates/patchbay-mesh/src/client.rs, `status`). */
export interface RemoteStatus {
  nodeId: string;
  name: string;
  member: boolean;
  meshId: string | null;
  phase: "offline" | "connecting" | "online";
  error: string | null;
  gateway: string | null;
  gatewayName: string | null;
  gatewayUrl: string | null;
  rttMs: number | null;
  resumed: boolean | null;
  joining: { url: string; code: string | null; status: "connecting" | "waiting" | "failed" | "rejected"; error: string | null } | null;
  members: MeshMember[];
  /** Nodes waiting to join, on any computer; this phone may decide them. */
  pending: MeshStatus["pending"];
}

/** What the user chose on a join request's notification (see remote/src-tauri, `on_notification_action`). */
interface JoinAction {
  action: "open" | "approve" | "reject";
  nodeId: string;
}

/** What a gateway knows a Code session is doing. */
interface Live {
  status: string;
  busy: boolean;
  permissions: string[];
  last: number;
}

const remote = {
  status: () => invoke<RemoteStatus>("remote_status"),
  join: (url: string, passphrase: string, name: string) => invoke<void>("remote_join", { url, passphrase, name }),
  cancelJoin: () => invoke<void>("remote_cancel_join"),
  localNetwork: () => invoke<boolean>("remote_local_network"),
  leave: () => invoke<void>("remote_leave"),
  kick: () => invoke<void>("remote_kick"),
  setForeground: (foreground: boolean) => invoke<void>("remote_set_foreground", { foreground }),
  unwatch: (topic: string) => invoke<void>("remote_unwatch", { topic }),
  keepAlive: (on: boolean) => invoke<void>("remote_keep_alive", { on }),
  unlock: (reason: string) => invoke<boolean>("remote_unlock", { reason }),
  live: () => invoke<Record<string, Live>>("remote_live"),
  takeJoinAction: () => invoke<JoinAction | null>("remote_take_join_action"),
};

// Per-device conveniences, so plain local storage.
const LOCK_KEY = "patchbay.remote.lock";
const STAY_KEY = "patchbay.remote.stayConnected";
const TAB_KEY = "patchbay.remote.tab";
/** Lock again after the app has been out of sight this long. */
const RELOCK_MS = 5 * 60_000;

function flag(key: string, dflt: boolean): boolean {
  try {
    const v = localStorage.getItem(key);
    return v === null ? dflt : v === "1";
  } catch {
    return dflt;
  }
}

function setFlag(key: string, on: boolean) {
  try {
    localStorage.setItem(key, on ? "1" : "0");
  } catch {
    /* storage unavailable */
  }
}

const inputCls =
  "h-11 w-full rounded-[10px] border border-border bg-raised px-3 text-[15px] text-fg outline-none placeholder:text-faint focus:border-accent";
const primaryCls =
  "flex h-11 w-full items-center justify-center rounded-[10px] bg-accent px-4 text-[15px] font-semibold text-accent-fg active:brightness-95 disabled:opacity-50";
const ghostCls = "flex h-11 w-full items-center justify-center rounded-[10px] px-4 text-[15px] text-muted active:bg-panel-2";

export function RemoteApp() {
  const [status, setStatus] = useState<RemoteStatus | null>(null);
  const [locked, setLocked] = useState(() => flag(LOCK_KEY, false));
  // A tap on a join request's notification, or on its Approve or Decline button. It waits here
  // through the lock screen, so a button in the shade can't get around the lock.
  const [joinAction, setJoinAction] = useState<JoinAction | null>(null);
  const joinActionDone = useCallback(() => setJoinAction(null), []);
  useEffect(() => {
    remote.status().then(setStatus, () => {});
    const un = listen<RemoteStatus>("remote://status", (ev) => setStatus(ev.payload));
    // The app may have been started by the notification, before this window listened.
    const take = () => remote.takeJoinAction().then((a) => a && setJoinAction(a), () => {});
    take();
    const act = listen("remote://join-action", take);
    return () => {
      un.then((f) => f());
      act.then((f) => f());
    };
  }, []);

  if (!status) return <div className="h-full bg-bg" />;
  if (!status.member) return <JoinScreen status={status} />;
  if (locked) return <LockScreen onUnlocked={() => setLocked(false)} />;
  return <Main status={status} onLock={() => setLocked(true)} joinAction={joinAction} onJoinActionDone={joinActionDone} />;
}

function Screen({ children }: { children: ReactNode }) {
  return (
    <div className="flex h-full flex-col items-stretch justify-center overflow-y-auto bg-bg px-6 py-10">
      <div className="mx-auto flex w-full max-w-sm flex-col gap-4">{children}</div>
    </div>
  );
}

function Brand({ line }: { line: string }) {
  return (
    <div className="mb-2 flex flex-col gap-1.5">
      <div className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">Patchbay Remote</div>
      <div className="text-[26px] font-medium leading-tight tracking-[-0.02em]">{line}</div>
    </div>
  );
}

function JoinScreen({ status }: { status: RemoteStatus }) {
  const [url, setUrl] = useState("");
  const [pass, setPass] = useState("");
  const [name, setName] = useState(status.name || "Phone");
  const [err, setErr] = useState<string | null>(null);
  const [lanDenied, setLanDenied] = useState(false);
  const j = status.joining;
  const busy = j?.status === "connecting" || j?.status === "waiting";

  if (j?.status === "waiting" && j.code) {
    return (
      <Screen>
        <Brand line="Approve this phone" />
        <p className="text-muted">
          Any computer in the mesh can approve it. Open Patchbay on one, go to Settings → Mesh, or approve it from another
          phone in the mesh, and check it shows this code before you approve:
        </p>
        <div className="rounded-xl border border-border bg-panel py-5 text-center font-mono text-[34px] tracking-[0.12em]">
          {j.code}
        </div>
        <button className={ghostCls} onClick={() => remote.cancelJoin()}>
          Cancel
        </button>
      </Screen>
    );
  }

  return (
    <Screen>
      <Brand line="Join your mesh" />
      <p className="text-muted">
        Connect to any computer in your Patchbay mesh: on your home network, or through ZeroTier, Tailscale or a VPN.
      </p>
      <label className="flex flex-col gap-1.5 text-[13px] text-muted">
        Computer's address
        <input
          className={inputCls}
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          placeholder="192.168.1.20 or studio.local:41890"
          autoCapitalize="off"
          autoCorrect="off"
          inputMode="url"
        />
      </label>
      <label className="flex flex-col gap-1.5 text-[13px] text-muted">
        Mesh passphrase
        <input className={inputCls} value={pass} onChange={(e) => setPass(e.target.value)} type="password" autoComplete="off" />
      </label>
      <label className="flex flex-col gap-1.5 text-[13px] text-muted">
        Name for this phone
        <input className={inputCls} value={name} onChange={(e) => setName(e.target.value)} maxLength={64} />
      </label>
      {(err || j?.error) && (
        <div className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[13px] text-danger">{err ?? j?.error}</div>
      )}
      {lanDenied && (
        <p className="text-[13px] text-muted">
          Local network access is off, so only ZeroTier, Tailscale or VPN addresses can connect. Turn it on in Android
          Settings → Apps → Patchbay Remote → Permissions → Nearby devices.
        </p>
      )}
      <button
        className={primaryCls}
        disabled={busy || !url.trim() || !pass}
        onClick={async () => {
          setErr(null);
          // Android 17 blocks home-network addresses until the user allows it; VPN addresses still work.
          const lan = await remote.localNetwork().catch(() => true);
          setLanDenied(!lan);
          remote.join(url, pass, name).catch((e) => setErr(errorText(e)));
        }}
      >
        {busy ? "Connecting…" : "Join"}
      </button>
      {busy && (
        <button className={ghostCls} onClick={() => remote.cancelJoin()}>
          Cancel
        </button>
      )}
    </Screen>
  );
}

function LockScreen({ onUnlocked }: { onUnlocked: () => void }) {
  const [failed, setFailed] = useState(false);
  const tryUnlock = () => {
    setFailed(false);
    remote.unlock("Unlock Patchbay Remote").then((ok) => (ok ? onUnlocked() : setFailed(true)), () => setFailed(true));
  };
  // Ask right away; the button is for retrying.
  useEffect(tryUnlock, []); // eslint-disable-line react-hooks/exhaustive-deps
  return (
    <Screen>
      <Brand line="Locked" />
      <p className="text-muted">This phone can run commands on your computers, so it asks who you are first.</p>
      {failed && <div className="text-[13px] text-danger">That didn't work. Try again.</div>}
      <button className={primaryCls} onClick={tryUnlock}>
        Unlock
      </button>
    </Screen>
  );
}

type View = { kind: "home" } | { kind: "chat" } | { kind: "code" };

function Main({
  status,
  onLock,
  joinAction,
  onJoinActionDone,
}: {
  status: RemoteStatus;
  onLock: () => void;
  joinAction: JoinAction | null;
  onJoinActionDone: () => void;
}) {
  const [view, setView] = useState<View>({ kind: "home" });
  const [tab, setTab] = useState<"chats" | "code">(() => {
    try {
      return localStorage.getItem(TAB_KEY) === "code" ? "code" : "chats";
    } catch {
      return "chats";
    }
  });
  const [live, setLive] = useState<Record<string, Live>>({});
  const [settings, setSettings] = useState(false);
  const [newSession, setNewSession] = useState(false);
  const toast = useStore((s) => s.toast);
  const setToast = useStore((s) => s.setToast);
  const hiddenAt = useRef<number | null>(null);
  const liveRef = useRef(live);
  liveRef.current = live;

  // A join request's notification: tapping it opens the membership screen with the code; its
  // buttons decide at once. After a cold start the request may not have reached us yet.
  useEffect(() => {
    if (!joinAction) return;
    const { action, nodeId } = joinAction;
    if (action === "open") {
      setSettings(true);
      onJoinActionDone();
      return;
    }
    const req = status.pending.find((p) => p.nodeId === nodeId);
    if (req) {
      onJoinActionDone();
      const done = action === "approve" ? api.meshApprove(nodeId) : api.meshReject(nodeId);
      done.then(
        () => setToast(`${action === "approve" ? "Approved" : "Declined"} ${req.name}`),
        (e) => setToast(errorText(e)),
      );
      return;
    }
    const t = setTimeout(() => {
      setToast("That join request is gone");
      onJoinActionDone();
    }, 6000);
    return () => clearTimeout(t);
  }, [joinAction, status.pending, onJoinActionDone, setToast]);

  // Load what the gateway knows, and follow its changes.
  useEffect(() => {
    const st = useStore.getState();
    st.loadPrefs().catch(() => {});
    st.refreshMesh();
    // Android 17 asks before an app may reach the home network, which reconnecting to a LAN
    // gateway needs; Android 13 and later ask before an app may notify, which the backend does when
    // out of sight. One prompt at a time: Android cancels a request made while another shows.
    remote
      .localNetwork()
      .catch(() => {})
      .then(() => isPermissionGranted())
      .then(async (ok) => {
        if (!ok) await requestPermission();
      })
      .catch(() => {});
    const refresh = () => {
      const s = useStore.getState();
      Promise.all([s.refreshCatalog(), s.refreshConversations(), s.refreshCodeSessions()]).catch((e) => s.setToast(errorText(e)));
      remote.live().then(setLive, () => {});
    };
    refresh();
    const unlisteners = [
      listen<CodeEnvelope>("code://event", (ev) => codeEventArrived(ev.payload)),
      listen<MeshChatEvent>("mesh://chat", (ev) => useStore.getState().applyMeshChat(ev.payload)),
      listen<{ tables: string[] }>("mesh://changed", (ev) => {
        useStore.getState().onSynced(ev.payload.tables);
        remote.live().then(setLive, () => {});
      }),
      listen("remote://status", () => useStore.getState().refreshMesh()),
      listen<{ id: string; live: Live }>("remote://live", (ev) => setLive((m) => ({ ...m, [ev.payload.id]: ev.payload.live }))),
      // After a reconnect the gateway no longer had what this phone missed: start that view over.
      listen<{ topic: string; snapshot?: { stream?: MeshChatEvent["event"] & Record<string, unknown> } }>("remote://resync", (ev) => {
        const { topic } = ev.payload;
        const s = useStore.getState();
        if (topic === "index") refresh();
        else if (topic.startsWith("session:")) {
          const id = topic.slice("session:".length);
          if (s.code[id]) s.attachAgent(id);
        } else if (topic.startsWith("chat:")) s.onSynced(["messages"]);
      }),
    ];
    return () => {
      for (const u of unlisteners) u.then((f) => f());
    };
  }, []);

  // Foreground and background: ping rarely when hidden, keep the connection if something runs.
  useEffect(() => {
    const onVis = () => {
      const visible = document.visibilityState === "visible";
      remote.setForeground(visible).catch(() => {});
      if (!visible) {
        hiddenAt.current = Date.now();
        const st = useStore.getState();
        const running =
          Object.values(st.code).some((c) => c.busy || c.permissions.length > 0) ||
          Object.values(liveRef.current).some((l) => l.busy || l.permissions.length > 0) ||
          Object.values(st.chatStreams).some((c) => c.running);
        if (running && flag(STAY_KEY, true)) remote.keepAlive(true).catch(() => {});
        return;
      }
      remote.keepAlive(false).catch(() => {});
      remote.kick().catch(() => {});
      if (flag(LOCK_KEY, false) && hiddenAt.current && Date.now() - hiddenAt.current > RELOCK_MS) onLock();
      hiddenAt.current = null;
    };
    document.addEventListener("visibilitychange", onVis);
    return () => document.removeEventListener("visibilitychange", onVis);
  }, [onLock]);

  // The back button and the system back gesture both return to the list. Leaving a Code session
  // stops following it, so its state here goes too: opening it again catches up from the host
  // instead of showing what it was doing when we left.
  useEffect(() => {
    const onPop = () => {
      const s = useStore.getState();
      const id = s.activeCodeSessionId;
      if (id) {
        remote.unwatch(`session:${id}`).catch(() => {});
        useStore.setState((st) => {
          const { [id]: _, ...code } = st.code;
          return { code };
        });
      }
      setView({ kind: "home" });
    };
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);

  useEffect(() => {
    if (!toast) return;
    const t = setTimeout(() => setToast(null), 5000);
    return () => clearTimeout(t);
  }, [toast, setToast]);

  const open = (v: View) => {
    history.pushState({ v: v.kind }, "");
    setView(v);
  };
  const back = () => history.back();
  const backButton = (
    <button className="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg text-muted active:bg-panel-2" aria-label="Back" onClick={back}>
      <ChevronLeftIcon size={20} />
    </button>
  );

  return (
    <div className="flex h-full flex-col bg-bg">
      {view.kind !== "home" && (
        <div className="flex min-h-0 flex-1 flex-col pb-[env(safe-area-inset-bottom)] pt-[env(safe-area-inset-top)]">
          {view.kind === "chat" ? (
            <ChatView
              leading={backButton}
              controls={<ChatNodePicker members={status.members} gateway={status.gateway} />}
            />
          ) : (
            <CodeView leading={backButton} />
          )}
        </div>
      )}
      {view.kind === "home" && (
        <Home
          status={status}
          tab={tab}
          setTab={(t) => {
            setTab(t);
            try {
              localStorage.setItem(TAB_KEY, t);
            } catch {
              /* storage unavailable */
            }
          }}
          live={live}
          onOpenChat={(id) => {
            useStore.getState().selectConversation(id);
            open({ kind: "chat" });
          }}
          onOpenCode={(id) => {
            useStore.getState().selectCodeSession(id);
            open({ kind: "code" });
          }}
          onNewSession={() => setNewSession(true)}
          onSettings={() => setSettings(true)}
        />
      )}
      {view.kind !== "home" && status.phase !== "online" && <ConnectionBar status={status} />}
      {settings && <SettingsSheet status={status} onClose={() => setSettings(false)} />}
      {newSession && (
        <NewSessionSheet
          members={status.members}
          gateway={status.gateway}
          onClose={() => setNewSession(false)}
          onCreated={(id) => {
            setNewSession(false);
            useStore.getState().selectCodeSession(id);
            open({ kind: "code" });
          }}
        />
      )}
      <ConfirmDialog />
      <SessionInfoDialog />
      {toast && (
        <div className="pointer-events-none fixed inset-x-4 bottom-[calc(env(safe-area-inset-bottom)+16px)] z-50 flex justify-center">
          <div className="pointer-events-auto max-w-md rounded-xl border border-border bg-raised px-4 py-2.5 text-[14px] shadow-float">
            {toast}
          </div>
        </div>
      )}
    </div>
  );
}

function ConnectionPill({ status }: { status: RemoteStatus }) {
  const online = status.phase === "online";
  return (
    <button
      className="flex min-w-0 items-center gap-1.5 rounded-full border border-line px-2.5 py-1 text-[12px] text-muted active:bg-panel-2"
      onClick={() => remote.kick()}
      title={status.error ?? undefined}
    >
      <Dot size={6} className={online ? "text-ok" : "signal-pulse text-accent"} />
      <span className="truncate">{online ? `via ${status.gatewayName}` : "Reconnecting…"}</span>
    </button>
  );
}

function ConnectionBar({ status }: { status: RemoteStatus }) {
  return (
    <div className="flex items-center gap-2 border-t border-line bg-panel px-4 py-2 pb-[calc(env(safe-area-inset-bottom)+8px)] text-[13px] text-muted">
      <Dot size={6} className="signal-pulse text-accent" />
      <span className="min-w-0 flex-1 truncate">Reconnecting… {status.error ? `(${status.error})` : ""}</span>
      <button className="shrink-0 rounded-md px-2 py-1 text-fg active:bg-panel-2" onClick={() => remote.kick()}>
        Retry
      </button>
    </div>
  );
}

function Home({
  status,
  tab,
  setTab,
  live,
  onOpenChat,
  onOpenCode,
  onNewSession,
  onSettings,
}: {
  status: RemoteStatus;
  tab: "chats" | "code";
  setTab: (t: "chats" | "code") => void;
  live: Record<string, Live>;
  onOpenChat: (id: string | null) => void;
  onOpenCode: (id: string) => void;
  onNewSession: () => void;
  onSettings: () => void;
}) {
  const conversations = useStore((s) => s.conversations);
  const sessions = useStore((s) => s.codeSessions);
  const streams = useStore((s) => s.chatStreams);
  const mesh = useStore((s) => s.mesh);
  const needsYou = sessions.filter((s) => (live[s.id]?.permissions.length ?? 0) > 0).length;

  const [sheet, setSheet] = useState<{ title: string; actions: ItemAction[] } | null>(null);
  const [showArchived, setShowArchived] = useState(false);
  const pinnedFirst = <T extends { pinned: boolean; updatedAt: number }>(a: T, b: T) =>
    Number(b.pinned) - Number(a.pinned) || b.updatedAt - a.updatedAt;

  const chats = useMemo(() => conversations.filter((c) => c.archivedAt == null).sort(pinnedFirst), [conversations]);
  const archivedChats = useMemo(() => conversations.filter((c) => c.archivedAt != null), [conversations]);
  const archivedSessions = useMemo(() => sessions.filter((s) => s.archivedAt != null), [sessions]);
  const byHost = useMemo(() => {
    const groups = new Map<string, CodeSession[]>();
    for (const s of sessions.filter((x) => x.archivedAt == null).sort(pinnedFirst)) {
      const h = s.hostNode ?? "";
      groups.set(h, [...(groups.get(h) ?? []), s]);
    }
    // Hosts that are online first.
    return [...groups.entries()].sort(([a], [b]) => Number(memberOnline(mesh, b)) - Number(memberOnline(mesh, a)));
  }, [sessions, mesh]);

  // Opening an archived chat or session brings it back.
  const restoreThen = async (archived: boolean, restore: () => Promise<unknown>, open: () => void) => {
    try {
      if (archived) await restore();
      open();
    } catch (e) {
      useStore.getState().setToast(errorText(e));
    }
  };

  const chatRow = (c: (typeof conversations)[number]) => (
    <Row
      key={c.id}
      title={c.title}
      sub={fmtShort(c.updatedAt)}
      leading={c.pinned ? <StarIcon size={13} /> : null}
      trailing={streams[c.id]?.running ? <Dot className="signal-pulse text-live" role="img" aria-label="Replying" /> : null}
      onClick={() =>
        restoreThen(
          c.archivedAt != null,
          async () => {
            await api.updateConversation(c.id, { archived: false });
            await useStore.getState().refreshConversations();
          },
          () => onOpenChat(c.id),
        )
      }
      onHold={() =>
        setSheet({
          title: c.title,
          actions: chatActions(c, {
            desktop: false,
            rename: async () => {
              const t = await promptRename(c.title);
              if (!t) return;
              await api.updateConversation(c.id, { title: t });
              await useStore.getState().refreshConversations();
            },
          }),
        })
      }
    />
  );

  const sessionRow = (s: CodeSession) => {
    const l = live[s.id];
    return (
      <Row
        key={s.id}
        title={s.title}
        sub={`${s.cwd.split(/[\\/]/).filter(Boolean).pop() ?? s.cwd} · ${fmtShort(s.updatedAt)}`}
        leading={s.pinned ? <StarIcon size={13} /> : null}
        trailing={<SessionState live={l} online={memberOnline(mesh, s.hostNode ?? "")} />}
        onClick={() =>
          restoreThen(
            s.archivedAt != null,
            async () => {
              await api.updateCodeSession(s.id, { archived: false });
              await useStore.getState().refreshCodeSessions();
            },
            () => onOpenCode(s.id),
          )
        }
        onHold={() =>
          setSheet({
            title: s.title,
            actions: sessionActions(s, {
              desktop: false,
              local: false,
              running: !!l && (l.busy || l.status === "ready" || l.status === "starting"),
              rename: async () => {
                const t = await promptRename(s.title);
                if (!t) return;
                await api.updateCodeSession(s.id, { title: t });
                await useStore.getState().refreshCodeSessions();
              },
            }),
          })
        }
      />
    );
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col pt-[env(safe-area-inset-top)]">
      {sheet && <ActionSheet title={sheet.title} actions={sheet.actions} onClose={() => setSheet(null)} />}
      <header className="flex h-14 shrink-0 items-center gap-2 px-4">
        <div className="text-[18px] font-semibold tracking-[-0.01em]">Patchbay</div>
        <div className="flex-1" />
        <ConnectionPill status={status} />
        <button className="flex h-10 w-10 items-center justify-center rounded-lg text-muted active:bg-panel-2" aria-label="Settings" onClick={onSettings}>
          <SettingsIcon size={18} />
        </button>
      </header>
      {status.pending.length > 0 && (
        <button
          className="mx-4 mb-2 flex items-center gap-2 rounded-xl border border-accent/50 bg-accent/10 px-3 py-2.5 text-left text-[14px] active:bg-accent/20"
          onClick={onSettings}
        >
          <span className="min-w-0 flex-1 truncate">
            {status.pending[0].name} asks to join{status.pending.length > 1 ? ` (+${status.pending.length - 1} more)` : ""}
          </span>
          <span className="shrink-0 font-semibold text-accent-ink">Review</span>
        </button>
      )}
      <div className="px-4 pb-2">
        <div role="tablist" className="grid grid-cols-2 gap-1 rounded-[11px] border border-line bg-panel p-1">
          {(["chats", "code"] as const).map((t) => (
            <button
              key={t}
              role="tab"
              aria-selected={tab === t}
              className={`flex h-9 items-center justify-center gap-2 rounded-[8px] text-[14px] ${tab === t ? "bg-raised text-fg shadow-raised" : "text-muted"}`}
              onClick={() => setTab(t)}
            >
              {t === "chats" ? <ChatIcon size={15} /> : <CodeIcon size={15} />}
              {t === "chats" ? "Chats" : "Code"}
              {t === "code" && needsYou > 0 && (
                <span className="rounded-full bg-accent px-1.5 text-[11px] font-semibold leading-[18px] text-accent-fg">{needsYou}</span>
              )}
            </button>
          ))}
        </div>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto pb-[calc(env(safe-area-inset-bottom)+88px)]">
        {tab === "chats" ? (
          chats.length === 0 && archivedChats.length === 0 ? (
            <Empty line="No chats yet" sub="Start one: the reply runs on the computer you're connected through." />
          ) : (
            <>
              {chats.map((c) => chatRow(c))}
              <ArchivedRow count={archivedChats.length} open={showArchived} onToggle={() => setShowArchived(!showArchived)} />
              {showArchived && archivedChats.map((c) => chatRow(c))}
            </>
          )
        ) : byHost.length === 0 && archivedSessions.length === 0 ? (
          <Empty line="No Code sessions" sub="Start one here or on a computer; it runs on that computer, and you follow and drive it here." />
        ) : (
          byHost.map(([host, list]) => (
            <section key={host}>
              <div className="flex items-center gap-2 px-4 pb-1.5 pt-4 font-mono text-[11px] uppercase tracking-[0.06em] text-faint">
                <MonitorIcon size={12} />
                {memberName(mesh, host) || "unknown computer"}
                {!memberOnline(mesh, host) && <span className="normal-case tracking-normal">· offline</span>}
              </div>
              {list.map((s) => sessionRow(s))}
            </section>
          ))
        )}
        {tab === "code" && (
          <>
            <ArchivedRow count={archivedSessions.length} open={showArchived} onToggle={() => setShowArchived(!showArchived)} />
            {showArchived && archivedSessions.map((s) => sessionRow(s))}
          </>
        )}
      </div>
      <button
        className="fixed bottom-[calc(env(safe-area-inset-bottom)+20px)] right-5 flex h-14 items-center gap-2 rounded-2xl bg-accent px-5 text-[15px] font-semibold text-accent-fg shadow-float active:brightness-95"
        onClick={() => (tab === "chats" ? onOpenChat(null) : onNewSession())}
      >
        <PlusIcon size={18} />
        {tab === "chats" ? "New chat" : "New session"}
      </button>
    </div>
  );
}

function SessionState({ live, online }: { live?: Live; online: boolean }) {
  if (!online) return <span className="text-[12px] text-faint">Offline</span>;
  if (live && live.permissions.length > 0)
    return <span className="rounded-full bg-accent px-2 py-0.5 text-[12px] font-semibold text-accent-fg">Needs you</span>;
  if (live?.busy)
    return (
      <span className="flex items-center gap-1.5 text-[12px] text-live">
        <Dot size={6} className="signal-pulse text-live" />
        Working
      </span>
    );
  if (live?.status === "ready") return <span className="text-[12px] text-ok">Ready</span>;
  return null;
}

/** The collapsed "Archived" row at the end of a list. */
function ArchivedRow({ count, open, onToggle }: { count: number; open: boolean; onToggle: () => void }) {
  if (count === 0) return null;
  return (
    <button
      className="flex w-full items-center gap-2 px-4 pb-1.5 pt-5 text-left font-mono text-[11px] uppercase tracking-[0.06em] text-faint"
      aria-expanded={open}
      onClick={onToggle}
    >
      <span className={`inline-block transition-transform ${open ? "rotate-90" : ""}`}>›</span>
      Archived · {count}
    </button>
  );
}

function Row({
  title,
  sub,
  leading,
  trailing,
  onClick,
  onHold,
}: {
  title: string;
  sub?: string;
  leading?: ReactNode;
  trailing?: ReactNode;
  onClick: () => void;
  /** Hold the row (or right-click it) for more. */
  onHold?: () => void;
}) {
  const hold = useHold(onHold ?? (() => {}));
  return (
    <button
      className="flex w-full select-none items-center gap-3 px-4 py-3 text-left [-webkit-touch-callout:none] active:bg-panel-2"
      onClick={onClick}
      {...(onHold ? hold : {})}
    >
      {leading && <span className="shrink-0 text-accent-ink">{leading}</span>}
      <span className="flex min-w-0 flex-1 flex-col gap-0.5">
        <span className="truncate text-[15px]">{title}</span>
        {sub && <span className="truncate text-[12.5px] text-faint">{sub}</span>}
      </span>
      {trailing && <span className="shrink-0">{trailing}</span>}
    </button>
  );
}

function Empty({ line, sub }: { line: string; sub: string }) {
  return (
    <div className="flex flex-col items-center gap-1.5 px-8 pt-16 text-center">
      <div className="text-[16px] text-fg">{line}</div>
      <div className="text-[13px] text-muted">{sub}</div>
    </div>
  );
}

function Toggle({ label, sub, on, onChange }: { label: string; sub: string; on: boolean; onChange: (on: boolean) => void }) {
  return (
    <label className="flex items-center gap-3 py-2">
      <span className="flex min-w-0 flex-1 flex-col gap-0.5">
        <span className="text-[15px]">{label}</span>
        <span className="text-[12.5px] text-muted">{sub}</span>
      </span>
      <input type="checkbox" className="h-5 w-5 accent-[var(--color-accent)]" checked={on} onChange={(e) => onChange(e.target.checked)} />
    </label>
  );
}

function JoinRequests({ pending }: { pending: RemoteStatus["pending"] }) {
  const setToast = useStore((s) => s.setToast);
  const [busy, setBusy] = useState<string | null>(null);
  const decide = async (id: string, approve: boolean) => {
    setBusy(id);
    try {
      await (approve ? api.meshApprove(id) : api.meshReject(id));
    } catch (e) {
      setToast(errorText(e));
    } finally {
      setBusy(null);
    }
  };
  if (pending.length === 0) return null;
  return (
    <div className="mb-6 flex flex-col gap-3">
      <div className="font-mono text-[11px] uppercase tracking-[0.06em] text-faint">Join requests</div>
      {pending.map((p) => (
        <section key={p.nodeId} className="flex flex-col gap-3 rounded-xl border border-accent/50 bg-accent/5 p-4" aria-label={`Join request from ${p.name}`}>
          <div>
            <div className="text-[15px] font-medium">
              {p.name} asks to join{p.role === "remote" ? " as a phone" : ""}
            </div>
            <div className="mt-0.5 text-[12.5px] leading-5 text-muted">
              Approve only if {p.name} shows this same code.{" "}
              {p.role === "remote"
                ? "It can read every chat and session and run commands through their agents, but gets no API keys."
                : "It will get all chats, sessions and API keys."}
              {p.hostName && <span className="text-faint"> It is connected to {p.hostName}.</span>}
            </div>
          </div>
          <div className="rounded-xl border border-border bg-panel py-3 text-center font-mono text-[28px] tracking-[0.12em]">{p.code}</div>
          <div className="grid grid-cols-2 gap-2">
            <button className={ghostCls + " border border-border"} disabled={busy === p.nodeId} onClick={() => decide(p.nodeId, false)}>
              Decline
            </button>
            <button className={primaryCls} disabled={busy === p.nodeId} onClick={() => decide(p.nodeId, true)}>
              Approve
            </button>
          </div>
        </section>
      ))}
    </div>
  );
}

function SettingsSheet({ status, onClose }: { status: RemoteStatus; onClose: () => void }) {
  const [lock, setLock] = useState(() => flag(LOCK_KEY, false));
  const [stay, setStay] = useState(() => flag(STAY_KEY, true));
  const askConfirm = useStore((s) => s.askConfirm);
  const computers = status.members.filter((m) => m.role !== "remote");
  return (
    <div className="fixed inset-0 z-40 flex flex-col bg-bg pt-[env(safe-area-inset-top)]">
      <header className="flex h-14 shrink-0 items-center gap-2 px-4">
        <div className="flex-1 text-[18px] font-semibold">Settings</div>
        <button className="flex h-10 w-10 items-center justify-center rounded-lg text-muted active:bg-panel-2" aria-label="Close" onClick={onClose}>
          <XIcon size={18} />
        </button>
      </header>
      <div className="min-h-0 flex-1 overflow-y-auto px-4 pb-[calc(env(safe-area-inset-bottom)+24px)]">
        <JoinRequests pending={status.pending} />
        <div className="font-mono text-[11px] uppercase tracking-[0.06em] text-faint">This phone</div>
        <div className="mt-2 rounded-xl border border-line bg-panel px-4 py-3">
          <div className="text-[15px]">{status.name}</div>
          <div className="text-[12.5px] text-muted">
            {status.phase === "online"
              ? `Connected through ${status.gatewayName}${status.rttMs != null ? ` · ${status.rttMs} ms` : ""}`
              : `Reconnecting${status.error ? `: ${status.error}` : "…"}`}
          </div>
        </div>
        <div className="mt-2 flex flex-col divide-y divide-line">
          <Toggle
            label="Lock with fingerprint or screen lock"
            sub="Asks when the app opens, and after five minutes away"
            on={lock}
            onChange={(on) => {
              setLock(on);
              setFlag(LOCK_KEY, on);
            }}
          />
          <Toggle
            label="Stay connected in the background"
            sub="While an agent or a reply runs, so approvals and finished turns notify you"
            on={stay}
            onChange={(on) => {
              setStay(on);
              setFlag(STAY_KEY, on);
            }}
          />
        </div>
        <div className="mt-6 font-mono text-[11px] uppercase tracking-[0.06em] text-faint">Computers</div>
        <div className="mt-2 flex flex-col">
          {computers.map((m) => (
            <div key={m.nodeId} className="flex items-center gap-3 py-2.5">
              <Dot className={m.state === "direct" ? "text-ok" : m.state === "relay" ? "text-live" : "text-faint/50"} />
              <span className="min-w-0 flex-1 truncate text-[15px]">{m.name}</span>
              <span className="text-[12.5px] text-muted">
                {m.nodeId === status.gateway ? "gateway" : m.state === "relay" ? "online" : "offline"}
              </span>
            </div>
          ))}
        </div>
        <button
          className="mt-8 h-11 w-full rounded-[10px] border border-danger/40 text-[15px] text-danger active:bg-danger/10"
          onClick={async () => {
            const { ok } = await askConfirm({
              title: "Leave the mesh?",
              body: "This phone forgets the mesh. To join again you'll need the passphrase and an approval. If no computer is reachable right now, remove it on a computer too, so it drops off their lists.",
              confirmLabel: "Leave",
              danger: true,
            });
            if (ok) {
              await remote.leave();
              onClose();
            }
          }}
        >
          Leave the mesh
        </button>
      </div>
    </div>
  );
}
