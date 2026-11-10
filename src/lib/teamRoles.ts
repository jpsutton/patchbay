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
  pollTimer = setInterval(fetchRoles, 5000);
  window.addEventListener("teams-changed", onTeamsChanged);
}

function stopPolling() {
  if (pollTimer != null) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
  window.removeEventListener("teams-changed", onTeamsChanged);
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
  sessionId: string,
  role: string,
  teamId?: string,
): Promise<boolean> {
  const { ok } = await useStore.getState().askConfirm({
    title: "Restart with fresh context?",
    body: `Start a new session for ${role} with a fresh context window. Its current open items and pending decisions will be briefed into the new session, and this session will be archived.`,
    confirmLabel: "Restart",
  });
  if (!ok) return false;

  let resolvedTeamId = teamId;
  if (!resolvedTeamId) {
    const teams = await api.listTeams();
    for (const t of teams) {
      if (t.archivedAt != null) continue;
      const detail = await api.teamDetail(t.id);
      const found = detail.members.some((m) => m.member.sessionId === sessionId);
      if (found) {
        resolvedTeamId = t.id;
        break;
      }
    }
  }
  if (!resolvedTeamId) {
    throw new Error(`Team not found for session ${sessionId}`);
  }

  await api.teamRestartMember(resolvedTeamId, sessionId);
  window.dispatchEvent(new Event("teams-changed"));
  void useStore.getState().refreshCodeSessions().catch(() => {});
  return true;
}
