import type { ReactNode } from "react";
import { Dot } from "./Dot";

/** Icon button for a view's header bar; `active` marks a toggled-open panel. */
export function HeaderButton({
  label,
  active,
  badge,
  dot,
  disabled,
  onClick,
  children,
  ...rest
}: {
  label: string;
  active?: boolean;
  /** Small count in the corner, e.g. changed files. */
  badge?: number;
  /** A marker that something is set, e.g. a system prompt. */
  dot?: boolean;
  disabled?: boolean;
  onClick: () => void;
  children: ReactNode;
  "aria-haspopup"?: "menu";
  "aria-expanded"?: boolean;
}) {
  return (
    <button
      className={`relative flex h-8 w-8 shrink-0 items-center justify-center rounded-lg ${
        active ? "bg-panel-2 text-fg" : "text-muted hover:bg-panel-2/60 hover:text-fg"
      } disabled:pointer-events-none disabled:opacity-40`}
      onClick={onClick}
      disabled={disabled}
      title={label}
      aria-label={badge ? `${label} (${badge})` : label}
      aria-pressed={rest["aria-haspopup"] ? undefined : active}
      {...rest}
    >
      {children}
      {badge ? (
        <span className="absolute -right-1 -top-1 min-w-4 rounded-full bg-fg px-1 text-center font-mono text-[10px] font-medium leading-4 text-bg">
          {badge > 99 ? "99+" : badge}
        </span>
      ) : dot ? (
        <Dot size={6} className="absolute right-1 top-1 text-fg/70" />
      ) : null}
    </button>
  );
}
