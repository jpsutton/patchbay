/** Pure rules for the provider "extra headers and query parameters" editor (pb-20261111-34). */

export interface ExtraRow {
  name: string;
  value: string;
  /** The name came from the backend, so a stored value exists under it. */
  stored: boolean;
}

export interface ExtraEntry {
  name: string;
  value?: string;
}

/** Where the load of a provider's stored names stands. Saving is only allowed after "loaded". */
export type ExtrasLoad = "loading" | "loaded" | "failed";

/**
 * A save sends the whole list and an empty list clears the stored entry, so a save before the names
 * loaded (or after a failed load) would wipe values the user never saw. It is not allowed.
 */
export function canSaveExtras(load: ExtrasLoad, busy: boolean): boolean {
  return load === "loaded" && !busy;
}

/** Rows to send: blank rows are dropped, and a blank value means "keep the stored one". */
export function extraEntries(rows: ExtraRow[]): ExtraEntry[] {
  return rows
    .filter((r) => r.name.trim() !== "")
    .map((r) => ({ name: r.name.trim(), value: r.value === "" ? undefined : r.value }));
}
