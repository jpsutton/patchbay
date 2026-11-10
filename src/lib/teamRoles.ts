import { useEffect, useState } from "react";
import { api } from "./ipc";
import { useStore } from "./store";

let cachedRoles: Record<string, string> = {};
const subscribers = new Set<(roles: Record<string, string>) => void>();
let pollTimer: ReturnType<typeof setInterval> | null = null;

const fetchRoles = () => {
  api.teamSessionRoles().then(
    (next) => {
      cachedRoles = next;
      for (const notify of subscribers) {
        notify(cachedRoles);
      }
    },
    () => {},
  );
};

const onTeamsChanged = () => {
  fetchRoles();
};

function startPolling() {
  if (pollTimer != null) return;
  fetchRoles();
  if (typeof setInterval !== "undefined") {
    pollTimer = setInterval(fetchRoles, 5000);
  }
  if (typeof window !== "undefined") {
    window.addEventListener("teams-changed", onTeamsChanged);
  }
}

function stopPolling() {
  if (pollTimer != null) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
  if (typeof window !== "undefined") {
    window.removeEventListener("teams-changed", onTeamsChanged);
  }
}

/** Role name by session id for sessions on live teams; refreshed on a timer and when the Teams page changes something. */
export function useTeamRoles(): Record<string, string> {
  const [roles, setRoles] = useState<Record<string, string>>(cachedRoles);
  useEffect(() => {
    subscribers.add(setRoles);
    if (subscribers.size === 1) {
      startPolling();
    } else {
      setRoles(cachedRoles);
    }
    return () => {
      subscribers.delete(setRoles);
      if (subscribers.size === 0) {
        stopPolling();
      }
    };
  }, []);
  return roles;
}

/**
 * Shared action: ask confirm, resolve teamId if needed, call api.teamRestartMember, and emit changes.
 * Returns true if restarted, false if cancelled by the user. Throws backend errors unchanged.
 */
export async function restartTeamMember(
  teamId: string | undefined,
  sessionId: string,
  role: string,
): Promise<boolean>;
export async function restartTeamMember(
  sessionId: string,
  role: string,
  teamId?: string,
): Promise<boolean>;
export async function restartTeamMember(
  arg1: string | undefined,
  arg2: string,
  arg3?: string,
): Promise<boolean> {
  let teamId: string | undefined;
  let sessionId: string;
  let role: string;

  if (arg3 !== undefined) {
    if (arg1 === undefined) {
      teamId = undefined;
      sessionId = arg2;
      role = arg3;
    } else if (arg2.length === 36 && arg2.includes("-") && (arg3.length !== 36 || !arg3.includes("-"))) {
      teamId = arg1;
      sessionId = arg2;
      role = arg3;
    } else if (arg3.length === 36 && arg3.includes("-") && (arg2.length !== 36 || !arg2.includes("-"))) {
      sessionId = arg1;
      role = arg2;
      teamId = arg3;
    } else {
      teamId = arg1;
      sessionId = arg2;
      role = arg3;
    }
  } else {
    teamId = undefined;
    sessionId = arg1!;
    role = arg2;
  }

  const { ok } = await useStore.getState().askConfirm({
    title: "Restart with fresh context?",
    body: `Start a new session for ${role} with a fresh context window. Its current open items and pending decisions will be briefed into the new session, and this session will be archived.`,
    confirmLabel: "Restart",
  });
  if (!ok) return false;

  if (!teamId) {
    const teams = await api.listTeams();
    for (const t of teams) {
      if (t.archivedAt != null) continue;
      const detail = await api.teamDetail(t.id);
      const found = detail.members.some((m) => m.member.sessionId === sessionId);
      if (found) {
        teamId = t.id;
        break;
      }
    }
  }
  if (!teamId) {
    throw new Error(`Team not found for session ${sessionId}`);
  }

  await api.teamRestartMember(teamId, sessionId);
  if (typeof window !== "undefined") {
    window.dispatchEvent(new Event("teams-changed"));
  }
  void useStore.getState().refreshCodeSessions().catch(() => {});
  return true;
}
