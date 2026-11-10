import {
  Children,
  Fragment,
  createContext,
  isValidElement,
  useContext,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type ChangeEvent,
  type KeyboardEvent,
  type ReactNode,
  type SelectHTMLAttributes,
} from "react";
import { createPortal } from "react-dom";
import { groupItems, groupNames, matchByPrefix, placeChipMenu, topItems, type MenuItem, type MenuPlace } from "../lib/chipMenu";
import { CheckIcon, ChevronDownIcon, ChevronLeftIcon, ChevronRightIcon } from "./icons";

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

type ChipOption = Pick<MenuItem, "value" | "label" | "group">;

function optionText(node: ReactNode): string {
  if (node == null || typeof node === "boolean") return "";
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(optionText).join("");
  if (isValidElement(node)) return optionText((node.props as { children?: ReactNode }).children);
  return "";
}

/** `<option>` / `<optgroup>` children, in order. Callers already speak select. */
function readOptions(children: ReactNode): ChipOption[] {
  const out: ChipOption[] = [];
  const walk = (nodes: ReactNode, group?: string) => {
    Children.forEach(nodes, (child) => {
      if (!isValidElement(child)) return;
      const props = child.props as { value?: string | number; label?: string; children?: ReactNode };
      if (child.type === Fragment) {
        walk(props.children, group);
        return;
      }
      if (child.type === "optgroup") {
        walk(props.children, props.label);
        return;
      }
      if (child.type !== "option") return;
      const label = optionText(props.children).trim();
      out.push({ value: props.value == null ? label : String(props.value), label, group });
    });
  };
  walk(children);
  return out;
}

function currentValue(value: SelectHTMLAttributes<HTMLSelectElement>["value"]): string {
  if (value == null || Array.isArray(value)) return "";
  return String(value);
}

function samePlace(a: MenuPlace | null, b: MenuPlace): boolean {
  return a !== null && a.left === b.left && a.top === b.top && a.maxHeight === b.maxHeight && a.minWidth === b.minWidth && a.side === b.side;
}

type ChipProps = SelectHTMLAttributes<HTMLSelectElement> & {
  lead?: ReactNode;
  mono?: boolean;
  /** Short name for the row in the options sheet; defaults to the aria-label. */
  label?: string;
  /**
   * Desktop list: when the options span two or more `<optgroup>`s, open on the group names and
   * show one group's options once it is chosen. The phone's native select keeps the plain groups.
   */
  twoLevel?: boolean;
};

/**
 * A composer chip: a leading mark, the value, a chevron.
 *
 * Desktop draws its own list. A native `<select>` popup is an OS window the page cannot move.
 * WebKitGTK opens it downward, and a composer chip sits on the bottom edge of a maximized
 * window, so the list runs off the screen. This list opens upward when that side has no room.
 *
 * The phone options sheet keeps the native select: each chip is already a full-width row, and
 * the OS picker is what a phone expects.
 */
export function ChipSelect({ lead, mono, label, twoLevel, className = "", children, ...rest }: ChipProps) {
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
    <DesktopChip lead={lead} mono={mono} twoLevel={twoLevel} className={className} {...rest}>
      {children}
    </DesktopChip>
  );
}

