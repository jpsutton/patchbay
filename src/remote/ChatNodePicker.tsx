import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { MeshMember } from "../lib/ipc";
import { useStore } from "../lib/store";
import { ChipSelect } from "../components/ChipSelect";
import { MonitorIcon } from "../components/icons";

/**
 * The computer that makes a chat's model calls: the gateway unless the phone picks another one.
 * The pick is kept on the phone per chat ("new" until the chat exists).
 */
export function ChatNodePicker({ members, gateway }: { members: MeshMember[]; gateway: string | null }) {
  const key = useStore((s) => s.activeConversationId) ?? "new";
  const [node, setNode] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setNode(null);
    invoke<string | null>("remote_chat_node", { key }).then((n) => live && setNode(n ?? null), () => {});
    return () => {
      live = false;
    };
  }, [key]);

  // Online computers other than the gateway, as the new-session sheet lists them.
  const others = members.filter(
    (m) => m.role !== "remote" && m.nodeId !== gateway && (m.state === "direct" || m.state === "relay"),
  );
  const picked = node && node !== gateway ? members.find((m) => m.nodeId === node) : undefined;
  if (others.length === 0 && !picked) return null;
  const gatewayName = members.find((m) => m.nodeId === gateway)?.name ?? "Gateway";

  const choose = (v: string) => {
    const n = v || null;
    setNode(n);
    invoke("remote_set_chat_node", { key, node: n }).catch(() => {});
  };

  return (
    <ChipSelect
      lead={<MonitorIcon size={14} />}
      className="max-w-44"
      value={picked ? picked.nodeId : ""}
      onChange={(e) => choose(e.target.value)}
      title="The computer that runs this chat's replies, with its own provider keys"
      aria-label="Runs on"
      label="Runs on"
    >
      <option value="">{`${gatewayName} (gateway)`}</option>
      {others.map((m) => (
        <option key={m.nodeId} value={m.nodeId}>
          {m.name}
        </option>
      ))}
      {picked && !others.includes(picked) && <option value={picked.nodeId}>{`${picked.name} (offline)`}</option>}
    </ChipSelect>
  );
}
