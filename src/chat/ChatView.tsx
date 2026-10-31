import { useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import { api, errorText, estimateCost, type Attachment, type ChatEvent, type Message } from "../lib/ipc";
import { useStore } from "../lib/store";
import { AttachmentChip, Composer } from "../components/Composer";
import { AttachedImage } from "../components/AgentImage";
import { Markdown } from "../components/Markdown";
import { EditMessage } from "../components/EditMessage";
import { fmtTokens, UsageBar } from "../components/UsageBar";
import { ModelPicker } from "../components/ModelPicker";
import { ExportMenu } from "../components/ExportMenu";
import { WidthMenu } from "../components/WidthMenu";
import { chatToJson, chatToMarkdown, saveExport } from "../lib/export";
import { fmtDateTime, fmtTime } from "../lib/format";
import { MessageTime } from "../components/MessageTime";
import { useViewAction, ViewActive } from "../lib/shortcuts";
import { HeaderButton } from "../components/HeaderButton";
import { useDragRegion, WindowControls } from "../components/WindowControls";
import {
  ChevronLeftIcon,
  ChevronRightIcon,
  CopyIcon,
  FileTextIcon,
  PencilIcon,
  RestartIcon,
  ScrollTextIcon,
} from "../components/icons";
import { ModelRing } from "../lib/cable";
import { JumpToBottom, ScrollFade, useStickyScroll } from "../lib/useStickyScroll";
import { CopyButton, copyText } from "../components/CopyButton";
import { ListenButton, ReadAloudButton, useSpeakingId, useStopSpeechWhenHidden } from "../components/ReadAloudButton";
import { speakingNow, stopSpeech, speakText } from "../lib/speech";
import { EffortPicker } from "../components/EffortPicker";
import { SystemPromptPanel } from "./SystemPromptPanel";
import { notifyInBackground } from "../lib/notify";
import { isRemoteApp } from "../lib/platform";
import { Dot } from "../components/Dot";

function BranchNav({ m, onSwitch }: { m: Message; onSwitch: (id: string) => void }) {
  const n = m.siblingIds.length;
  if (n < 2) return null;
  const i = m.siblingIds.indexOf(m.id);
  return (
    <span className="flex items-center text-muted" aria-label={`Version ${i + 1} of ${n}`}>
      <button
        className="flex h-7 w-6 items-center justify-center rounded-md hover:bg-panel-2 hover:text-fg disabled:opacity-30"
        disabled={i <= 0}
        onClick={() => onSwitch(m.siblingIds[i - 1])}
        aria-label="Previous version"
      >
        <ChevronLeftIcon size={13} />
      </button>
      <span className="meta px-0.5">
        {i + 1} / {n}
      </span>
      <button
        className="flex h-7 w-6 items-center justify-center rounded-md hover:bg-panel-2 hover:text-fg disabled:opacity-30"
        disabled={i >= n - 1}
        onClick={() => onSwitch(m.siblingIds[i + 1])}
        aria-label="Next version"
      >
        <ChevronRightIcon size={13} />
      </button>
    </span>
  );
}

function Reasoning({ text, live }: { text: string; live: boolean }) {
  const [open, setOpen] = useState(() => useStore.getState().prefs.expandReasoning);
  if (!text) return null;
  return (
    <div className="mb-3 text-xs">
      <button
        className="flex h-[30px] items-center gap-2 rounded-lg border border-line px-2.5 text-muted hover:bg-panel-2/60 hover:text-fg"
        onClick={() => setOpen(!open)}
        aria-expanded={open}
      >
        <span className={`flex transition-transform ${open ? "rotate-90" : ""}`}>
          <ChevronRightIcon size={12} />
        </span>
        <span className={live ? "signal-pulse" : ""}>{live ? "Thinking…" : "Thought process"}</span>
      </button>
      {open && (
        <div className="mt-2 whitespace-pre-wrap border-l-2 border-line pl-3 leading-relaxed text-muted">{text}</div>
      )}
    </div>
  );
}

function MessageRow({
  m,
  streamText,
  streamReasoning,
  live,
  editing,
  setEditing,
  onRetry,
  onEdit,
  onSwitch,
  highlight,
}: {
  m: Message;
  streamText?: string;
  streamReasoning?: string;
  live: boolean;
  editing: boolean;
  setEditing: (on: boolean) => void;
  /** Regenerate this reply (a new version). */
  onRetry?: () => void;
  onEdit?: (text: string, attachments: Attachment[]) => Promise<boolean>;
  onSwitch: (siblingId: string) => void;
  highlight?: boolean;
}) {
  const models = useStore((s) => s.models);
  const providers = useStore((s) => s.providers);
  const body = useRef<HTMLDivElement>(null);
  const reading = useSpeakingId() === m.id;
  const time = (
    <span className="meta text-faint" title={fmtDateTime(m.createdAt)}>
      {fmtTime(m.createdAt)}
    </span>
  );
  if (m.role === "user" && editing && onEdit) {
    return (
      <div className="flex flex-col">
        <EditMessage
          text={m.content}
          attachments={m.attachments}
          onCancel={() => setEditing(false)}
          onSubmit={async (t, a) => {
            if (await onEdit(t, a)) setEditing(false);
          }}
        />
      </div>
    );
  }
  if (m.role === "user") {
    return (
      <div
        data-msg-id={m.id}
        className={`group flex flex-col items-end gap-1.5 rounded-xl transition-colors duration-700 ${highlight ? "bg-accent/10" : ""}`}
      >
        {m.attachments.length > 0 && (
          <div className="flex flex-wrap justify-end gap-2">
            {m.attachments.map((a) =>
              a.kind === "image" ? (
                <AttachedImage key={a.id} att={a} fallback={<AttachmentChip att={a} />} />
              ) : (
                <AttachmentChip key={a.id} att={a} />
              ),
            )}
          </div>
        )}
        {m.content && (
          <div className="max-w-[80%] rounded-[16px_16px_4px_16px] border border-line bg-panel-2 px-3.5 py-2.5 leading-6">
            <Markdown text={m.content} keepBreaks />
          </div>
        )}
        <div
          className={`action-row flex items-center gap-0.5 text-muted transition-opacity group-hover:opacity-100 focus-within:opacity-100 ${
            m.siblingIds.length > 1 ? "" : "opacity-0"
          }`}
        >
          <BranchNav m={m} onSwitch={onSwitch} />
          <span className="mr-1">{isRemoteApp() ? <MessageTime at={m.createdAt} /> : time}</span>
          {onEdit && (
            <button
              className={`flex h-7 items-center justify-center rounded-md hover:bg-panel-2 hover:text-fg ${isRemoteApp() ? "gap-1 px-1.5 text-xs" : "w-7"}`}
              onClick={() => setEditing(true)}
              title="Edit and resend"
              aria-label="Edit message"
            >
              <PencilIcon size={14} />
              {isRemoteApp() && "Edit"}
            </button>
          )}
          {m.content && <CopyButton text={m.content} icon={<CopyIcon size={14} />} />}
        </div>
      </div>
    );
  }
  const text = live ? (streamText ?? "") : m.content;
  const reasoning = live ? (streamReasoning ?? "") : (m.reasoning ?? "");
  const model = models.find((x) => x.id === m.modelRef);
  const provider = model && providers.find((p) => p.id === model.providerId);
  return (
    <div
      data-msg-id={m.id}
      className={`group rounded-xl transition-colors duration-700 ${highlight ? "bg-accent/10" : ""}`}
    >
      {model && (
        <div className="mb-2 flex items-center gap-2 text-[12px] text-faint">
          <ModelRing id={m.modelRef} className={live ? "signal-pulse" : ""} />
          <span className="font-mono text-muted">{model.displayName}</span>
          {provider && <span>{provider.name}</span>}
        </div>
      )}
      <Reasoning text={reasoning} live={live && !text} />
      {text ? (
        <div ref={body}>
          <Markdown text={text} />
        </div>
      ) : live ? (
        <div className="flex h-6 items-center gap-1" aria-label="Waiting for the reply">
          <Dot size={6} className="signal-pulse text-accent" />
        </div>
      ) : null}
      {m.error && (
        <div className="mt-2 rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-xs text-danger">
          {m.error}
          {onRetry && (
            <button className="ml-3 font-medium underline" onClick={onRetry}>
              Retry
            </button>
          )}
        </div>
      )}
      {!live && (
        <div
          className={`action-row mt-2 flex items-center gap-0.5 text-muted transition-opacity group-hover:opacity-100 focus-within:opacity-100 ${
            m.siblingIds.length > 1 || reading ? "" : "opacity-0"
          }`}
        >
          {m.content && <ReadAloudButton id={m.id} text={m.content} />}
          {m.content && <CopyButton text={m.content} label="Copy Markdown" icon={<CopyIcon size={14} />} />}
          {m.content && (
            <CopyButton
              text={() => body.current?.innerText ?? m.content}
              label="Copy as plain text"
              icon={<FileTextIcon size={14} />}
            />
          )}
          {onRetry && !m.error && (
            <button
              className="flex h-7 w-7 items-center justify-center rounded-md hover:bg-panel-2 hover:text-fg"
              onClick={onRetry}
              title="Ask again for another version of this reply"
              aria-label="Regenerate"
            >
              <RestartIcon size={14} />
            </button>
          )}
          <BranchNav m={m} onSwitch={onSwitch} />
          {isRemoteApp() && <MessageTime at={m.createdAt} />}
          <span className="flex-1" />
          {!isRemoteApp() && (
            <span className="meta flex gap-2 text-faint">
              {fmtTime(m.createdAt)}
              {m.usage && (
                <span title={`${m.usage.input.toLocaleString()} input tokens, ${m.usage.output.toLocaleString()} output tokens`}>
                  · {fmtTokens(m.usage.input)} in · {fmtTokens(m.usage.output)} out
                </span>
              )}
            </span>
          )}
          {isRemoteApp() && m.usage && (
            <span
              className="meta text-faint"
              title={`${m.usage.input.toLocaleString()} input tokens, ${m.usage.output.toLocaleString()} output tokens`}
            >
              {fmtTokens(m.usage.input)} in · {fmtTokens(m.usage.output)} out
            </span>
          )}
        </div>
      )}
    </div>
  );
}

/** `leading`: something before the title (the phone app's back button). `controls`: more
 * composer controls (the phone app's computer picker). */
export function ChatView({ leading, controls }: { leading?: ReactNode; controls?: ReactNode } = {}) {
  const drag = useDragRegion();
  const activeId = useStore((s) => s.activeConversationId);
  const conversations = useStore((s) => s.conversations);
  const models = useStore((s) => s.models);
  const defaultModelRef = useStore((s) => s.defaultModelRef);
  const setDefaultModel = useStore((s) => s.setDefaultModel);
  const refreshConversations = useStore((s) => s.refreshConversations);
  const selectConversation = useStore((s) => s.selectConversation);
  const stream = useStore((s) => (activeId ? s.chatStreams[activeId] : undefined));
  const patchChatStream = useStore((s) => s.patchChatStream);
  const setToast = useStore((s) => s.setToast);

  const conv = conversations.find((c) => c.id === activeId) ?? null;
  const [messages, setMessages] = useState<Message[]>([]);
  const [showSystem, setShowSystem] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [highlightId, setHighlightId] = useState<string | null>(null);
  const effort = useStore((s) => s.chatEffort[activeId ?? "new"] ?? "default");
  const setChatEffort = useStore((s) => s.setChatEffort);
  const pendingJump = useStore((s) => s.pendingJump);
  const pendingQuickPrompt = useStore((s) => s.pendingQuickPrompt);
  const syncRev = useStore((s) => s.syncRev);
  const sticky = useStickyScroll([messages, stream?.text, stream?.reasoning]);

  const modelRef = conv?.modelRef && models.some((m) => m.id === conv.modelRef) ? conv.modelRef : defaultModelRef;
  const model = models.find((m) => m.id === modelRef);

  const load = useCallback(async (id: string | null) => {
    setMessages(id ? await api.listMessages(id) : []);
  }, []);

  useEffect(() => {
    setEditingId(null);
    const jump = useStore.getState().pendingJump;
    if (jump && jump.conversationId === activeId) return; // the jump effect loads
    load(activeId);
    sticky.pin();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeId, load]);

  // Another computer's changes landed (a new message, a finished reply): show them.
  useEffect(() => {
    if (syncRev && activeId) load(activeId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [syncRev]);

  // A reply already streaming on another computer when this window opened.
  useEffect(() => {
    api
      .meshChatStreams()
      .then((streams) => {
        for (const r of streams) {
          useStore.getState().applyMeshChat({
            conversationId: r.conversationId,
            streamer: r.streamer,
            streamerName: r.streamerName,
            event: { type: "snapshot", userMessageId: r.userMessageId, assistantMessageId: r.assistantMessageId, text: r.text, reasoning: r.reasoning },
          });
        }
      })
      .catch(() => {});
  }, []);

  // Jump to a search hit: switch to its branch if needed, then scroll and flash it.
  useEffect(() => {
    if (!pendingJump || pendingJump.conversationId !== activeId) return;
    const { conversationId, messageId } = pendingJump;
    useStore.getState().setPendingJump(null);
    (async () => {
      let msgs = await api.listMessages(conversationId);
      if (!msgs.some((m) => m.id === messageId)) {
        await api.switchBranch(conversationId, messageId);
        msgs = await api.listMessages(conversationId);
      }
      setMessages(msgs);
      setHighlightId(messageId);
      // After the rows render (a timer, since rAF stalls in background windows).
      setTimeout(() => document.querySelector(`[data-msg-id="${messageId}"]`)?.scrollIntoView({ block: "center" }), 30);
      setTimeout(() => setHighlightId(null), 1600);
    })();
  }, [pendingJump, activeId]);

  /** Stream one turn; `start` makes the backend call with the channel handler. */
  const runTurn = async (convId: string, start: (onEvent: (e: ChatEvent) => void) => Promise<void>) => {
    patchChatStream(convId, { assistantId: null, text: "", reasoning: "", status: null, error: null, running: true, remote: null });
    sticky.pin();
    try {
      await start((e) => {
          switch (e.type) {
            case "started":
              patchChatStream(convId, { assistantId: e.assistantMessageId });
              if (useStore.getState().activeConversationId === convId) load(convId);
              break;
            case "text":
              patchChatStream(convId, (s) => ({ text: s.text + e.text, status: null }));
              break;
            case "reasoning":
              patchChatStream(convId, (s) => ({ reasoning: s.reasoning + e.text }));
              break;
            case "status":
              patchChatStream(convId, { status: e.message });
              break;
            case "title":
              refreshConversations();
              break;
            case "error":
              patchChatStream(convId, { error: e.message, running: false });
              if (useStore.getState().activeConversationId === convId) load(convId);
              refreshConversations();
              break;
            case "done":
              patchChatStream(convId, { running: false });
              if (e.stopReason !== "cancelled") {
                const title = useStore.getState().conversations.find((c) => c.id === convId)?.title ?? "Chat";
                notifyInBackground("Reply ready", title, { kind: "chat", id: convId });
              }
              // The DB row is finalized right after this event; reload once it lands.
              setTimeout(() => {
                if (useStore.getState().activeConversationId === convId) load(convId);
                refreshConversations();
              }, 150);
              break;
          }
      });
    } catch (e) {
      patchChatStream(convId, { running: false, error: errorText(e) });
      setToast(errorText(e));
    }
  };

  const effortArg = effort === "default" ? null : effort;

  /** `parentId`: omitted = continue the visible branch; see `api.sendChatMessage`. */
  const send = async (text: string, atts: Attachment[], parentId?: string) => {
    if (!modelRef) {
      setToast("Pick a model first (Settings → Providers)");
      return;
    }
    let cid = activeId;
    if (!cid) {
      const c = await api.createConversation(modelRef);
      const sp = useStore.getState().prefs.defaultSystemPrompt.trim();
      if (sp) await api.updateConversation(c.id, { systemPrompt: sp });
      // The effort picked before the conversation existed carries over.
      const pre = useStore.getState().chatEffort.new;
      if (pre) setChatEffort(c.id, pre);
      await refreshConversations();
      selectConversation(c.id);
      cid = c.id;
    }
    const convId = cid;
    await runTurn(convId, (onEvent) =>
      api.sendChatMessage(convId, modelRef, text, atts.map((a) => a.id), { parentId, reasoningEffort: effortArg }, onEvent),
    );
  };

  // A prompt typed in the quick-entry window starts a new chat here.
  useEffect(() => {
    if (!pendingQuickPrompt || activeId || stream?.running) return;
    useStore.getState().setPendingQuickPrompt(null);
    send(pendingQuickPrompt, []);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pendingQuickPrompt, activeId]);

  /** Another version of the reply to this user message. */
  const regenerate = async (userMessageId: string) => {
    if (!conv || !modelRef) return;
    const convId = conv.id;
    await runTurn(convId, (onEvent) => api.regenerateReply(convId, modelRef, userMessageId, effortArg, onEvent));
  };

  // Editing adds a sibling version; the original stays reachable with ‹ ›.
  const editAndResend = async (idx: number, text: string, atts: Attachment[]): Promise<boolean> => {
    if (!conv) return false;
    send(text, atts, messages[idx].parentId ?? "");
    return true;
  };

  const switchBranch = async (siblingId: string) => {
    if (!conv) return;
    await api.switchBranch(conv.id, siblingId);
    await load(conv.id);
  };

  const running = !!stream?.running;
  // An empty chat centers the composer under the greeting instead of pinning it to the bottom.
  const empty = messages.length === 0 && !running;
  const viewActive = useContext(ViewActive);
  const listenOn = useStore((s) => s.prefs.readAloudListen);
  const skipListen = useRef(false);
  useStopSpeechWhenHidden();
  const turnRef = useRef({ id: activeId, running });

  useEffect(() => {
    const prev = turnRef.current;
    turnRef.current = { id: activeId, running };
    if (prev.id !== activeId || !prev.running || running) return;
    if (!viewActive || !listenOn) return;
    if (skipListen.current) {
      skipListen.current = false;
      return;
    }
    const cur = activeId ? useStore.getState().chatStreams[activeId] : undefined;
    if (!cur || cur.error || !cur.text.trim()) return;
    speakText(cur.text, cur.assistantId ?? `listen:${activeId}`).catch((e) =>
      setToast(e instanceof Error ? e.message : "Could not read this aloud"),
    );
  }, [activeId, running, viewActive, listenOn, setToast]);

  /** Conversation whose last message should open for editing once Stop finishes. Phone only. */
  const amendAfterStop = useRef<string | null>(null);

  useViewAction((a) => {
    if (a === "stop") {
      if (speakingNow()) {
        stopSpeech();
        return;
      }
      if (running && activeId) {
        skipListen.current = true;
        if (isRemoteApp()) amendAfterStop.current = activeId;
        api.cancelChat(activeId);
      }
    }
    if (a === "copy-last") {
      const last = [...messages].reverse().find((m) => m.role === "assistant" && m.content);
      if (last) {
        copyText(last.content).then((ok) => ok && setToast("Copied the last reply"));
      }
    }
  });

  useEffect(() => {
    if (!amendAfterStop.current) return;
    if (amendAfterStop.current !== activeId || !viewActive) {
      amendAfterStop.current = null;
      return;
    }
    if (running) return;
    const last = [...messages].reverse().find((m) => m.role === "user");
    if (!last) return;
    amendAfterStop.current = null;
    setEditingId(last.id);
    setTimeout(() => document.querySelector("[data-amend]")?.scrollIntoView({ block: "center" }), 30);
  }, [running, messages, activeId, viewActive]);

  const editLast = () => {
    if (running) return;
    const last = [...messages].reverse().find((m) => m.role === "user");
    if (last) setEditingId(last.id);
  };

  // Totals over the replies in this conversation; context = what the last call sent + produced.
  const withUsage = messages.filter((m) => m.usage);
  const totals = withUsage.reduce(
    (t, m) => ({
      input: t.input + m.usage!.input,
      cachedRead: t.cachedRead + m.usage!.cachedRead,
      cacheWrite: t.cacheWrite + m.usage!.cacheWrite,
      output: t.output + m.usage!.output,
      reasoning: t.reasoning + m.usage!.reasoning,
      costUsd: m.usage!.costUsd != null ? (t.costUsd ?? 0) + m.usage!.costUsd : t.costUsd,
      apiMs: t.apiMs + m.usage!.apiMs,
    }),
    { input: 0, cachedRead: 0, cacheWrite: 0, output: 0, reasoning: 0, costUsd: null as number | null, apiMs: 0 },
  );
  const lastUsage = withUsage[withUsage.length - 1]?.usage ?? null;
  // Per reply, with that reply's model prices (a conversation can switch models).
  const estimated = withUsage.reduce<number | null>((sum, m) => {
    if (m.usage!.costUsd != null) return sum;
    const c = estimateCost(m.usage!, models.find((x) => x.id === m.modelRef)?.params.pricing);
    return c == null ? sum : (sum ?? 0) + c;
  }, null);

  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <header {...drag} className={`flex h-12 shrink-0 items-center gap-1 border-b border-line pr-3 ${leading ? "pl-1" : "pl-6"}`}>
        {leading}
        <h1 {...drag} className="mr-2 min-w-0 flex-1 truncate text-[15px] font-medium">
          {conv?.title ?? "New chat"}
        </h1>
        <HeaderButton
          label={conv ? (conv.systemPrompt ? "System prompt (set)" : "System prompt") : "System prompt: send a message first"}
          active={showSystem}
          dot={!!conv?.systemPrompt}
          onClick={() => setShowSystem(!showSystem)}
          disabled={!conv}
        >
          <ScrollTextIcon />
        </HeaderButton>
        {!isRemoteApp() && <WidthMenu />}
        {!isRemoteApp() && (
          <ExportMenu
            disabled={!conv || messages.length === 0}
            onExport={(f) =>
              conv &&
              saveExport(conv.title, f, f === "markdown" ? chatToMarkdown(conv, messages, models) : chatToJson(conv, messages))
            }
          />
        )}
        <WindowControls />
      </header>
      {showSystem && conv && <SystemPromptPanel conv={conv} />}
      <div className="relative min-h-0 flex-1">
        <ScrollFade show={!sticky.atTop} />
        <div
          ref={sticky.ref}
          className={`h-full overflow-y-auto [overflow-anchor:none] ${sticky.atTop ? "" : "fade-top"}`}
          onScroll={sticky.onScroll}
        >
          <div
            className={`mx-auto flex max-w-[var(--chat-width)] flex-col gap-7 px-6 py-8 ${empty ? "h-full justify-end pb-8" : ""}`}
          >
            {empty && (
              <div className="flex flex-col items-center gap-2.5 text-center">
                <div className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">New chat</div>
                <div className="text-[32px] font-medium leading-tight tracking-[-0.02em]">What should we patch in?</div>
                <div className="text-muted">Any OpenAI- or Anthropic-compatible model. Attach images, PDFs or text files.</div>
              </div>
            )}
            {messages.map((m, i) => (
              <MessageRow
                key={m.id}
                m={m}
                live={running && m.id === stream?.assistantId}
                streamText={stream?.text}
                streamReasoning={stream?.reasoning}
                editing={editingId === m.id}
                setEditing={(on) => setEditingId(on ? m.id : null)}
                onRetry={m.role === "assistant" && m.parentId && !running ? () => regenerate(m.parentId!) : undefined}
                onEdit={m.role === "user" && !running ? (t, a) => editAndResend(i, t, a) : undefined}
                onSwitch={switchBranch}
                highlight={highlightId === m.id}
              />
            ))}
            {stream?.status && running && <div className="text-xs text-warn">{stream.status}</div>}
            {stream?.remote && running && (
              <div className="meta text-faint">Replying on {stream.remote}</div>
            )}
            {stream?.error && !running && !messages.some((m) => m.error) && (
              <div className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-xs text-danger">
                {stream.error}
              </div>
            )}
          </div>
        </div>
        <JumpToBottom show={!sticky.atBottom} onClick={sticky.toBottom} />
      </div>
      <div className="mx-auto w-full max-w-[var(--chat-width)] px-6 pb-3 pt-2">
        <Composer
          draftKey={`chat:${activeId ?? "new"}`}
          busy={running}
          onEditLast={editLast}
          onStop={() => {
            if (running && activeId) skipListen.current = true;
            if (running && activeId && isRemoteApp()) amendAfterStop.current = activeId;
            if (activeId) api.cancelChat(activeId);
          }}
          onSend={(t, a) => send(t, a)}
          placeholder={model ? `Message ${model.displayName}` : "Add a model in Settings to start"}
          warnNoVision={model ? !model.vision : false}
          footer={
            <>
              <ModelPicker
                value={modelRef}
                onChange={async (ref) => {
                  await setDefaultModel(ref);
                  if (conv) {
                    await api.updateConversation(conv.id, { modelRef: ref });
                    refreshConversations();
                  }
                }}
              />
              <EffortPicker
                value={effort}
                modelDefault={model?.params.reasoningEffort ?? null}
                onChange={(v) => setChatEffort(activeId ?? "new", v)}
              />
              <ListenButton />
              {controls}
            </>
          }
        />
        <UsageBar
          totals={withUsage.length ? totals : null}
          contextTokens={lastUsage ? lastUsage.input + lastUsage.output : null}
          contextWindow={model?.contextWindow ?? null}
          estimatedCostUsd={estimated}
          onConfigureWindow={() => useStore.getState().setSettingsOpen(true)}
        />
      </div>
      {empty && <div className="flex-[1.2]" />}
    </div>
  );
}
