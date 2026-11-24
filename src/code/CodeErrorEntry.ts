import { createElement } from "react";

/** An error returned by the host, including a refused worker restart. */
export function CodeErrorEntry({ text }: { text: string }) {
  return createElement("div", {
    className: "whitespace-pre-wrap rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 font-mono text-xs text-danger",
  }, text);
}
