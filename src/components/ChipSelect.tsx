import type { ReactNode, SelectHTMLAttributes } from "react";
import { ChevronDownIcon } from "./icons";

/**
 * A native <select> dressed as a composer chip: a leading mark, the value, a chevron. The select
 * stays native so keyboard use, screen readers and the OS popup all keep working.
 */
export function ChipSelect({
  lead,
  mono,
  className = "",
  children,
  ...rest
}: SelectHTMLAttributes<HTMLSelectElement> & { lead?: ReactNode; mono?: boolean }) {
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
