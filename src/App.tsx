import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { listen } from "@tauri-apps/api/event";
import { api, errorText, type CodeEnvelope, type Conversation, type MeshChatEvent } from "./lib/ipc";
import { codeEventArrived, useStore, type Mode } from "./lib/store";
import { goBack, goForward, resetHistory, useHistory } from "./lib/history";
import { initZoom, zoomIn, zoomOut, zoomReset } from "./lib/zoom";
import { ChatView } from "./chat/ChatView";
import { CodeView, newSessionIn, openFolderAsSession } from "./code/CodeView";
import { Settings } from "./settings/Settings";
import { CodeSidebar } from "./code/CodeSidebar";
import { ConfirmDialog } from "./components/ConfirmDialog";
import { ShortcutsDialog } from "./components/ShortcutsDialog";
import { CommandPalette } from "./components/CommandPalette";
import { SectionLabel, SidebarItem } from "./components/SidebarItem";
import { fmtShort, groupByDate, matchesQuery } from "./lib/format";
import { ModelRing } from "./lib/cable";
import { emitViewAction, focusComposer, focusSidebarSearch, isMac, isMod, keyLabel, ViewActive } from "./lib/shortcuts";
import { ScrollFade } from "./lib/useStickyScroll";
import { notifyInBackground } from "./lib/notify";
import { useDragRegion } from "./components/WindowControls";
import {
  ArrowLeftIcon,
  ArrowRightIcon,
  ChatsIcon,
  CodeIcon,
  FolderPlusIcon,
  KeyboardIcon,
  MenuIcon,
  PanelLeftIcon,
  PlusCircleIcon,
  PlusIcon,
  SearchIcon,
  SettingsIcon,
} from "./components/icons";

const LAST_MODE_KEY = "patchbay.lastMode";
const SIDEBAR_WIDTH_KEY = "patchbay.sidebarWidth";
const SIDEBAR_DEFAULT = 256;
const SIDEBAR_MIN = 248;
const SIDEBAR_MAX = 520;

const clampWidth = (w: number) => Math.round(Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, w)));

function loadSidebarWidth(): number {
  try {
    const w = Number(localStorage.getItem(SIDEBAR_WIDTH_KEY));
    return w ? clampWidth(w) : SIDEBAR_DEFAULT;
  } catch {
    return SIDEBAR_DEFAULT;
  }
}

function saveSidebarWidth(w: number) {
  try {
    localStorage.setItem(SIDEBAR_WIDTH_KEY, String(w));
  } catch {
    /* storage unavailable */
  }
}

/** Drag the sidebar's right edge to resize it; double-click resets, arrow keys nudge. */
function ResizeHandle({ width, onChange }: { width: number; onChange: (w: number, done: boolean) => void }) {
  const [dragging, setDragging] = useState(false);
  return (
    <div
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize sidebar"
      aria-valuenow={width}
      aria-valuemin={SIDEBAR_MIN}
      aria-valuemax={SIDEBAR_MAX}
      tabIndex={0}
      title="Drag to resize · double-click to reset"
      className="group absolute -right-1 top-0 bottom-0 z-20 w-2 cursor-col-resize outline-none"
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        const startX = e.clientX;
        const startW = width;
        const el = e.currentTarget;
        el.setPointerCapture(e.pointerId);
        setDragging(true);
        document.body.style.cursor = "col-resize";
        document.body.style.userSelect = "none";
        let last = startW;
        const move = (ev: PointerEvent) => {
          last = clampWidth(startW + ev.clientX - startX);
          onChange(last, false);
        };
        const up = () => {
          el.removeEventListener("pointermove", move);
          el.removeEventListener("pointerup", up);
          el.removeEventListener("pointercancel", up);
          document.body.style.cursor = "";
          document.body.style.userSelect = "";
          setDragging(false);
          onChange(last, true);
        };
        el.addEventListener("pointermove", move);
        el.addEventListener("pointerup", up);
        el.addEventListener("pointercancel", up);
      }}
      onDoubleClick={() => onChange(SIDEBAR_DEFAULT, true)}
      onKeyDown={(e) => {
        const step = e.shiftKey ? 48 : 16;
        if (e.key === "ArrowLeft") onChange(clampWidth(width - step), true);
        else if (e.key === "ArrowRight") onChange(clampWidth(width + step), true);
        else return;
        e.preventDefault();
      }}
    >
      <div
        className={`mx-auto h-full w-0.5 transition-colors ${
          dragging ? "bg-accent" : "bg-transparent group-hover:bg-accent/50 group-focus-visible:bg-accent"
        }`}
      />
    </div>
  );
}

