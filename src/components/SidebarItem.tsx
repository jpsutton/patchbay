import { useState, type ReactNode } from "react";
import { ContextMenu, type ItemAction } from "./ItemMenu";
import type { Activity } from "../lib/activity";
import {
  ArchiveIcon,
  ChatIcon,
  FileDiffIcon,
  GitBranchIcon,
  GitCommitIcon,
  GitPullRequestIcon,
  MessageCodeIcon,
  StarIcon,
  XIcon,
} from "./icons";

export function SectionLabel({ children }: { children: ReactNode }) {
  return (
    <div className="px-4 pb-1.5 pt-3 font-mono text-[11px] uppercase tracking-[0.06em] text-faint">{children}</div>
  );
}

/** The collapsed "Archived" heading at the end of a sidebar list. */
export function ArchivedToggle({ count, open, onToggle }: { count: number; open: boolean; onToggle: () => void }) {
  if (count === 0) return null;
  return (
    <button
      className="mx-2 mt-2 flex items-center gap-1.5 rounded-lg px-2 pb-1.5 pt-3 text-left font-mono text-[11px] uppercase tracking-[0.06em] text-faint hover:text-muted"
      aria-expanded={open}
      onClick={onToggle}
    >
      <span className={`inline-block transition-transform ${open ? "rotate-90" : ""}`}>›</span>
      Archived · {count}
    </button>
  );
}

