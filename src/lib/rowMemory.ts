import { createContext, useCallback, useContext, useState } from "react";
import { usePaneSession } from "./pane";

/**
 * The key of the transcript row being rendered. A virtualized transcript unmounts a row once it
 * is scrolled far away, so state kept in the row's components is lost; this says which row a
 * component belongs to, to key what it keeps outside React.
 */
export const RowKeyContext = createContext<string | null>(null);

/** Open/closed choices of rows that have been unmounted, by session, row and slot. */
const remembered = new Map<string, boolean>();

/**
 * `useState` for a disclosure's open state that survives the row unmounting and mounting again.
 * Without that, a row the user opened comes back closed with a different height than the one
 * measured, and the transcript shifts under the reader while they scroll. `slot` tells apart the
 * components within one row. Outside a transcript row it is plain state.
 *
 * `restored` is true when the state came from an earlier mount, i.e. someone chose it.
 */
export function useRememberedOpen(slot: string, initial: boolean): [open: boolean, setOpen: (open: boolean) => void, restored: boolean] {
  const session = usePaneSession();
  const row = useContext(RowKeyContext);
  const key = row === null ? null : `${session}\u0000${row}\u0000${slot}`;
  const [start] = useState(() => (key === null ? undefined : remembered.get(key)));
  const [open, setOpenState] = useState(start ?? initial);
  const setOpen = useCallback(
    (v: boolean) => {
      if (key !== null) remembered.set(key, v);
      setOpenState(v);
    },
    [key],
  );
  return [open, setOpen, start !== undefined];
}
