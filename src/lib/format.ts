const DAY_MS = 86_400_000;

function startOfDay(ts: number): number {
  const d = new Date(ts);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** "3:42 PM" today, "Oct 3, 3:42 PM" this year, with the year otherwise. */
export function fmtTime(ts: number, now = Date.now()): string {
  const d = new Date(ts);
  const time = d.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
  if (startOfDay(ts) === startOfDay(now)) return time;
  const sameYear = d.getFullYear() === new Date(now).getFullYear();
  const date = d.toLocaleDateString(undefined, { month: "short", day: "numeric", ...(sameYear ? {} : { year: "numeric" }) });
  return `${date}, ${time}`;
}

export function fmtDateTime(ts: number): string {
  return new Date(ts).toLocaleString();
}

const BUCKETS = ["Today", "Yesterday", "Previous 7 days", "Previous 30 days", "Older"] as const;

function bucket(ts: number, now: number): (typeof BUCKETS)[number] {
  const today = startOfDay(now);
  if (ts >= today) return "Today";
  if (ts >= today - DAY_MS) return "Yesterday";
  if (ts >= today - 7 * DAY_MS) return "Previous 7 days";
  if (ts >= today - 30 * DAY_MS) return "Previous 30 days";
  return "Older";
}

/** Group items (already sorted newest first) into sidebar date sections, dropping empty ones. */
export function groupByDate<T>(items: T[], ts: (t: T) => number, now = Date.now()): { label: string; items: T[] }[] {
  const by = new Map<string, T[]>(BUCKETS.map((b) => [b, []]));
  for (const it of items) by.get(bucket(ts(it), now))!.push(it);
  return BUCKETS.map((label) => ({ label, items: by.get(label)! })).filter((g) => g.items.length > 0);
}

/** Case-insensitive match of every whitespace-separated term against any of the fields. */
export function matchesQuery(query: string, ...fields: (string | null | undefined)[]): boolean {
  const terms = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return true;
  const hay = fields.filter(Boolean).join("\n").toLowerCase();
  return terms.every((t) => hay.includes(t));
}

/** Compact sidebar time: "3:42 PM" today, "Mon" this week, "Oct 3" this year, "2025" before. */
export function fmtShort(ts: number, now = Date.now()): string {
  const d = new Date(ts);
  if (startOfDay(ts) === startOfDay(now)) return d.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
  if (ts >= startOfDay(now) - 6 * DAY_MS) return d.toLocaleDateString(undefined, { weekday: "short" });
  if (d.getFullYear() === new Date(now).getFullYear()) {
    return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
  }
  return String(d.getFullYear());
}