export function SidebarItem({
  title,
  subtitle,
  active,
  busy,
  status,
  icon,
  ring,
  meta,
  attention,
  indent,
  pinned,
  onSelect,
  onRename,
  onDelete,
  onArchive,
  onTogglePin,
  menu,
}: {
  title: string;
  subtitle?: string;
  active: boolean;
  busy?: boolean;
  /** Code sessions: whether an agent is attached. */
  status?: "running" | "idle";
  /** Leading icon: a chat, or what a Code session has done. */
  icon: "conversation" | Activity;
  /** Chats: the model's cable ring replaces the icon (a `ModelRing`). */
  ring?: ReactNode;
  /** Right-aligned mono detail, e.g. a short time; hidden while the row's buttons show. */
  meta?: string;
  /** A session waiting on the user (a permission request). */
  attention?: boolean;
  indent?: boolean;
  pinned?: boolean;
  onSelect: () => void;
  onRename: (t: string) => void;
  onDelete: () => void;
  /** When set, the row's hover button archives instead of deleting. */
  onArchive?: () => void;
  onTogglePin?: () => void;
  /** Right-click actions; `rename` starts this row's inline editor. */
  menu?: (rename: () => void) => ItemAction[];
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(title);
  const [menuAt, setMenuAt] = useState<{ x: number; y: number } | null>(null);
  const rename = () => {
    setDraft(title);
    setEditing(true);
  };
  if (editing) {
    return (
      <input
        autoFocus
        className="mx-2 rounded-lg border border-accent bg-bg px-2 py-1.5 text-[14px] outline-none"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={() => {
          setEditing(false);
          if (draft.trim() && draft !== title) onRename(draft.trim());
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") (e.target as HTMLInputElement).blur();
          if (e.key === "Escape") {
            setDraft(title);
            setEditing(false);
          }
        }}
      />
    );
  }
  return (
    <div
      className={`group mx-2 flex h-[34px] cursor-pointer items-center gap-2.5 rounded-lg pr-2 text-[14px] ${indent ? "pl-4" : "pl-2.5"} ${
        active ? "bg-panel-2 font-medium text-fg" : "text-fg/75 hover:bg-panel-2/50 hover:text-fg"
      } ${subtitle ? "h-auto py-1.5" : ""}`}
      onClick={onSelect}
      onDoubleClick={rename}
      onContextMenu={(e) => {
        if (!menu) return;
        e.preventDefault();
        setMenuAt({ x: e.clientX, y: e.clientY });
      }}
      title={menu ? "Double-click to rename, right-click for more" : "Double-click to rename"}
    >
      {menuAt && menu && (
        <ContextMenu x={menuAt.x} y={menuAt.y} title={title} actions={menu(rename)} onClose={() => setMenuAt(null)} />
      )}
      {ring ? (
        <span className={`flex w-4 shrink-0 justify-center ${busy ? "signal-pulse" : ""}`}>{ring}</span>
      ) : (
        <ItemIcon icon={icon} busy={busy} status={status} attention={attention} />
      )}
      <div className="min-w-0 flex-1">
        <div className="truncate">{title}</div>
        {subtitle && <div className="truncate font-mono text-[11px] font-normal text-faint">{subtitle}</div>}
      </div>
      {attention ? (
        <span className="shrink-0 text-[11px] font-normal text-accent-ink group-hover:hidden">Needs you</span>
      ) : (
        meta && <span className="meta shrink-0 font-normal text-faint group-hover:hidden">{meta}</span>
      )}
      {onTogglePin && (
        <button
          className={`${pinned && !meta && !attention ? "flex text-accent-ink" : "hidden"} ${pinned ? "text-accent-ink" : "text-muted"} h-6 w-6 shrink-0 items-center justify-center rounded-md hover:bg-panel-2 hover:text-fg group-hover:flex`}
          onClick={(e) => {
            e.stopPropagation();
            onTogglePin();
          }}
          title={pinned ? "Unpin" : "Pin"}
          aria-label={`${pinned ? "Unpin" : "Pin"} ${title}`}
        >
          <StarIcon size={14} fill={pinned ? "currentColor" : "none"} />
        </button>
      )}
      {onArchive ? (
        <button
          className="hidden h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted hover:bg-panel-2 hover:text-fg group-hover:flex"
          onClick={(e) => {
            e.stopPropagation();
            onArchive();
          }}
          title="Archive"
          aria-label={`Archive ${title}`}
        >
          <ArchiveIcon size={14} />
        </button>
      ) : (
        <button
          className="hidden h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted hover:bg-panel-2 hover:text-danger group-hover:flex"
          onClick={(e) => {
            e.stopPropagation();
            onDelete();
          }}
          aria-label={`Delete ${title}`}
        >
          <XIcon size={14} />
        </button>
      )}
    </div>
  );
}

const ICONS: Record<"conversation" | Activity, { Icon: typeof ChatIcon; color: string; label: string }> = {
  conversation: { Icon: ChatIcon, color: "text-muted", label: "Chat" },
  chat: { Icon: MessageCodeIcon, color: "text-muted", label: "No file changes yet" },
  edit: { Icon: FileDiffIcon, color: "text-accent-ink", label: "Edited files" },
  commit: { Icon: GitCommitIcon, color: "text-live", label: "Made a commit" },
  branch: { Icon: GitBranchIcon, color: "text-live", label: "Created a branch" },
  pr: { Icon: GitPullRequestIcon, color: "text-pr", label: "Opened a pull request" },
};

function ItemIcon({
  icon,
  busy,
  status,
  attention,
}: {
  icon: "conversation" | Activity;
  busy?: boolean;
  status?: "running" | "idle";
  attention?: boolean;
}) {
  const { Icon, color, label } = ICONS[icon];
  const state = attention
    ? "needs you"
    : busy
      ? "working"
      : status === "running"
        ? "agent running"
        : status === "idle"
          ? "agent stopped"
          : null;
  // The status light: amber = needs you, blue = working, green = agent ready. None when stopped.
  const light = attention ? "bg-accent signal-pulse" : busy ? "bg-live" : status === "running" ? "bg-ok" : null;
  return (
    <span className={`relative flex shrink-0 ${color}`} title={state ? `${label} · ${state}` : label}>
      <Icon size={16} />
      {light && <span className={`absolute -bottom-0.5 -right-0.5 h-2 w-2 rounded-full ring-2 ring-panel ${light}`} />}
    </span>
  );
}
