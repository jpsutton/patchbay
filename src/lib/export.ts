import { save } from "@tauri-apps/plugin-dialog";
import { api, errorText, type AcpToolCall, type CodeSession, type Conversation, type Message, type Model } from "./ipc";
import type { Entry } from "./store";
import { useStore } from "./store";
import { fmtDateTime } from "./format";
import { isWeb } from "./platform";

export type ExportFormat = "markdown" | "json";

function fileStem(title: string): string {
  const s = title.replace(/[\\/:*?"<>|\u0000-\u001f]+/g, " ").replace(/\s+/g, " ").trim().slice(0, 80);
  return s || "patchbay-export";
}

/** Ask where to save, then write. Resolves true when a file was written. */
export async function saveExport(title: string, format: ExportFormat, contents: string): Promise<boolean> {
  const ext = format === "markdown" ? "md" : "json";
  if (isWeb()) {
    // The browser saves it, to its downloads folder or wherever it asks.
    const a = document.createElement("a");
    a.href = URL.createObjectURL(new Blob([contents], { type: format === "markdown" ? "text/markdown" : "application/json" }));
    a.download = `${fileStem(title)}.${ext}`;
    a.click();
    setTimeout(() => URL.revokeObjectURL(a.href), 10_000);
    return true;
  }
  try {
    const path = await save({
      title: "Export",
      defaultPath: `${fileStem(title)}.${ext}`,
      filters: [{ name: format === "markdown" ? "Markdown" : "JSON", extensions: [ext] }],
    });
    if (!path) return false;
    await api.writeTextFile(path, contents);
    useStore.getState().setToast(`Exported to ${path}`);
    return true;
  } catch (e) {
    useStore.getState().setToast(`Export failed: ${errorText(e)}`);
    return false;
  }
}

export function chatToMarkdown(conv: Conversation, messages: Message[], models: Model[]): string {
  const out = [`# ${conv.title}`, "", `_Exported from Patchbay on ${fmtDateTime(Date.now())}_`, ""];
  if (conv.systemPrompt) out.push("**System prompt**", "", ...conv.systemPrompt.split("\n").map((l) => `> ${l}`), "");
  for (const m of messages) {
    if (m.role === "user") {
      out.push(`## You · ${fmtDateTime(m.createdAt)}`, "");
      if (m.attachments.length) out.push(`Attachments: ${m.attachments.map((a) => a.name).join(", ")}`, "");
      out.push(m.content, "");
    } else {
      const model = models.find((x) => x.id === m.modelRef)?.displayName ?? "Assistant";
      out.push(`## ${model} · ${fmtDateTime(m.createdAt)}`, "");
      if (m.content) out.push(m.content, "");
      if (m.error) out.push(`> Error: ${m.error}`, "");
    }
  }
  return out.join("\n");
}

export function chatToJson(conv: Conversation, messages: Message[]): string {
  return JSON.stringify({ format: "patchbay.chat.v1", exportedAt: Date.now(), conversation: conv, messages }, null, 2);
}

function toolLine(tc: AcpToolCall | undefined): string {
  if (!tc) return "> Tool call";
  const status = tc.status ? ` (${tc.status})` : "";
  return `> **Tool:** ${tc.title || tc.kind || "tool call"}${status}`;
}

export function codeToMarkdown(session: CodeSession, entries: Entry[], toolCalls: Record<string, AcpToolCall>): string {
  const out = [`# ${session.title}`, "", `Folder: \`${session.cwd}\``, "", `_Exported from Patchbay on ${fmtDateTime(Date.now())}_`, ""];
  for (const e of entries) {
    switch (e.kind) {
      case "user":
        out.push("## You", "");
        if (e.attachments?.length) out.push(`Attachments: ${e.attachments.map((a) => a.name).join(", ")}`, "");
        out.push(e.text, "");
        break;
      case "agent":
        out.push(e.text, "");
        break;
      case "image":
        out.push(`![Image from the agent](data:${e.mimeType};base64,${e.data})`, "");
        break;
      case "thought":
        out.push("<details><summary>Thinking</summary>", "", e.text, "", "</details>", "");
        break;
      case "tool":
        out.push(toolLine(toolCalls[e.id]), "");
        break;
      case "error":
        out.push(`> Error: ${e.text}`, "");
        break;
      case "info":
        out.push(`_${e.text}_`, "");
        break;
      // Not prompts: the harness's own messages stay folded, a cancel is a note.
      case "harness":
        out.push("<details><summary>Added by the agent</summary>", "", "````", e.text, "````", "", "</details>", "");
        break;
      case "cancelled":
        out.push("_Turn cancelled_", "");
        break;
    }
  }
  return out.join("\n");
}

export function codeToJson(session: CodeSession, entries: Entry[], toolCalls: Record<string, AcpToolCall>): string {
  return JSON.stringify({ format: "patchbay.code.v1", exportedAt: Date.now(), session, entries, toolCalls }, null, 2);
}