/** New chat, or in Code a new session in the current folder (a folder picker when there is none). */
function newThing() {
  const st = useStore.getState();
  if (st.mode === "chat") {
    st.selectConversation(null);
    focusComposer();
    return;
  }
  const cur = st.codeSessions.find((s) => s.id === st.activeCodeSessionId);
  if (cur) newSessionIn(cur.cwd, cur.agent);
  else openFolderAsSession();
}

function useGlobalShortcuts() {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.isComposing) return;
      const st = useStore.getState();
      const modal = st.settingsOpen || !!st.confirm || st.shortcutsOpen || st.paletteOpen;
      // Back/forward: ⌘[ and ⌘] on macOS (Option+arrows move by word there), Alt+arrows elsewhere.
      const navBack = isMac ? isMod(e) && e.key === "[" : e.altKey && !e.ctrlKey && !e.metaKey && e.key === "ArrowLeft";
      const navFwd = isMac ? isMod(e) && e.key === "]" : e.altKey && !e.ctrlKey && !e.metaKey && e.key === "ArrowRight";
      if ((navBack || navFwd) && !e.shiftKey && !modal) {
        e.preventDefault();
        if (navBack) goBack();
        else goForward();
        return;
      }
      if (isMod(e) && !e.altKey) {
        const k = e.key.toLowerCase();
        // Zoom works everywhere, dialogs included. "+" arrives as "=" without Shift.
        if (e.key === "=" || e.key === "+" || e.key === "-" || e.key === "_" || e.key === "0") {
          e.preventDefault();
          if (e.key === "0") zoomReset();
          else if (e.key === "-" || e.key === "_") zoomOut();
          else zoomIn();
          return;
        }
        if (k === "/" || e.key === "?") {
          e.preventDefault();
          st.setShortcutsOpen(!st.shortcutsOpen);
          return;
        }
        if (modal) return;
        if (k === "n" && !e.shiftKey) {
          e.preventDefault();
          newThing();
        } else if (k === "k" && !e.shiftKey) {
          e.preventDefault();
          st.setPaletteOpen(true);
        } else if (k === "f" && e.shiftKey) {
          e.preventDefault();
          if (st.sidebarCollapsed) st.setSidebarCollapsed(false);
          focusSidebarSearch();
        } else if (k === "b" && !e.shiftKey) {
          e.preventDefault();
          st.setSidebarCollapsed(!st.sidebarCollapsed);
        } else if (e.key === "," && !e.shiftKey) {
          e.preventDefault();
          st.setSettingsOpen(true);
        } else if (k === "c" && e.shiftKey) {
          e.preventDefault();
          emitViewAction("copy-last");
        }
        return;
      }
      const t = e.target as HTMLElement | null;
      // Fields with their own Esc (rename, search, message edit) keep it; the composer doesn't.
      const ownEsc = !!t && (t.tagName === "INPUT" || (t.tagName === "TEXTAREA" && !t.hasAttribute("data-composer")));
      if (e.key === "Escape" && !modal && !ownEsc && !e.shiftKey && !e.ctrlKey && !e.metaKey && !e.altKey) {
        emitViewAction("stop");
      }
    };
    // Mouse back/forward buttons.
    const onMouse = (e: MouseEvent) => {
      if (e.button !== 3 && e.button !== 4) return;
      const st = useStore.getState();
      if (st.settingsOpen || st.confirm || st.shortcutsOpen || st.paletteOpen) return;
      e.preventDefault();
      if (e.button === 3) goBack();
      else goForward();
    };
    window.addEventListener("keydown", onKey);
    window.addEventListener("mouseup", onMouse);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("mouseup", onMouse);
    };
  }, []);
}

