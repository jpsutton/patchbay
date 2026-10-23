import { Dot } from "../components/Dot";

/**
 * Each model gets a "cable" color, shown as a hollow ring wherever that model appears (sidebar,
 * replies, pickers). Filled dots stay reserved for session state, so the two never look alike.
 */
const RINGS = ["text-cable-1", "text-cable-2", "text-cable-3", "text-cable-4", "text-cable-5"] as const;

/** Stable across runs: a small string hash of the model's id. */
export function cableIndex(id: string | null | undefined): number {
  if (!id) return 0;
  let h = 0;
  for (let i = 0; i < id.length; i++) h = (h * 31 + id.charCodeAt(i)) | 0;
  return Math.abs(h) % RINGS.length;
}

export function ModelRing({ id, className = "" }: { id: string | null | undefined; className?: string }) {
  return (
    <Dot hollow={2} className={`${id ? RINGS[cableIndex(id)] : "text-faint"} ${className}`} />
  );
}
