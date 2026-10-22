import { useEffect, useMemo, useState } from "react";

/** A string list in localStorage. Bad JSON or a missing key is an empty list. */
export function loadStringList(key: string): string[] {
  try {
    const v = JSON.parse(localStorage.getItem(key) ?? "[]");
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
  } catch {
    return [];
  }
}

export function saveStringList(key: string, values: string[]) {
  try {
    localStorage.setItem(key, JSON.stringify(values));
  } catch {
    /* storage unavailable */
  }
}

/**
 * `keys` in their default order. Keys the saved list has never seen come first, in that order.
 * The rest follow the saved list. Keys in the saved list that aren't in `keys` are not returned;
 * they stay in storage so a folder that drops out of view keeps its place.
 */
export function applyOrder(keys: string[], saved: string[]): string[] {
  const rank = new Map(saved.map((k, i) => [k, i]));
  const fresh = keys.filter((k) => !rank.has(k));
  const known = keys.filter((k) => rank.has(k)).sort((a, b) => rank.get(a)! - rank.get(b)!);
  return [...fresh, ...known];
}

/**
 * Move `key` to sit just above `visible[index]`, or past the end when `index` is `visible.length`.
 * `index === from` and `index === from + 1` are the same place, and change nothing.
 * `saved` keeps keys that aren't on screen. Returns null when the order would not change.
 */
export function moveInOrder(saved: string[], visible: string[], key: string, index: number): string[] | null {
  const from = visible.indexOf(key);
  if (from < 0 || index < 0 || index === from || index === from + 1) return null;
  const known = new Set(saved);
  const base = [...saved];
  // A key seen for the first time lives at the front until this write records it.
  for (const k of [...visible].reverse()) {
    if (!known.has(k)) base.unshift(k);
  }
  const next = base.filter((k) => k !== key);
  const before = visible[index];
  if (before !== undefined) {
    const at = next.indexOf(before);
    if (at < 0) next.unshift(key);
    else next.splice(at, 0, key);
  } else {
    const last = visible[visible.length - 1];
    const at = last === undefined ? -1 : next.indexOf(last);
    if (at < 0) next.push(key);
    else next.splice(at + 1, 0, key);
  }
  return next;
}

/**
 * Persist a manual order under `storageKey`. `keys` is the default order (newest first, usually).
 * A key seen for the first time is recorded at the front, so later activity doesn't move it.
 */
export function useManualOrder(storageKey: string, keys: string[]) {
  const [saved, setSaved] = useState(() => loadStringList(storageKey));
  const ordered = useMemo(() => applyOrder(keys, saved), [keys, saved]);

  useEffect(() => {
    const fresh = ordered.filter((k) => !saved.includes(k));
    if (fresh.length === 0) return;
    const next = [...fresh, ...saved];
    saveStringList(storageKey, next);
    setSaved(next);
  }, [ordered, saved, storageKey]);

  /** `visible` is the list the index refers to. Omit it to use `keys` in their saved order. */
  const move = (key: string, index: number, visible?: string[]) => {
    setSaved((cur) => {
      const vis = visible ?? applyOrder(keys, cur);
      const next = moveInOrder(cur, vis, key, index);
      if (!next) return cur;
      saveStringList(storageKey, next);
      return next;
    });
  };

  return { ordered, move };
}