function ConversationItem({ c }: { c: Conversation }) {
  const active = useStore((s) => s.activeConversationId === c.id);
  const busy = useStore((s) => s.chatStreams[c.id]?.running);
  const { selectConversation, refreshConversations } = useStore.getState();
  return (
    <SidebarItem
      icon="conversation"
      ring={<ModelRing id={c.modelRef} />}
      meta={fmtShort(c.updatedAt)}
      title={c.title}
      active={active}
      busy={busy}
      pinned={c.pinned}
      onSelect={() => selectConversation(c.id)}
      onTogglePin={async () => {
        await api.updateConversation(c.id, { pinned: !c.pinned });
        refreshConversations();
      }}
      onRename={async (t) => {
        await api.updateConversation(c.id, { title: t });
        refreshConversations();
      }}
      onDelete={async () => {
        const { ok } = await useStore.getState().askConfirm({
          title: "Delete conversation?",
          body: `"${c.title}" and all of its messages will be permanently deleted.`,
          confirmLabel: "Delete",
          danger: true,
        });
        if (!ok) return;
        await api.deleteConversation(c.id);
        if (useStore.getState().activeConversationId === c.id) selectConversation(null);
        refreshConversations();
      }}
    />
  );
}

function ChatList() {
  const conversations = useStore((s) => s.conversations);
  const query = useStore((s) => s.sidebarQuery);
  const sections = useMemo(() => {
    const hits = conversations.filter((c) => matchesQuery(query, c.title));
    const pinned = hits.filter((c) => c.pinned);
    const rest = groupByDate(
      hits.filter((c) => !c.pinned),
      (c) => c.updatedAt,
    );
    return pinned.length ? [{ label: "Pinned", items: pinned }, ...rest] : rest;
  }, [conversations, query]);

  if (conversations.length === 0) return <div className="px-4 py-3 text-xs text-faint">No conversations yet</div>;
  if (sections.length === 0) return <div className="px-4 py-3 text-xs text-faint">No matches</div>;
  return (
    <>
      {sections.map((g) => (
        <div key={g.label} className="flex flex-col gap-0.5">
          <SectionLabel>{g.label}</SectionLabel>
          {g.items.map((c) => (
            <ConversationItem key={c.id} c={c} />
          ))}
        </div>
      ))}
    </>
  );
}

function SidebarSearch() {
  const query = useStore((s) => s.sidebarQuery);
  const setQuery = useStore((s) => s.setSidebarQuery);
  const mode = useStore((s) => s.mode);
  return (
    <div className="relative px-2 pb-1">
      <span className="pointer-events-none absolute left-[18px] top-1/2 flex -translate-y-1/2 text-faint">
        <SearchIcon size={15} />
      </span>
      <input
        id="sidebar-search"
        type="search"
        aria-label={`Filter ${mode === "chat" ? "chats" : "sessions"}`}
        className="peer h-9 w-full rounded-lg border border-transparent bg-transparent pl-9 pr-16 text-[14px] outline-none placeholder:text-muted hover:bg-panel-2/50 focus:border-border focus:bg-bg"
        placeholder={`Filter ${mode === "chat" ? "chats" : "sessions"}`}
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            setQuery("");
            e.currentTarget.blur();
          }
        }}
      />
      {!query && <Kbd className="pointer-events-none absolute right-4 top-1/2 -translate-y-1/2 peer-focus:hidden">{keyLabel("Mod+Shift+F")}</Kbd>}
    </div>
  );
}

function Kbd({ children, className = "" }: { children: ReactNode; className?: string }) {
  return (
    <kbd className={`rounded-[5px] border border-border px-1.5 font-mono text-[11px] leading-[18px] text-faint ${className}`}>
      {children}
    </kbd>
  );
}

function IconButton({
  label,
  onClick,
  disabled,
  children,
}: {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  children: ReactNode;
}) {
  return (
    <button
      className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg text-muted hover:bg-panel-2 hover:text-fg disabled:pointer-events-none disabled:opacity-35"
      onClick={onClick}
      disabled={disabled}
      title={label}
      aria-label={label}
    >
      {children}
    </button>
  );
}

const MODES: { mode: Mode; label: string; Icon: typeof ChatsIcon }[] = [
  { mode: "chat", label: "Chat", Icon: ChatsIcon },
  { mode: "code", label: "Code", Icon: CodeIcon },
];

