import { useSyncExternalStore } from "react";

/**
 * The icon-size preference, applied where every line icon is drawn (`icon()` in
 * components/icons.tsx). It is kept here, not read from the store, so icons.tsx needs no store
 * import, and so only the icons re-render when it changes: `applyUiPrefs` calls `setIconSize`,
 * every icon subscribes through `useIconSize`, and the change shows at once with no reload.
 */
export type IconSize = "small" | "medium" | "large";

const FACTOR: Record<IconSize, number> = { small: 0.875, medium: 1, large: 1.125 };

/** Icons this size or smaller sit in slots sized to them (disclosure chevrons, chip glyphs). */
const TINY = 12;

let current: IconSize = "medium";
const listeners = new Set<() => void>();

export function setIconSize(size: string | undefined) {
  const next: IconSize = size === "small" || size === "large" ? size : "medium";
  if (next === current) return;
  current = next;
  listeners.forEach((l) => l());
}

function subscribe(l: () => void) {
  listeners.add(l);
  return () => {
    listeners.delete(l);
  };
}

export function useIconSize(): IconSize {
  return useSyncExternalStore(subscribe, () => current);
}

/**
 * The size to draw an icon asked for at `size`. Medium returns `size` untouched, so default
 * rendering is exactly what it was before the preference existed. Other levels scale to a whole
 * even number of pixels (the 24-unit drawings stay crisp), and leave alone sizes that aren't a
 * plain pixel count and the tiny ones.
 */
export function iconPx(size: number | string, level: IconSize): number | string {
  if (level === "medium" || typeof size !== "number" || size <= TINY) return size;
  return Math.round((size * FACTOR[level]) / 2) * 2;
}
