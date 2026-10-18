import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { XIcon } from "./icons";

/** An image an agent produced (an ACP image block), shown inline; a click opens it full size. */
export function AgentImage({ data, mimeType, alt }: { data: string; mimeType: string; alt?: string }) {
  const [open, setOpen] = useState(false);
  const src = `data:${mimeType};base64,${data}`;
  return (
    <>
      <button
        className="block w-fit max-w-full overflow-hidden rounded-lg border border-line bg-bg"
        onClick={() => setOpen(true)}
        aria-label={alt ? `Open ${alt}` : "Open image"}
      >
        <img src={src} alt={alt ?? ""} className="block max-h-[28rem] max-w-full object-contain" />
      </button>
      {open && <ImageViewer src={src} alt={alt} onClose={() => setOpen(false)} />}
    </>
  );
}

/**
 * The image over everything, fitted to the screen. Clicking the image toggles its actual size
 * (scrollable, for reading a large render on a phone); clicking outside it or Escape closes.
 */
function ImageViewer({ src, alt, onClose }: { src: string; alt?: string; onClose: () => void }) {
  const [actual, setActual] = useState(false);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return createPortal(
    <div
      role="dialog"
      aria-modal="true"
      aria-label={alt || "Image"}
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/85 p-4 pb-[max(1rem,env(safe-area-inset-bottom))] pt-[max(1rem,env(safe-area-inset-top))]"
      onClick={onClose}
    >
      <button
        className="absolute right-3 top-[max(0.75rem,env(safe-area-inset-top))] flex h-9 w-9 items-center justify-center rounded-full bg-white/10 text-white hover:bg-white/20"
        onClick={onClose}
        aria-label="Close"
      >
        <XIcon size={18} />
      </button>
      <div className="max-h-full max-w-full overflow-auto" onClick={(e) => e.stopPropagation()}>
        <img
          src={src}
          alt={alt ?? ""}
          title={actual ? "Fit to screen" : "Actual size"}
          className={
            actual
              ? "block max-w-none cursor-zoom-out"
              : "block max-h-[calc(100dvh-2rem)] max-w-[calc(100vw-2rem)] cursor-zoom-in object-contain"
          }
          onClick={() => setActual(!actual)}
        />
      </div>
    </div>,
    document.body,
  );
}
