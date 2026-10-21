import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { XIcon } from "./icons";

/** Object URLs for image data, shared by everything showing the same image, with their users. */
const blobUrls = new Map<string, { url: string; users: number }>();

/**
 * An object URL for base64 image data, held until `release`. A `data:` URL would put the whole
 * image, as text, in the page once per `<img>`, and a long session can hold many screenshots.
 */
function acquireUrl(data: string, mimeType: string): string {
  const key = `${mimeType};${data}`;
  const hit = blobUrls.get(key);
  if (hit) {
    hit.users++;
    return hit.url;
  }
  let url: string;
  try {
    const bin = atob(data);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    url = URL.createObjectURL(new Blob([bytes], { type: mimeType }));
  } catch {
    // Not valid base64: let the browser show it as broken, the way the data URL did.
    return `data:${mimeType};base64,${data}`;
  }
  blobUrls.set(key, { url, users: 1 });
  return url;
}

function releaseUrl(data: string, mimeType: string) {
  const key = `${mimeType};${data}`;
  const hit = blobUrls.get(key);
  if (!hit || --hit.users > 0) return;
  blobUrls.delete(key);
  URL.revokeObjectURL(hit.url);
}

/** The image's object URL while the component is mounted. */
function useImageUrl(data: string, mimeType: string): string | null {
  const [url, setUrl] = useState<string | null>(null);
  useEffect(() => {
    setUrl(acquireUrl(data, mimeType));
    return () => releaseUrl(data, mimeType);
  }, [data, mimeType]);
  return url;
}

/** An image an agent produced (an ACP image block), shown inline; a click opens it full size. */
export function AgentImage({ data, mimeType, alt }: { data: string; mimeType: string; alt?: string }) {
  const [open, setOpen] = useState(false);
  const src = useImageUrl(data, mimeType);
  if (!src) return null;
  return (
    <>
      <button
        className="block w-fit max-w-full overflow-hidden rounded-lg border border-line bg-bg"
        onClick={() => setOpen(true)}
        aria-label={alt ? `Open ${alt}` : "Open image"}
      >
        <img
          src={src}
          alt={alt ?? ""}
          loading="lazy"
          decoding="async"
          className="block max-h-[28rem] max-w-full object-contain"
        />
      </button>
      {open && <ImageViewer src={src} alt={alt} onClose={() => setOpen(false)} />}
    </>
  );
}

type View = { s: number; x: number; y: number };
const FIT: View = { s: 1, x: 0, y: 0 };
/** Movement under this many pixels, released quickly, is a tap rather than a drag. */
const TAP_SLOP = 6;
const TAP_MS = 300;

/**
 * The image over everything, fitted to the screen. Pinch or the mouse wheel zooms around the
 * fingers or pointer, one finger or the mouse pans a zoomed image, and a tap on the image toggles
 * between fitted and actual size. A tap outside the image, the close button or Escape closes.
 */
