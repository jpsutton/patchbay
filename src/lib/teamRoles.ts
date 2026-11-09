import { useEffect, useState } from "react";
import { api } from "./ipc";

/** Role name by session id for sessions on live teams; refreshed on a timer and when the Teams page changes something. */
export function useTeamRoles(): Record<string, string> {
  const [roles, setRoles] = useState<Record<string, string>>({});
  useEffect(() => {
    const load = () => api.teamSessionRoles().then(setRoles, () => {});
    load();
    const t = setInterval(load, 5000);
    window.addEventListener("teams-changed", load);
    return () => {
      clearInterval(t);
      window.removeEventListener("teams-changed", load);
    };
  }, []);
  return roles;
}