/** Desktop chip. Own component so its hooks don't run on the phone's native row. */
function DesktopChip({ lead, mono, twoLevel, className = "", children, value, onChange, disabled, title, id, ...rest }: ChipProps) {
  const all = readOptions(children);
  const current = currentValue(value);
  const selected = all.find((o) => o.value === current);
  const selectedLabel = selected?.label ?? current;
  const groups = groupNames(all);
  const drill = !!twoLevel && groups.length >= 2;
  // Which group's list is showing; null is the list of groups. Always null when the chip opens.
  const [level, setLevel] = useState<string | null>(null);
  const openLevel = drill && level !== null && groups.includes(level) ? level : null;
  const top: MenuItem[] = drill ? topItems(all) : all;
  const options: MenuItem[] = openLevel !== null ? groupItems(all, openLevel) : top;
  const isCurrent = (o: MenuItem) => (o.drill ? o.value === selected?.group : o.value === current);
  const ariaLabel = rest["aria-label"];
  const listId = useId();
  const buttonRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const typed = useRef("");
  const typedAt = useRef(0);
  // Set while the pointer is down on an option. WebKit can blur the chip before the click lands;
  // closing on that blur unmounts the list and the choice is lost.
  const hold = useRef(false);
  const [open, setOpen] = useState(false);
  const [focus, setFocus] = useState(0);
  const [place, setPlace] = useState<MenuPlace | null>(null);
  const optionKey = `${openLevel ?? ""}\n${options.map((o) => `${o.drill ? "+" : ""}${o.group ?? ""}\0${o.value}`).join("\n")}`;
  const focusIdx = options.length === 0 ? 0 : Math.min(focus, options.length - 1);

  const measure = () => {
    const button = buttonRef.current;
    const menu = menuRef.current;
    if (!button || !menu) return;
    const anchor = button.getBoundingClientRect();
    if (anchor.bottom < 0 || anchor.top > window.innerHeight) {
      setOpen(false);
      return;
    }
    const height = menu.scrollHeight;
    let width = Math.max(menu.offsetWidth, anchor.width);
    let next = placeChipMenu(
      { top: anchor.top, bottom: anchor.bottom, left: anchor.left, right: anchor.right },
      { width, height },
      { width: window.innerWidth, height: window.innerHeight },
    );
    // A scrollbar appears once the list is capped. Measure that width before committing.
    if (next.maxHeight + 1 < height) {
      menu.style.maxHeight = `${next.maxHeight}px`;
      width = Math.max(menu.offsetWidth, anchor.width);
      next = placeChipMenu(
        { top: anchor.top, bottom: anchor.bottom, left: anchor.left, right: anchor.right },
        { width, height },
        { width: window.innerWidth, height: window.innerHeight },
      );
    }
    setPlace((prev) => (samePlace(prev, next) ? prev : next));
  };

  useLayoutEffect(() => {
    if (!open) return;
    measure();
    // optionKey, not `options`: a new array each render must not remeasure forever.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, optionKey]);

  useLayoutEffect(() => {
    if (!open || !place) return;
    menuRef.current?.querySelector("[data-active='true']")?.scrollIntoView({ block: "nearest" });
  }, [open, place, focusIdx]);

  useEffect(() => {
    if (!open) return;
    const away = (e: Event) => {
      const t = e.target as Node;
      if (buttonRef.current?.contains(t) || menuRef.current?.contains(t)) return;
      setOpen(false);
    };
    const release = () => {
      hold.current = false;
    };
    const onMove = (e: Event) => {
      if (menuRef.current?.contains(e.target as Node)) return;
      measure();
    };
    window.addEventListener("pointerdown", away, true);
    window.addEventListener("pointerup", release, true);
    window.addEventListener("resize", onMove);
    window.addEventListener("scroll", onMove, true);
    return () => {
      window.removeEventListener("pointerdown", away, true);
      window.removeEventListener("pointerup", release, true);
      window.removeEventListener("resize", onMove);
      window.removeEventListener("scroll", onMove, true);
    };
    // measure reads refs; re-subscribe only when the menu opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const openMenu = () => {
    if (disabled || all.length === 0) return;
    const at = top.findIndex(isCurrent);
    setLevel(null);
    setFocus(at < 0 ? 0 : at);
    typed.current = "";
    setPlace(null);
    setOpen(true);
  };

  const pick = (next: string) => {
    setOpen(false);
    if (next !== current) onChange?.({ target: { value: next } } as ChangeEvent<HTMLSelectElement>);
  };

  /** Show one group's options, with the current one focused when it is in that group. */
  const enter = (group: string) => {
    const at = groupItems(all, group).findIndex((o) => o.value === current);
    setLevel(group);
    setFocus(at < 0 ? 0 : at);
    typed.current = "";
  };

  /** Back to the list of groups, with the group just left focused. */
  const leave = () => {
    const at = top.findIndex((o) => o.drill && o.value === openLevel);
    setLevel(null);
    setFocus(at < 0 ? 0 : at);
    typed.current = "";
  };

  const choose = (opt: MenuItem) => (opt.drill ? enter(opt.value) : pick(opt.value));

  const onKeyDown = (e: KeyboardEvent<HTMLButtonElement>) => {
    if (disabled) return;
    if (!open) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp" || e.key === "Enter" || e.key === " " || (e.altKey && e.key === "ArrowDown")) {
        e.preventDefault();
        openMenu();
      }
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      setOpen(false);
      return;
    }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      e.stopPropagation();
      if (options.length === 0) return;
      const d = e.key === "ArrowDown" ? 1 : -1;
      setFocus((i) => (i + d + options.length) % options.length);
      return;
    }
    if (drill && openLevel !== null && (e.key === "ArrowLeft" || e.key === "Backspace")) {
      e.preventDefault();
      leave();
      return;
    }
    if (drill && e.key === "ArrowRight") {
      const opt = options[focusIdx];
      if (opt?.drill) {
        e.preventDefault();
        enter(opt.value);
      }
      return;
    }
    if (e.key === "Home") {
      e.preventDefault();
      setFocus(0);
      return;
    }
    if (e.key === "End") {
      e.preventDefault();
      setFocus(Math.max(0, options.length - 1));
      return;
    }
    if (e.key === "Tab") {
      setOpen(false);
      return;
    }
    const typing = e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey;
    const prefixLive = typed.current !== "" && Date.now() - typedAt.current < 700;
    if (e.key === " " && prefixLive) {
      // A space continues the prefix ("Claude Sonnet"), it does not choose.
    } else if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      const opt = options[focusIdx];
      if (opt) choose(opt);
      return;
    }
    if (!typing) return;
    e.preventDefault();
    const now = Date.now();
    const fresh = now - typedAt.current > 700;
    const prefix = fresh ? e.key : typed.current + e.key;
    typed.current = prefix;
    typedAt.current = now;
    setFocus((i) => matchByPrefix(options.map((o) => o.label), i, prefix, fresh && prefix.length === 1));
  };

  let lastGroup: string | undefined;
  const rows: ReactNode[] = [];
  if (openLevel !== null) {
    rows.push(
      <button
        key="back"
        type="button"
        tabIndex={-1}
        aria-label="Back to groups"
        className={`mb-1 flex h-8 w-full items-center gap-1.5 rounded-lg border-b border-border/60 px-2 text-left text-[12px] text-muted hover:bg-panel-2 hover:text-fg ${
          mono ? "font-mono" : ""
        }`}
        onMouseDown={(ev) => {
          hold.current = true;
          ev.preventDefault();
        }}
        onClick={leave}
      >
        <ChevronLeftIcon size={14} className="shrink-0" />
        <span className="min-w-0 flex-1 truncate">{openLevel}</span>
      </button>,
    );
  }
  options.forEach((opt, i) => {
    if (!drill && opt.group && opt.group !== lastGroup) {
      rows.push(
        <div key={`g:${opt.group}`} className="px-2.5 pb-0.5 pt-1.5 text-[11px] text-faint">
          {opt.group}
        </div>,
      );
    }
    lastGroup = opt.group;
    const chosen = isCurrent(opt);
    rows.push(
      <button
        key={`${opt.group ?? ""}:${opt.value}:${i}`}
        id={`${listId}-opt-${i}`}
        type="button"
        role="option"
        aria-selected={chosen}
        data-active={i === focusIdx ? "true" : undefined}
        className={`flex h-8 w-full items-center gap-2 rounded-lg px-2.5 text-left ${
          i === focusIdx ? "bg-panel-2 text-fg" : "text-fg/90"
        } ${mono ? "font-mono text-[12px]" : "text-[13px]"}`}
        onMouseDown={(ev) => {
          hold.current = true;
          ev.preventDefault();
        }}
        onMouseEnter={() => setFocus(i)}
        onClick={() => choose(opt)}
      >
        <span className="min-w-0 flex-1 truncate">{opt.label}</span>
        {opt.drill && <span className="shrink-0 text-[11px] text-faint">{opt.count}</span>}
        {chosen && <CheckIcon size={14} className="shrink-0 text-accent" />}
        {opt.drill && <ChevronRightIcon size={14} className="shrink-0 text-faint" />}
      </button>,
    );
  });

  return (
    <span
      className={`relative inline-flex h-8 min-w-0 shrink items-center rounded-[9px] border border-border text-muted focus-within:border-accent hover:bg-panel-2/60 hover:text-fg ${
        disabled ? "pointer-events-none opacity-50" : ""
      } ${className}`}
    >
      {lead && <span className="pointer-events-none absolute left-2.5 flex items-center">{lead}</span>}
      <button
        ref={buttonRef}
        id={id}
        type="button"
        disabled={!!disabled}
        title={title}
        aria-label={ariaLabel ? (selectedLabel ? `${ariaLabel}, ${selectedLabel}` : String(ariaLabel)) : undefined}
        role="combobox"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? listId : undefined}
        aria-activedescendant={open ? `${listId}-opt-${focusIdx}` : undefined}
        className={`h-full min-w-0 max-w-full cursor-pointer truncate border-0 bg-transparent py-0 pr-7 text-left outline-none ${
          lead ? "pl-7" : "pl-2.5"
        } ${mono ? "font-mono text-[12px] text-fg/90" : "text-[13px]"}`}
        onClick={() => (open ? setOpen(false) : openMenu())}
        onKeyDown={onKeyDown}
        onBlur={(e) => {
          if (hold.current || menuRef.current?.contains(e.relatedTarget as Node)) return;
          setOpen(false);
        }}
      >
        {selectedLabel}
      </button>
      <span className="pointer-events-none absolute right-2 flex text-faint">
        <ChevronDownIcon size={12} />
      </span>
      {open &&
        createPortal(
          <div
            ref={menuRef}
            id={listId}
            data-chip-menu=""
            role="listbox"
            aria-label={ariaLabel ? String(ariaLabel) : undefined}
            className={`fixed z-50 w-max max-h-[calc(100vh-16px)] max-w-[calc(100vw-16px)] overflow-y-auto overscroll-contain rounded-xl border border-border bg-raised p-1 shadow-float ${
              mono ? "font-mono" : ""
            }`}
            style={{
              left: place?.left ?? 0,
              top: place?.top ?? 0,
              maxHeight: place?.maxHeight,
              minWidth: place?.minWidth,
              visibility: place ? "visible" : "hidden",
            }}
          >
            {rows}
          </div>,
          document.body,
        )}
    </span>
  );
}
