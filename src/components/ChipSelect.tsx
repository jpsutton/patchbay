import { createContext, useContext, type ReactNode, type SelectHTMLAttributes } from "react";
import { ChevronDownIcon } from "./icons";

/**
 * True inside the phone's composer options sheet, where each chip becomes a full-width row with
 * its label: a phone has no room for a line of chips, and no hover to explain one.
 */
export const ChipRows = createContext(false);

/** Read-only text among the chips, e.g. a model the agent doesn't let you change. */
export function ChipText({ label, children }: { label: string; children: ReactNode }) {
  if (useContext(ChipRows)) {
    return (
      <div className="flex h-14 items-center gap-3 px-3">
        <span className="shrink-0 text-[15px] text-fg">{label}</span>
        <span className="flex min-w-0 flex-1 items-center justify-end gap-2 font-mono text-[13px] text-muted">{children}</span>
      </div>
    );
  }
  return <span className="flex min-w-0 items-center gap-2 px-1 font-mono text-[12px] text-muted">{children}</span>;
}

/**
 * A native <select> dressed as a composer chip: a leading mark, the value, a chevron. The select
 * stays native so keyboard use, screen readers and the OS popup all keep working.
 */
export function ChipSelect({
  lead,
  mono,
  label,
  className = "",
  children,
  ...rest
}: SelectHTMLAttributes<HTMLSelectElement> & {
  lead?: ReactNode;
  mono?: boolean;
  /** Short name for the row in the options sheet; defaults to the aria-label. */
  label?: string;
}) {
  const rows = useContext(ChipRows);
  if (rows) {
    return (
      <label className={`flex h-14 items-center gap-3 px-3 ${rest.disabled ? "opacity-50" : ""}`}>
        <span className="shrink-0 text-[15px] text-fg">{label ?? rest["aria-label"]}</span>
        {/* No lead mark: a select is as wide as its longest option, which would strand it. */}
        <span className="relative flex min-w-0 flex-1 items-center justify-end text-muted">
          <select
            {...rest}
            className={`h-10 min-w-0 cursor-pointer appearance-none truncate bg-transparent pr-6 text-right outline-none ${
              mono ? "font-mono text-[13px] text-fg/90" : "text-[15px]"
            }`}
          >
            {children}
          </select>
          <span className="pointer-events-none absolute right-0 flex text-faint">
            <ChevronDownIcon size={14} />
          </span>
        </span>
      </label>
    );
  }
  return (
    <span
      className={`relative inline-flex h-8 min-w-0 shrink items-center rounded-[9px] border border-border text-muted focus-within:border-accent hover:bg-panel-2/60 hover:text-fg ${
        rest.disabled ? "pointer-events-none opacity-50" : ""
      } ${className}`}
    >
      {lead && <span className="pointer-events-none absolute left-2.5 flex items-center">{lead}</span>}
      <select
        {...rest}
        className={`h-full min-w-0 max-w-full cursor-pointer appearance-none truncate bg-transparent pr-7 outline-none ${
          lead ? "pl-7" : "pl-2.5"
        } ${mono ? "font-mono text-[12px] text-fg/90" : "text-[13px]"}`}
      >
        {children}
      </select>
      <span className="pointer-events-none absolute right-2 flex text-faint">
        <ChevronDownIcon size={12} />
      </span>
    </span>
  );
}
