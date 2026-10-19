import { useEffect } from "react";
import { create } from "zustand";
import { api, errorText } from "./ipc";
import { isRemoteApp, isWeb } from "./platform";
import { useStore } from "./store";

/** A newer release the app can install itself (see `update_check` in src-tauri/src/desktop.rs). */
export interface UpdateInfo {
  version: string;
  notes: string | null;
}

const FIRST_CHECK_MS = 15_000;
const CHECK_EVERY_MS = 6 * 60 * 60 * 1000;

export const useUpdate = create<{ available: UpdateInfo | null; installing: boolean }>(() => ({
  available: null,
  installing: false,
}));

/** Check for updates shortly after launch, then every few hours. Only the native app updates
 *  itself; the browser UI and the phone app follow their host. */
export function useUpdateChecks() {
  useEffect(() => {
    if (isWeb() || isRemoteApp()) return;
    const check = () =>
      api
        .updateCheck()
        .then((available) => useUpdate.setState({ available }))
        .catch((e) => console.warn("update check:", errorText(e)));
    const first = setTimeout(check, FIRST_CHECK_MS);
    const every = setInterval(check, CHECK_EVERY_MS);
    return () => {
      clearTimeout(first);
      clearInterval(every);
    };
  }, []);
}

/** Ask, then install the update and restart into it. */
export async function installUpdate() {
  const { available, installing } = useUpdate.getState();
  if (!available || installing) return;
  const { ok } = await useStore.getState().askConfirm({
    title: `Update to Patchbay ${available.version}?`,
    body: "Patchbay downloads the update and restarts. Agents still working in the Code tab are stopped.",
    confirmLabel: "Update and restart",
  });
  if (!ok) return;
  useUpdate.setState({ installing: true });
  try {
    await api.updateInstall();
  } catch (e) {
    useUpdate.setState({ installing: false });
    useStore.getState().setToast(`Update failed: ${errorText(e)}`);
  }
}
