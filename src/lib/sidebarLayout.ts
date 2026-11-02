/**
 * The Code sidebar's layout: user-made groups of folders, and the manual order of both.
 *
 * The layout syncs through the mesh (`sidebar_layout`). Every entry is a group or a folder.
 * `parent` is the key of the group an entry is in, or "" at the top level. `pos` orders the
 * entries that share a parent. A group holds folders only: groups don't nest.
 */

import { uuid } from "./platform";

export interface SidebarEntry {
  /** `group:<id>` for a group, or the folder's key. */
  key: string;
  kind: "group" | "folder";
  /** A group's name; empty for a folder. */
  name: string;
  /** The key of the group this entry is in; empty at the top level. */
  parent: string;
  pos: number;
}

export const GROUP_PREFIX = "group:";
/** Gap between neighbours' positions. A move renumbers its lists, so the gap never runs out. */
export const POS_STEP = 1024;

export const newGroupKey = () => `${GROUP_PREFIX}${uuid()}`;

/** One item at the top level of the sidebar. */
export type TopItem =
  | { type: "folder"; key: string }
  | { type: "group"; key: string; name: string; children: string[] };

/** Where an item lands: before `siblings[index]` in `parent`'s list, or last when `index` is the length. */
export interface Target {
  parent: string;
  index: number;
}

export interface LayoutChange {
  put: SidebarEntry[];
  remove: string[];
}

const byPos = (a: SidebarEntry, b: SidebarEntry) => a.pos - b.pos || (a.key < b.key ? -1 : a.key > b.key ? 1 : 0);

/** The key of the group `e` sits in, when that group still exists. */
function groupOf(e: SidebarEntry | undefined, groups: Map<string, SidebarEntry>): string {
  return e && e.parent !== "" && groups.has(e.parent) ? e.parent : "";
}

/**
 * The sidebar as a tree. `folders` are the folder keys on screen, newest first. One with no entry
 * of its own takes the place of the first key in `inherit` that has one: a project sits where its
 * folders are, and a folder where its project is. A folder with no place at all is fresh: it goes
 * first at the top level, in that order. A group stays even when it holds no folder on screen;
 * the caller decides whether to show it.
 */
export function arrange(
  folders: string[],
  layout: SidebarEntry[],
  inherit?: Map<string, string[]>,
): { top: TopItem[]; fresh: string[] } {
  const groups = new Map(layout.filter((e) => e.kind === "group").map((e) => [e.key, e]));
  const own = new Map(layout.filter((e) => e.kind === "folder").map((e) => [e.key, e]));
  const placed = new Map<string, SidebarEntry>();
  for (const k of folders) {
    const e = own.get(k) ?? inherit?.get(k)?.map((f) => own.get(f)).find((f) => f != null);
    if (e) placed.set(k, { ...e, key: k });
  }
  const fresh = folders.filter((k) => !placed.has(k));

  const children = new Map<string, SidebarEntry[]>();
  const loose: SidebarEntry[] = [];
  for (const k of folders) {
    const e = placed.get(k);
    if (!e) continue;
    const g = groupOf(e, groups);
    if (g === "") loose.push(e);
    else children.set(g, [...(children.get(g) ?? []), e]);
  }

  const rows: SidebarEntry[] = [...groups.values(), ...loose].sort(byPos);
  const top: TopItem[] = [
    ...fresh.map((key): TopItem => ({ type: "folder", key })),
    ...rows.map((e): TopItem =>
      e.kind === "group"
        ? { type: "group", key: e.key, name: e.name, children: (children.get(e.key) ?? []).sort(byPos).map((c) => c.key) }
        : { type: "folder", key: e.key },
    ),
  ];
  return { top, fresh };
}

/**
 * Put projects back where their folders are. Before projects took their folders' place, grouping
 * by project recorded every project as new: at the top level, outside its folders' groups. Each
 * project at the top level whose folders (`sources`, newest first) have a place moves to it. A
 * project someone put in a group stays there.
 */
export function planProjectsToFolders(layout: SidebarEntry[], sources: Map<string, string[]>): LayoutChange | null {
  const groups = new Map(layout.filter((e) => e.kind === "group").map((e) => [e.key, e]));
  const own = new Map(layout.filter((e) => e.kind === "folder").map((e) => [e.key, e]));
  const put: SidebarEntry[] = [];
  for (const [key, from] of sources) {
    const e = own.get(key);
    if (!e || groupOf(e, groups) !== "") continue;
    const f = from.map((k) => own.get(k)).find((x) => x != null);
    if (!f) continue;
    const parent = groupOf(f, groups);
    if (e.parent !== parent || e.pos !== f.pos) put.push({ ...e, parent, pos: f.pos });
  }
  return put.length ? { put, remove: [] } : null;
}

/** Entries that record folders the layout has never seen, so later activity doesn't move them. */
export function recordFresh(layout: SidebarEntry[], fresh: string[]): SidebarEntry[] {
  if (fresh.length === 0) return [];
  const topPos = layout.filter((e) => e.parent === "" || e.kind === "group").map((e) => e.pos);
  const base = topPos.length ? Math.min(...topPos) : POS_STEP * (fresh.length + 1);
  return fresh.map((key, i) => ({ key, kind: "folder", name: "", parent: "", pos: base - POS_STEP * (fresh.length - i) }));
}

/** Entries that lay `keys` out at the top level in that order: a first layout seeded from an older order. */
export function seedOrder(keys: string[]): SidebarEntry[] {
  return keys.map((key, i) => ({ key, kind: "folder", name: "", parent: "", pos: POS_STEP * (i + 1) }));
}

