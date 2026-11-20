// A Unix, `~/` or Windows path, or a relative one with a folder or an extension. No spaces or
// code punctuation, so `foo.bar()` and `a = b` stay plain.
const LOCAL = /^(?:~\/|\/|[A-Za-z]:[\\/]|\.{1,2}[\\/])?[\w.@+~-]+(?:[\\/][\w.@+~-]*)*$/;
// A network location: a pair of separators (either slash, any mix), a host and at least one more
// segment, so `//TODO` and a bare `\\host` stay plain. It links only so a click can copy it.
const UNC = /^[\\/]{2}[\w.@+~-]+(?:[\\/][\w.@+~-]+)+[\\/]?$/;
// A trailing line reference: `:12`, `:12:4`, `#L12` or `#L12-L20`.
const LINE = /(?::\d+(?::\d+)?|#L\d+(?:-L?\d+)?)$/;

/** The file part of `text` if it reads like a path, without any line reference. */
export function pathOf(text: string): string | null {
  const t = text.trim().replace(LINE, "");
  if (t.length < 2 || t.length > 1024 || t.includes("://")) return null;
  if (UNC.test(t)) return t;
  if (!LOCAL.test(t)) return null;
  if (!/[\\/]/.test(t) && !/\.[A-Za-z]\w*$/.test(t)) return null;
  if (/^\.+$/.test(t) || /^\d+(?:\.\d+)+$/.test(t)) return null;
  return t;
}