function ImageViewer({ src, alt, onClose }: { src: string; alt?: string; onClose: () => void }) {
  const stage = useRef<HTMLDivElement>(null);
  const img = useRef<HTMLImageElement>(null);
  const [view, setView] = useState<View>(FIT);
  const [animate, setAnimate] = useState(false);
  const viewRef = useRef(view);
  const pointers = useRef(new Map<number, { x: number; y: number }>());
  const gesture = useRef<{ x: number; y: number; t: number; moved: boolean; multi: boolean } | null>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  /** Zoom limit: twice actual size, and at least 4x the fitted size. */
  const maxScale = () => {
    const el = img.current;
    return el && el.offsetWidth ? Math.max(4, (2 * el.naturalWidth) / el.offsetWidth) : 4;
  };

  /** Keeps a zoomed image covering the screen: no panning past its edges. */
  const clamp = (v: View): View => {
    const el = img.current;
    const box = stage.current?.getBoundingClientRect();
    if (!el || !box) return v;
    const s = Math.min(Math.max(v.s, 1), maxScale());
    const mx = Math.max(0, (el.offsetWidth * s - box.width) / 2);
    const my = Math.max(0, (el.offsetHeight * s - box.height) / 2);
    return { s, x: Math.min(mx, Math.max(-mx, v.x)), y: Math.min(my, Math.max(-my, v.y)) };
  };

  /** Scale by `k` while the screen point (px, py) stays put. */
  const zoomAt = (v: View, k: number, px: number, py: number): View => {
    const box = stage.current!.getBoundingClientRect();
    const s = Math.min(Math.max(v.s * k, 1), maxScale());
    const cx = px - box.left - box.width / 2;
    const cy = py - box.top - box.height / 2;
    const r = s / v.s;
    return { s, x: cx - (cx - v.x) * r, y: cy - (cy - v.y) * r };
  };

  // Both fingers' moves can land before React re-renders, so the ref leads the state.
  const set = (v: View, smooth = false) => {
    viewRef.current = clamp(v);
    setAnimate(smooth);
    setView(viewRef.current);
  };

  useEffect(() => {
    const el = stage.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      set(zoomAt(viewRef.current, Math.exp(-e.deltaY * 0.002), e.clientX, e.clientY));
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  });

  const onTap = (x: number, y: number) => {
    const el = img.current;
    const r = el?.getBoundingClientRect();
    if (!el || !r || x < r.left || x > r.right || y < r.top || y > r.bottom) return onClose();
    const v = viewRef.current;
    if (v.s > 1.01) return set(FIT, true);
    set(zoomAt(v, Math.max(2, el.naturalWidth / el.offsetWidth), x, y), true);
  };

  return createPortal(
    <div
      role="dialog"
      aria-modal="true"
      aria-label={alt || "Image"}
      className="fixed inset-0 z-50 bg-black/85 pb-[env(safe-area-inset-bottom)] pt-[env(safe-area-inset-top)]"
    >
      <div
        ref={stage}
        className="relative flex h-full w-full touch-none select-none items-center justify-center overflow-hidden p-4"
        onPointerDown={(e) => {
          e.currentTarget.setPointerCapture(e.pointerId);
          pointers.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
          if (pointers.current.size === 1) gesture.current = { x: e.clientX, y: e.clientY, t: Date.now(), moved: false, multi: false };
          else if (gesture.current) gesture.current.multi = true;
        }}
        onPointerMove={(e) => {
          const prev = pointers.current.get(e.pointerId);
          if (!prev) return;
          const g = gesture.current;
          if (g && Math.hypot(e.clientX - g.x, e.clientY - g.y) > TAP_SLOP) g.moved = true;
          const others = [...pointers.current].filter(([id]) => id !== e.pointerId).map(([, p]) => p);
          pointers.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
          const v = viewRef.current;
          if (others.length === 0) {
            // One finger or the mouse: pan, only once zoomed in.
            if (v.s > 1) set({ ...v, x: v.x + e.clientX - prev.x, y: v.y + e.clientY - prev.y });
            return;
          }
          // Two fingers: scale by the change in their spread, around their midpoint, and follow it.
          const o = others[0];
          const d0 = Math.hypot(prev.x - o.x, prev.y - o.y);
          const d1 = Math.hypot(e.clientX - o.x, e.clientY - o.y);
          if (d0 < 1) return;
          const m0 = { x: (prev.x + o.x) / 2, y: (prev.y + o.y) / 2 };
          const m1 = { x: (e.clientX + o.x) / 2, y: (e.clientY + o.y) / 2 };
          const z = zoomAt(v, d1 / d0, m0.x, m0.y);
          set({ ...z, x: z.x + m1.x - m0.x, y: z.y + m1.y - m0.y });
        }}
        onPointerUp={(e) => {
          pointers.current.delete(e.pointerId);
          const g = gesture.current;
          if (pointers.current.size > 0) return;
          gesture.current = null;
          if (g && !g.moved && !g.multi && Date.now() - g.t < TAP_MS) onTap(e.clientX, e.clientY);
        }}
        onPointerCancel={(e) => {
          pointers.current.delete(e.pointerId);
          if (pointers.current.size === 0) gesture.current = null;
        }}
      >
        <img
          ref={img}
          src={src}
          alt={alt ?? ""}
          draggable={false}
          className={`block max-h-full max-w-full object-contain ${view.s > 1 ? "cursor-grab" : "cursor-zoom-in"}`}
          style={{
            transform: `translate(${view.x}px, ${view.y}px) scale(${view.s})`,
            transition: animate ? "transform 180ms ease-out" : undefined,
          }}
        />
      </div>
      <button
        className="absolute right-3 top-[max(0.75rem,env(safe-area-inset-top))] flex h-9 w-9 items-center justify-center rounded-full bg-white/10 text-white hover:bg-white/20"
        onClick={onClose}
        aria-label="Close"
      >
        <XIcon size={18} />
      </button>
    </div>,
    document.body,
  );
}