/** Chat/Code switch: a segmented control whose highlight slides to the active tab. */
function ModeToggle({ vertical }: { vertical?: boolean }) {
  const mode = useStore((s) => s.mode);
  const setMode = useStore((s) => s.setMode);
  // A Code session waiting on a permission lights the Code tab while you're in Chat.
  const codeNeedsYou = useStore((s) => Object.values(s.code).some((c) => c.permissions.length > 0));
  const idx = MODES.findIndex((m) => m.mode === mode);
  // Wide and flat in the sidebar's top row, so it doesn't make that row taller; square in the rail.
  const size = vertical ? "h-8 w-10" : "h-[26px] w-[42px]";
  return (
    <div
      role="tablist"
      aria-label="Mode"
      className={`relative flex gap-0.5 rounded-[10px] border border-line bg-bg p-0.5 ${vertical ? "flex-col" : ""}`}
    >
      <span
        aria-hidden="true"
        className={`absolute left-0.5 top-0.5 ${size} rounded-[7px] bg-panel-2 shadow-[inset_0_1px_0_rgb(255_255_255/0.04)] ring-1 ring-border/50 transition-transform duration-200 ease-out`}
        style={{
          transform: vertical ? `translateY(calc(${idx * 100}% + ${idx * 2}px))` : `translateX(calc(${idx * 100}% + ${idx * 2}px))`,
        }}
      />
      {MODES.map(({ mode: m, label, Icon }) => {
        const dot = m === "code" && codeNeedsYou && mode !== "code";
        return (
          <button
            key={m}
            role="tab"
            aria-selected={mode === m}
            aria-label={dot ? `${label}, a session needs you` : label}
            title={dot ? `${label} · a session needs you` : label}
            className={`relative flex ${size} items-center justify-center rounded-[7px] transition-colors duration-200 ${
              mode === m ? "text-fg" : "text-muted hover:text-fg"
            }`}
            onClick={() => setMode(m)}
          >
            <Icon size={18} />
            {dot && (
              <span className="signal-pulse absolute right-1.5 top-1 h-[7px] w-[7px] rounded-full bg-accent ring-2 ring-bg" />
            )}
          </button>
        );
      })}
    </div>
  );
}

function NavButtons() {
  const canBack = useHistory((h) => h.back.length > 0);
  const canForward = useHistory((h) => h.forward.length > 0);
  return (
    <>
      <IconButton label={`Back (${isMac ? "⌘[" : "Alt+←"})`} onClick={goBack} disabled={!canBack}>
        <ArrowLeftIcon />
      </IconButton>
      <IconButton label={`Forward (${isMac ? "⌘]" : "Alt+→"})`} onClick={goForward} disabled={!canForward}>
        <ArrowRightIcon />
      </IconButton>
    </>
  );
}

function MenuItem({ label, keys, onClick }: { label: string; keys?: string; onClick: () => void }) {
  return (
    <button
      role="menuitem"
      className="mx-1 flex w-[calc(100%-0.5rem)] items-center gap-3 rounded-md px-2.5 py-1.5 text-left text-xs hover:bg-panel-2"
      onClick={onClick}
    >
      <span className="flex-1">{label}</span>
      {keys && <span className="font-mono text-[11px] text-faint">{keyLabel(keys)}</span>}
    </button>
  );
}

function AppMenu() {
  const [open, setOpen] = useState(false);
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!box.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        setOpen(false);
      }
    };
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey, true);
    };
  }, [open]);
  const run = (f: () => void) => () => {
    setOpen(false);
    f();
  };
  const st = useStore.getState();
  return (
    <div ref={box} className="relative">
      <IconButton label="Menu" onClick={() => setOpen(!open)}>
        <MenuIcon />
      </IconButton>
      {open && (
        <div
          role="menu"
          className="absolute left-0 top-full z-40 mt-1 w-60 rounded-xl border border-border bg-raised py-1 shadow-float"
        >
          <MenuItem
            label="New chat"
            keys={useStore.getState().mode === "chat" ? "Mod+N" : undefined}
            onClick={run(() => {
              st.setMode("chat");
              st.selectConversation(null);
              focusComposer();
            })}
          />
          <MenuItem
            label="Open folder…"
            onClick={run(() => {
              st.setMode("code");
              openFolderAsSession();
            })}
          />
          <MenuItem label="Search…" keys="Mod+K" onClick={run(() => st.setPaletteOpen(true))} />
          <div className="my-1 border-t border-line" />
          <MenuItem label="Settings" keys="Mod+," onClick={run(() => st.setSettingsOpen(true))} />
          <MenuItem label="Keyboard shortcuts" keys="Mod+/" onClick={run(() => st.setShortcutsOpen(true))} />
        </div>
      )}
    </div>
  );
}