/** The visible sibling lists: the top level, then each group's folders. */
function lists(top: TopItem[]): Map<string, string[]> {
  const out = new Map<string, string[]>([["", top.map((t) => t.key)]]);
  for (const t of top) if (t.type === "group") out.set(t.key, [...t.children]);
  return out;
}

/** Where `key` sits now: its parent list and its index there. */
export function locate(top: TopItem[], key: string): Target | null {
  for (const [parent, list] of lists(top)) {
    const index = list.indexOf(key);
    if (index >= 0) return { parent, index };
  }
  return null;
}

/**
 * Move `key` to `target` (an index in the list as it is now, so dropping on the slot just below
 * or at the item itself changes nothing). Returns the writes, or null when nothing would change.
 * A group can only go to the top level.
 */
export function planMove(layout: SidebarEntry[], top: TopItem[], key: string, target: Target): LayoutChange | null {
  const all = lists(top);
  const from = locate(top, key);
  const dest = all.get(target.parent);
  if (!from || !dest) return null;
  const isGroup = key.startsWith(GROUP_PREFIX);
  if (isGroup && target.parent !== "") return null;
  if (!isGroup && target.parent === key) return null;
  if (target.index < 0 || target.index > dest.length) return null;
  if (from.parent === target.parent && (target.index === from.index || target.index === from.index + 1)) return null;

  const source = all.get(from.parent)!.filter((k) => k !== key);
  const next = from.parent === target.parent ? source : [...dest];
  const at = from.parent === target.parent && target.index > from.index ? target.index - 1 : target.index;
  next.splice(at, 0, key);

  const known = new Map(layout.map((e) => [e.key, e]));
  const groupName = (k: string) => (top.find((t) => t.key === k && t.type === "group") as { name: string } | undefined)?.name ?? "";
  const put: SidebarEntry[] = [];
  const number = (parent: string, list: string[]) =>
    list.forEach((k, i) => {
      const old = known.get(k);
      const e: SidebarEntry = {
        key: k,
        kind: k.startsWith(GROUP_PREFIX) ? "group" : "folder",
        name: k.startsWith(GROUP_PREFIX) ? (old?.name ?? groupName(k)) : "",
        parent,
        pos: POS_STEP * (i + 1),
      };
      if (!old || old.parent !== e.parent || old.pos !== e.pos) put.push(e);
    });
  number(target.parent, next);
  if (from.parent !== target.parent) number(from.parent, source);
  return put.length ? { put, remove: [] } : null;
}

/** A new group at the top level, holding `folder` when given. Goes where `folder` was, or first. */
export function planNewGroup(layout: SidebarEntry[], top: TopItem[], name: string, folder?: string): LayoutChange {
  const key = newGroupKey();
  const topPos = layout.filter((e) => e.parent === "" || e.kind === "group").map((e) => e.pos);
  const group: SidebarEntry = {
    key,
    kind: "group",
    name,
    parent: "",
    pos: (topPos.length ? Math.min(...topPos) : POS_STEP) - POS_STEP,
  };
  const put = [group];
  const from = folder ? locate(top, folder) : null;
  if (folder && from) {
    // Take the folder's place at the top level, so the group appears where the folder was.
    const old = layout.find((e) => e.key === folder);
    if (from.parent === "" && old) group.pos = old.pos;
    put.push({ key: folder, kind: "folder", name: "", parent: key, pos: POS_STEP });
  }
  return { put, remove: [] };
}

/** Rename a group. */
export function planRename(layout: SidebarEntry[], key: string, name: string): LayoutChange {
  const old = layout.find((e) => e.key === key);
  return old ? { put: [{ ...old, name }], remove: [] } : { put: [], remove: [] };
}

/** Delete a group; its folders move to the top level, in its place and order. */
export function planUngroup(layout: SidebarEntry[], top: TopItem[], key: string): LayoutChange {
  const group = top.find((t) => t.key === key && t.type === "group");
  const entry = layout.find((e) => e.key === key);
  if (!group || group.type !== "group" || !entry) return { put: [], remove: [key] };
  // The folders take the group's place; fractions would need real positions, so renumber the top level.
  const flat = top.flatMap((t) => (t.key === key ? group.children : [t.key]));
  const known = new Map(layout.map((e) => [e.key, e]));
  const put: SidebarEntry[] = [];
  flat.forEach((k, i) => {
    const old = known.get(k);
    const e: SidebarEntry = {
      key: k,
      kind: k.startsWith(GROUP_PREFIX) ? "group" : "folder",
      name: k.startsWith(GROUP_PREFIX) ? (old?.name ?? "") : "",
      parent: "",
      pos: POS_STEP * (i + 1),
    };
    if (!old || old.parent !== "" || old.pos !== e.pos) put.push(e);
  });
  // Folders in the group that aren't on screen lose their group too.
  for (const e of layout) if (e.parent === key && !flat.includes(e.key)) put.push({ ...e, parent: "" });
  return { put, remove: [key] };
}

/** Apply a change to a layout, as the backend will. */
export function applyChange(layout: SidebarEntry[], change: LayoutChange): SidebarEntry[] {
  const gone = new Set(change.remove);
  const next = new Map(layout.filter((e) => !gone.has(e.key)).map((e) => [e.key, e]));
  for (const e of change.put) next.set(e.key, e);
  return [...next.values()];
}
