import { useState, type ReactNode } from "react";
import { CheckIcon } from "./icons";
import { errorText } from "../lib/ipc";
import { useStore } from "../lib/store";

/** Copy to the clipboard; toasts and resolves false when the webview refuses. */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch (e) {
    useStore.getState().setToast(`Could not copy: ${errorText(e)}`);
    return false;
  }
}

/** A text link, or with `icon` a small square icon button labelled by `label`. */
export function CopyButton({
  text,
  label = "Copy",
  icon,
}: {
  text: string | (() => string);
  label?: string;
  icon?: ReactNode;
}) {
  const [done, setDone] = useState(false);
  const onClick = async () => {
    if (!(await copyText(typeof text === "function" ? text() : text))) return;
    setDone(true);
    setTimeout(() => setDone(false), 1200);
  };
  if (icon) {
    return (
      <button
        className={`flex h-7 w-7 items-center justify-center rounded-md hover:bg-panel-2 hover:text-fg ${done ? "text-ok" : ""}`}
        onClick={onClick}
        title={done ? "Copied" : label}
        aria-label={label}
      >
        {done ? <CheckIcon size={15} /> : icon}
      </button>
    );
  }
  return (
    <button className="hover:text-fg" onClick={onClick}>
      {done ? "Copied" : label}
    </button>
  );
}