/**
 * Chat and Code both stay mounted, so switching tabs doesn't reload messages or flash an empty
 * view. The hidden one keeps its layout (and scroll position) but is invisible and inert.
 */
function ViewPane({ active, children }: { active: boolean; children: ReactNode }) {
  return (
    <ViewActive.Provider value={active}>
      <div
        data-view-active={active}
        inert={!active}
        className={`absolute inset-0 flex ${active ? "" : "invisible pointer-events-none"}`}
      >
        {children}
      </div>
    </ViewActive.Provider>
  );
}

/** Keyed by mode, so a tab switch starts with a fresh scroll position and no stale fade. */
function SidebarList() {
  const mode = useStore((s) => s.mode);
  const [atTop, setAtTop] = useState(true);
  return (
    <div className="relative min-h-0 flex-1">
      <ScrollFade show={!atTop} />
      <div
        className={`flex h-full flex-col gap-0.5 overflow-y-auto pb-2 ${atTop ? "" : "fade-top"}`}
        onScroll={(e) => setAtTop(e.currentTarget.scrollTop < 1)}
      >
        {mode === "chat" ? <ChatList /> : <CodeSidebar />}
      </div>
    </div>
  );
}

function CollapsedRail() {
  const { setSidebarCollapsed, setSettingsOpen, setShortcutsOpen } = useStore.getState();
  const mode = useStore((s) => s.mode);
  const drag = useDragRegion();
  return (
    <aside {...drag} className="flex w-14 shrink-0 flex-col items-center gap-1 border-r border-line bg-panel py-2">
      <IconButton label={`Show sidebar (${keyLabel("Mod+B")})`} onClick={() => setSidebarCollapsed(false)}>
        <PanelLeftIcon />
      </IconButton>
      <NavButtons />
      <div className="my-1">
        <ModeToggle vertical />
      </div>
      <IconButton label={`${mode === "chat" ? "New chat" : "New session"} (${keyLabel("Mod+N")})`} onClick={newThing}>
        <PlusCircleIcon />
      </IconButton>
      <IconButton label={`Search (${keyLabel("Mod+K")})`} onClick={() => useStore.getState().setPaletteOpen(true)}>
        <SearchIcon />
      </IconButton>
      <div {...drag} className="flex-1 self-stretch" />
      <IconButton label={`Keyboard shortcuts (${keyLabel("Mod+/")})`} onClick={() => setShortcutsOpen(true)}>
        <KeyboardIcon />
      </IconButton>
      <IconButton label="Settings" onClick={() => setSettingsOpen(true)}>
        <SettingsIcon />
      </IconButton>
    </aside>
  );
}

