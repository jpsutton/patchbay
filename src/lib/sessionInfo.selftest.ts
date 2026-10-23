/**
 * Pure-function checks for the session info rows and their text.
 * Run: node --experimental-strip-types src/lib/sessionInfo.selftest.ts
 */
import type { CodeSession } from "./ipc";
import { sessionInfoSections, sessionInfoText, type SessionInfoInput } from "./sessionInfo.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}

const session: CodeSession = {
  id: "local-1",
  cwd: "/Users/me/proj",
  acpSessionId: "saved-acp",
  title: "Fix the thing",
  modelRef: null,
  pinned: false,
  createdAt: Date.UTC(2026, 9, 2, 20, 48, 14),
  updatedAt: Date.UTC(2026, 9, 22, 23, 51, 58),
  activity: null,
  agent: "claude-code",
  agentModel: "saved-model",
  hostNode: null,
  epoch: 0,
  permissionMode: "auto",
};

const rowsOf = (i: SessionInfoInput, title: string) =>
  Object.fromEntries(sessionInfoSections(i).find((s) => s.title === title)?.rows.map((r) => [r.label, r]) ?? []);

const base: SessionInfoInput = { session, agentLabel: "Claude Code", hostName: null, isLocalHost: true };

// No live state: saved values, nothing invented.
{
  const s = rowsOf(base, "Session");
  assert(s["Patchbay session ID"].value === "local-1" && s["Patchbay session ID"].copy, "patchbay id is copyable");
  assert(s["Agent session ID"].value === "saved-acp" && s["Agent session ID"].copy, "falls back to the saved agent id");
  assert(s["Model"].value === "saved-model", "model falls back to the saved agent model");
  assert(s["Approvals"].value === "Auto edits", "approvals are named");
  assert(s["Status"].value === "Not running", "no state means not running");
  assert(s["Created"].value === "2026-10-02T20:48:14.000Z", "created is ISO");
  assert(!("Archived" in s) && !("Mode" in s), "absent fields leave no row");
  assert(rowsOf(base, "Environment")["Runs on"].value === "This computer", "local host");
  assert(!("Host node ID" in rowsOf(base, "Environment")), "no host node row without a host");
}

// A session the agent never started: no id to copy.
{
  const s = rowsOf({ ...base, session: { ...session, acpSessionId: null } }, "Session");
  assert(!s["Agent session ID"].copy && s["Agent session ID"].value.startsWith("none yet"), "unstarted session says so");
}

// Live state wins over saved values; options, grouped choices and builds are resolved.
{
  const i: SessionInfoInput = {
    ...base,
    session: { ...session, hostNode: "node-b", epoch: 2 },
    hostName: "Studio",
    isLocalHost: false,
    build: { program: "/usr/bin/claude-agent-acp", version: "0.93.0", source: "path" },
    state: {
      status: "ready",
      acpSessionId: "live-acp",
      agentInfo: { name: "claude-agent-acp", title: "Claude Agent", version: "0.93.0" },
      models: { availableModels: [{ modelId: "opus", name: "Opus" }], currentModelId: "opus" },
      modes: { availableModes: [{ id: "plan", name: "Plan" }], currentModeId: "plan" },
      configOptions: [
        {
          id: "effort",
          name: "Effort",
          type: "select",
          currentValue: "med",
          options: [{ group: "g", name: "G", options: [{ value: "med", name: "Medium" }] }],
        },
        { id: "other", name: "Skipped", type: "text", currentValue: "x", options: [] },
      ],
      capabilities: { loadSession: true, promptCapabilities: { image: true }, mcpCapabilities: { http: true } },
      extensions: { statusLine: false, rewind: false, steering: true },
      approvalGaps: ["Runs read-only shell commands."],
      usage: {
        totals: { input: 1200, cachedRead: 200, cacheWrite: 0, output: 30, reasoning: 0, costUsd: 0.5, apiMs: 0 },
        contextTokens: 1000,
        contextWindow: 200000,
        autoCompactPercent: null,
      },
    },
  };
  const s = rowsOf(i, "Session");
  assert(s["Agent session ID"].value === "live-acp", "live agent id beats the saved one");
  assert(s["Agent"].value === "Claude Code 0.93.0", "agent label and version");
  assert(s["Agent reports"].value === "Claude Agent", "agent's own name shows when it differs");
  assert(s["Model"].value === "Opus (opus)", "model shows name and id");
  assert(s["Mode"].value === "Plan (plan)", "mode shows name and id");
  assert(s["Effort"].value === "Medium (med)", "grouped config choice resolved");
  assert(!("Skipped" in s), "non-select options are left out");
  const env = rowsOf(i, "Environment");
  assert(env["Runs on"].value === "Studio" && env["Host node ID"].value === "node-b", "remote host shown");
  assert(env["Moves between computers"].value === "2", "moves counted");
  assert(env["Agent program"].value === "/usr/bin/claude-agent-acp (on PATH)", "program with its source");
  const agent = rowsOf(i, "Agent");
  assert(agent["Supports"].value === "resume, images, MCP over HTTP, mid-turn steering", "features listed");
  assert(agent["Runs without asking"].value === "Runs read-only shell commands.", "approval gaps listed");
  const usage = rowsOf(i, "Usage");
  assert(usage["Context"].value === "1,000 of 200,000 tokens", "context usage");
  assert(usage["Cost"].value === "$0.5000", "cost");

  const text = sessionInfoText(sessionInfoSections(i), Date.UTC(2026, 9, 22, 23, 51, 58));
  assert(text.startsWith("Patchbay session info (2026-10-22T23:51:58.000Z)\n\nSession:\n- Title: Fix the thing\n"), "text header");
  assert(text.includes("\n\nEnvironment:\n- Folder: /Users/me/proj\n"), "text sections are separated by a blank line");
  assert(text.includes("- Agent session ID: live-acp\n"), "text carries the raw id");
}

console.log("sessionInfo selftest ok");
