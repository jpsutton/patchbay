// Loaded into Cursor CLI's ACP server (`cursor-agent acp`) through NODE_OPTIONS=--require; see
// `agents::cursor_preload`. Cursor's ACP adapter drops AwaitShell calls: its tool presentation
// (`summarizeToolCall`) has no case for `awaitToolCall`, returns null, and every caller sends
// nothing for a call without a title. A turn blocked in AwaitShell then looks silent and stuck.
//
// Instead of patching Cursor's source text, this wraps the presentation object at run time, so a
// minified rename or a moved line doesn't break it. The wrapper only fills in calls Cursor itself
// would drop (its own answer is null or undefined), so once Cursor handles AwaitShell natively,
// this steps aside. Anything unexpected leaves Cursor's code as it was.
"use strict";

(() => {
  // Shells and other Node programs the agent starts must not load this file too.
  const orig = process.env.PATCHBAY_ORIG_NODE_OPTIONS;
  if (orig !== undefined) {
    if (orig) process.env.NODE_OPTIONS = orig;
    else delete process.env.NODE_OPTIONS;
    delete process.env.PATCHBAY_ORIG_NODE_OPTIONS;
  }

  // Only Cursor's ACP server: a preload that leaked into another Node process does nothing.
  if (!process.argv.slice(2).includes("acp")) return;

  const debug = !!process.env.PATCHBAY_CURSOR_PRELOAD_DEBUG;
  const log = (msg) => debug && process.stderr.write(`[patchbay preload] ${msg}\n`);
  const PATCHED = Symbol.for("patchbay.cursorAwaitPatched");

  /** The await call's arguments, or null when `tc` isn't one. */
  function awaitArgs(tc) {
    const tool = tc && tc.tool;
    const which = tool && tool.case;
    if (typeof which !== "string") return null;
    const args = (tool.value && tool.value.args) || {};
    // By name, or by its arguments should the case be renamed.
    if (/^await/i.test(which) || "blockUntilMs" in args) return args;
    return null;
  }

  function awaitTitle(args) {
    if (args.taskId) return `Await shell ${args.taskId}`;
    const ms = Number(args.blockUntilMs || 0);
    return ms > 0 ? `Wait ${Math.round(ms / 1000)}s` : "Await";
  }

  /** Protobuf messages as plain JSON (their int64 fields are BigInt, which JSON can't encode). */
  function plain(v) {
    if (v && typeof v.toJson === "function") return v.toJson();
    return undefined;
  }

  // Fill in what Cursor leaves out for an await call, and only that.
  const fills = {
    summarizeToolCall: (tc) => {
      const args = awaitArgs(tc);
      return args ? awaitTitle(args) : undefined;
    },
    extractToolCallInput: (tc) => (awaitArgs(tc) ? plain(tc.tool.value.args) : undefined),
    extractToolCallOutput: (tc) => (awaitArgs(tc) ? plain(tc.tool.value.result) : undefined),
  };

  function patch(obj) {
    if (!obj || typeof obj !== "object" || typeof obj.summarizeToolCall !== "function" || obj[PATCHED]) return false;
    for (const [name, fill] of Object.entries(fills)) {
      const orig = obj[name];
      if (typeof orig !== "function") continue;
      obj[name] = function (...a) {
        const own = orig.apply(this, a);
        if (own !== null && own !== undefined) return own;
        try {
          const ours = fill(a[0]);
          return ours === undefined ? own : ours;
        } catch (e) {
          log(`${name}: ${e}`);
          return own;
        }
      };
    }
    obj[PATCHED] = true;
    log("patched the ACP tool presentation");
    return true;
  }

  /** Look through a module's exports for the presentation object. */
  function scan(exports) {
    if (!exports || (typeof exports !== "object" && typeof exports !== "function")) return false;
    if (patch(exports)) return true;
    for (const key of Object.getOwnPropertyNames(exports)) {
      let v;
      try {
        v = exports[key];
      } catch {
        continue; // an export not initialized yet
      }
      if (patch(v)) return true;
    }
    return false;
  }

  // Cursor is a webpack bundle whose chunks load through require(): each returns
  // `{ ids, modules }`, a map of module factories. Wrap the factories whose source mentions the
  // presentation, and look at their exports once they have run.
  const Module = require("module");
  const require0 = Module.prototype.require;
  Module.prototype.require = function (...a) {
    const out = require0.apply(this, a);
    try {
      if (out && Array.isArray(out.ids) && out.modules && typeof out.modules === "object" && !out[PATCHED]) {
        out[PATCHED] = true;
        for (const [key, factory] of Object.entries(out.modules)) {
          if (typeof factory !== "function" || !String(factory).includes("summarizeToolCall")) continue;
          out.modules[key] = function (module, ...rest) {
            const r = factory.call(this, module, ...rest);
            try {
              // An async module may finish its exports a tick later.
              if (!scan(module && module.exports)) setImmediate(() => scan(module && module.exports));
            } catch (e) {
              log(`scan: ${e}`);
            }
            return r;
          };
        }
      }
    } catch (e) {
      log(`chunk: ${e}`);
    }
    return out;
  };
})();