export default function App() {
  const st = useStore();
  const {
    mode,
    setMode,
    settingsOpen,
    setSettingsOpen,
    setShortcutsOpen,
    sidebarCollapsed,
    setSidebarCollapsed,
    refreshConversations,
    refreshCodeSessions,
    refreshCatalog,
    toast,
    setToast,
  } = st;

  useGlobalShortcuts();
  useEffect(() => initZoom(isMod), []);
  const [sidebarWidth, setSidebarWidth] = useState(loadSidebarWidth);
  const drag = useDragRegion();

  useEffect(() => {
    useStore
      .getState()
      .loadPrefs()
      .then(() => {
        const { prefs } = useStore.getState();
        let last: string | null = null;
        try {
          last = localStorage.getItem(LAST_MODE_KEY);
        } catch {
          /* storage unavailable */
        }
        const tab = prefs.launchTab === "last" ? last : prefs.launchTab;
        if (tab === "chat" || tab === "code") setMode(tab);
        resetHistory();
      })
      .catch(() => {});
    Promise.all([refreshCatalog(), refreshConversations(), refreshCodeSessions()])
      .then(() => {
        const s = useStore.getState();
        if (s.models.length === 0) s.setSettingsOpen(true);
      })
      .catch((e) => setToast(errorText(e)));
    useStore.getState().refreshMesh();
    const unlisteners = [
      listen<CodeEnvelope>("code://event", (ev) => codeEventArrived(ev.payload)),
      // Mesh (see src-tauri/src/mesh): membership changes, other nodes' writes, live streams.
      listen("mesh://status", () => useStore.getState().refreshMesh()),
      listen<{ tables: string[] }>("mesh://changed", (ev) => useStore.getState().onSynced(ev.payload.tables)),
      listen<MeshChatEvent>("mesh://chat", (ev) => useStore.getState().applyMeshChat(ev.payload)),
      listen<{ name: string; code: string }>("mesh://join-request", (ev) => {
        const s = useStore.getState();
        s.setToast(`${ev.payload.name} asks to join the mesh. Approve it in Settings → Mesh.`);
        notifyInBackground("Mesh join request", `${ev.payload.name} asks to join (code ${ev.payload.code})`);
      }),
      listen<{ message: string }>("mesh://error", (ev) => useStore.getState().setToast(ev.payload.message)),
      // From the tray menu and the quick-entry window (see src-tauri/src/desktop.rs).
      listen("tray://new-chat", () => {
        const s = useStore.getState();
        s.setMode("chat");
        s.selectConversation(null);
        focusComposer();
      }),
      listen<string>("quick://prompt", (ev) => {
        const s = useStore.getState();
        s.setMode("chat");
        s.selectConversation(null);
        s.setPendingQuickPrompt(ev.payload);
      }),
    ];
    return () => {
      for (const un of unlisteners) un.then((f) => f());
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    try {
      localStorage.setItem(LAST_MODE_KEY, mode);
    } catch {
      /* storage unavailable */
    }
  }, [mode]);

  useEffect(() => {
    if (!toast) return;
    const t = setTimeout(() => setToast(null), 6000);
    return () => clearTimeout(t);
  }, [toast, setToast]);

  return (
    <div className="flex h-full">
      {sidebarCollapsed ? (
        <CollapsedRail />
      ) : (
        <aside
          className="relative flex shrink-0 flex-col border-r border-line bg-panel"
          style={{ width: sidebarWidth }}
        >
          <ResizeHandle
            width={sidebarWidth}
            onChange={(w, done) => {
              setSidebarWidth(w);
              if (done) saveSidebarWidth(w);
            }}
          />
          <div {...drag} className="flex items-center gap-0.5 px-2 pb-1 pt-2">
            <AppMenu />
            <IconButton label={`Hide sidebar (${keyLabel("Mod+B")})`} onClick={() => setSidebarCollapsed(true)}>
              <PanelLeftIcon />
            </IconButton>
            <NavButtons />
            <div {...drag} className="flex-1 self-stretch" />
            <ModeToggle />
          </div>
          <div className="px-2 pb-0.5 pt-2">
            <button
              className="flex h-9 w-full items-center gap-2.5 rounded-lg px-2.5 text-left text-[14px] font-medium text-fg hover:bg-panel-2/60"
              onClick={() => (mode === "chat" ? newThing() : openFolderAsSession())}
              title={mode === "chat" ? `New chat (${keyLabel("Mod+N")})` : undefined}
            >
              <span className="flex text-accent-ink">
                {mode === "chat" ? <PlusIcon size={17} /> : <FolderPlusIcon size={17} />}
              </span>
              <span className="flex-1">{mode === "chat" ? "New chat" : "Open folder…"}</span>
              {mode === "chat" && <Kbd>{keyLabel("Mod+N")}</Kbd>}
            </button>
          </div>
          <SidebarSearch />
          <SidebarList key={mode} />
          <div className="flex items-center gap-1 border-t border-line p-2">
            <button
              className="flex h-9 flex-1 items-center gap-2.5 rounded-lg px-2.5 text-left text-[14px] text-muted hover:bg-panel-2 hover:text-fg"
              onClick={() => setSettingsOpen(true)}
            >
              <SettingsIcon size={17} />
              Settings
            </button>
            <IconButton label={`Keyboard shortcuts (${keyLabel("Mod+/")})`} onClick={() => setShortcutsOpen(true)}>
              <KeyboardIcon />
            </IconButton>
          </div>
        </aside>
      )}
      <main className="relative min-w-0 flex-1">
        <ViewPane active={mode === "chat"}>
          <ChatView />
        </ViewPane>
        <ViewPane active={mode === "code"}>
          <CodeView />
        </ViewPane>
      </main>
      {settingsOpen && <Settings />}
      <ConfirmDialog />
      <ShortcutsDialog />
      <CommandPalette />
      {toast && (
        <div
          className="fixed bottom-4 right-4 z-50 max-w-md rounded-xl border border-border bg-raised px-4 py-3 text-xs shadow-float"
          onClick={() => setToast(null)}
        >
          {toast}
        </div>
      )}
    </div>
  );
}
