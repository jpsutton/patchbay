// Dev-only page (`/sticky-harness.html`) that streams text into a useStickyScroll scroller, for
// scripts/e2e-sticky-scroll.mjs to drive on a real device. Not part of any build.
import { createRoot } from "react-dom/client";
import { useEffect, useState } from "react";
import { useStickyScroll } from "../lib/useStickyScroll";
import "../index.css";

const params = new URLSearchParams(location.search);
const LINES = Number(params.get("lines") ?? 200);
const EVERY_MS = Number(params.get("every") ?? 60);

function Harness() {
  const [n, setN] = useState(0);
  const sticky = useStickyScroll([n]);
  useEffect(() => {
    if (n >= LINES) return;
    const t = setTimeout(() => setN((x) => x + 1), EVERY_MS);
    return () => clearTimeout(t);
  }, [n]);
  useEffect(() => {
    (window as unknown as { __sticky: () => unknown }).__sticky = () => {
      const e = sticky.el;
      return e ? { n, gap: e.scrollHeight - e.scrollTop - e.clientHeight, dpr: devicePixelRatio } : null;
    };
  });
  return (
    <div className="fixed inset-0">
      <div ref={sticky.ref} onScroll={sticky.onScroll} className="h-full overflow-y-auto [overflow-anchor:none]">
        <div className="p-3">
          {Array.from({ length: n }, (_, i) => (
            <p key={i} className="mb-2 text-sm">
              Message {i}: streamed output that wraps across a couple of lines on a phone so rows have uneven heights.
            </p>
          ))}
        </div>
      </div>
    </div>
  );
}

createRoot(document.getElementById("root") as HTMLElement).render(<Harness />);
