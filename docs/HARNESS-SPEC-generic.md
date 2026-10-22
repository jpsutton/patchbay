# Agentic Coding CLI — Harness Reference Architecture

A vendor-neutral specification of the prompt/harness subsystem used by a terminal-based agentic coding assistant: the agent personas, the logic that selects them, how the system prompt is assembled and cached, the built-in tool definitions, and the runtime-injected context blocks.

This is a generalized reference derived from a single-file (Bun-compiled) CLI binary. It describes the **purpose and structure** of each prompt and prompt fragment and omits original prompt wording. Service-specific machinery of the original (billing/attribution headers, vendor telemetry, vendor hosting providers) has been removed; what remains is the harness architecture that any comparable agentic CLI could adopt.

Internal identifiers in parentheses (e.g. `CKn`, `GPo`) are the reference implementation's symbol names, kept as anchors; they are neutral labels, not part of any required design.

---

## 1. Harnesses (top-level personas)

A *harness* is a root-level agent persona plus the conditions that select it. The CLI ships one persona triad for the ordinary agent, a separate coordinator persona for multi-worker orchestration, and a family of child personas for spawned agents.

### 1.1 Core persona triad

Three base persona strings, collected into a set used later for cache tagging. Each asserts a slightly different identity:

| ID | Identity it asserts | When used |
|----|---------------------|-----------|
| `nl` | The official CLI agent | Default interactive terminal session; also forced under a hosted-provider variant. |
| `zd` | The CLI agent **running within the embeddable SDK** | Non-interactive session that also carries a user-supplied appended system prompt. |
| `qd` | A generic agent **built on the SDK** | Non-interactive / programmatic use (print mode, SDK) with no appended prompt. |

### 1.2 Base-persona selector (`CKn`)

Evaluated in strict order; first match wins:

1. Hosted-provider variant active → `nl`.
2. A recorded prefix is present → return it verbatim (record/replay test path).
3. Non-interactive session → `zd` if an appended system prompt was supplied, otherwise `qd`.
4. Otherwise → `nl`.

**Input meanings**
- *Provider/platform resolver* — abstracts which backend the CLI talks to. Only the hosted-provider variant affects the persona; all other backends leave it at the default.
- *Non-interactive session* — headless mode: print mode, SDK-driven, CI/cloud runs with no live TUI.
- *Appended-system-prompt flag* — the user supplied an extra system prompt to layer on top.
- *Recorded prefix* — a pre-recorded system-prompt prefix from the record/replay harness; returned as-is when set.

### 1.3 Coordinator harness (multi-worker orchestration)

- **Builder:** a coordinator-prompt builder (`getCoordinatorSystemPrompt`) with a companion user-context builder. Persona: a *coordinator* that orchestrates software-engineering tasks across multiple workers and delegates rather than coding directly when it can.
- **Trigger:** a coordinator-mode predicate (`da`): a coordinator-mode config flag must be set, and a secondary gate disables it in certain local/non-remote contexts.
- **Wiring:** in the root prompt builder, when coordinator mode is on and no custom prompt is set, the coordinator prompt replaces the normal base and the session is tagged `coordinator` vs `normal`; extra tool/context blocks are pushed.
- **Phrasing input:** the builder takes whether a cross-session peer-messaging capability is present, flipping wording between "every message goes to the user" and peer-aware variants.
- **Modifiers:** config flags can force workers to inherit the session model (substituting a "the model parameter is ignored" instruction). A spawn-depth cap controls whether workers themselves receive the subagent-spawning tool (depth > 1).

### 1.4 Worker / fork child personas

| Persona | Role |
|---------|------|
| Worker agent | Executes a task assigned by the coordinator; has Environment / Scope / Resumed-Tasks sections and commit discipline; may fan out further only below the depth cap. |
| Worker fork | Inherits the parent transcript as reference, executes exactly one directive, then stops. Hard rule: must not spawn its own subagents. |

### 1.5 General subagent persona

- **Base string** (`mhs`): an agent *for* the CLI. Its guideline body covers absolute paths, no emojis, not writing report files, and the fact that it is already the dedicated agent for its task.
- **Delegation-guidance builder** (`k_s`): assembles the "available modes" guidance appended to a subagent prompt. Flags are keyed off coordinator mode, whether fork / general-purpose / continue agent types are registered, and lean-prompt / model-mix experiment flags.

### 1.6 Specialized subagent personas

Each has its own persona, selected by subagent type or by a specific feature. These are child agents, not root harnesses:

- File-search specialist — fast file locator.
- Web-reading specialist — the only child agent granted web fetch.
- Software architect / planning agent.
- Lightweight one-shot query agent — single response.
- Status-line setup agent.
- Conversation summarizer — compaction.
- Memory-extraction / background-fork agents.
- SDK generic agent variants.

Numerous task-scoped evaluator / monitor / judge prompts also exist (security monitor, hook evaluators, auto-mode classifier, trace judges, test runner/author, scheduler, self-hosted-runner guides, design tool, artifact composer/analyst). They are not top-level harnesses.

### 1.7 Surface / environment descriptors (modifier layer)

A set of literals describing *where* the harness runs, appended to whichever base persona was chosen. They tune output expectations (live-watching vs logs, phone brevity, etc.) without changing the agent's identity:

terminal CLI, editor extension, web, mobile app, scheduled/triggered run, CI pipeline, collaborative workspace, embeddable SDK / print mode, remote sandbox, chat integrations, desktop app.

### 1.8 Root-prompt assembly control flow

1. Builder checks for a custom/override prompt first.
2. Else, if coordinator mode is on and no override → coordinator harness.
3. Else the normal base: the selector chooses among `nl` / `zd` / `qd` / recorded by the strict order in §1.2.
4. The chosen persona is wrapped by the block builder and tagged for caching by the cache-scope assembler (§2).
5. The surface descriptor and other dynamic sections are appended as context.
6. Child agents get the general subagent string, a worker/fork persona, or a specialized persona, with the delegation-guidance builder supplying mode guidance.

---

## 2. System prompt assembly and caching

### 2.1 System-block builder (`PNs`)

Produces the raw system-prompt block-string array, ordered:

`[ base persona, ...caller-supplied blocks ]`

Falsy entries are dropped. Caller-supplied blocks include tool guidance, the reporting-outcomes block (§2.2), memory / project-instruction file contents, and so on. A dynamic-boundary sentinel may sit among them (§2.3).

### 2.2 Reporting-outcomes block (`gsn`)

A fixed honesty / truthful-reporting block (report failures with evidence, state skipped steps, don't over-claim). Classified separately by the cache assembler so it gets its own cache treatment.

### 2.3 Cache-scope assembly (`mxo`)

Converts the block-string array into `{text, cacheScope}` objects and places prompt-cache breakpoints. A sentinel string (`__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__`) marks the static|dynamic cut. Blocks are classified into persona (member of the persona set), reporting-outcomes, and everything else (newline-joined).

Breakpoint placement depends on whether tool-based caching is enabled and whether the boundary sentinel is present:

- **Tool-based cache on, no boundary:** persona → *shared* scope, remaining blocks → *shared* scope; variable content uncached.
- **Tool-based cache on, boundary present:** split the remaining blocks at the sentinel — static pre-boundary join → *global-shared* scope, dynamic post-boundary join → *shared* scope; persona and reporting-outcomes uncached at this breakpoint.
- **Fallback (cache off, or on with no boundary):** persona → *shared* scope, remaining blocks → *shared* scope.

**Cache-scope meaning:** uncached = no breakpoint (per-request-variable content); *shared* = cache keyed to the account/workspace; *global-shared* = a broadly shared cache, applied only to the static pre-boundary segment. The sentinel is the mechanism for placing the split between globally-stable and session-variable prompt content.

### 2.4 Environment attachment (cwd / git / platform / date / model)

Built as an `environment` attachment on a **user message**, not the system array. Fields: primary working directory, worktree notes, is-git-repo, additional dirs, platform, shell, OS version, scratchpad dir, git status, VCS-CLI presence, the model identity, and today's date, closed by a disclaimer that this context was auto-attached and is not part of the user's message.

> Removed from this version: a telemetry/billing header fragment that the reference implementation prepended to the system array and, in some cases, to the first user message; and git commit/PR attribution footers. Both are tied to the originating service and are not part of the generalizable harness.

---

## 3. Built-in tools

Each tool ships two description variants — a terse **lean** form and a verbose **classic** form — selected at runtime by model capability; newer model bundles get lean. Many descriptions are assembled from fragments, so exact wording varies with sandbox mode, bash-first steering, remote/cloud, and OS.

Only the behavioral intent of each description is summarized; raw wording is not reproduced.

### 3.1 Filesystem

**Read** — reads a file; renders images visually, PDFs via a `pages` range, notebooks as cells+outputs. Rules: absolute path required; read only the needed part of large files; PDFs over 10 pages require `pages` (max 20/request); directory/missing/empty reads return an error or reminder; don't re-read a file just edited (the harness tracks state).

**Write** — creates or fully overwrites a file. Rules: overwriting a file not yet Read fails; prefer Edit for partial changes; don't create docs/README unless asked; no emojis unless asked.

**Edit** — exact string replacement. Rules: must Read first; preserve exact indentation and strip the line-number prefix from match text; non-unique match fails unless `replace_all`; prefer editing existing files.

**Glob** — glob file matching, results sorted by mtime; defers to the agent tool for open-ended multi-round search.

**Grep** — ripgrep-backed content search. Rules: always use this instead of shell `grep`/`rg`; ripgrep regex semantics (escape literal braces); filter by `glob`/`type`; `multiline` for cross-line; `output_mode` of content / files_with_matches / count.

**NotebookEdit** — replace/insert/delete one notebook cell. Must Read the notebook first; insert places after a given cell id.

### 3.2 Shell

**Bash** — runs a shell command, returns output. Rules: working dir persists, shell state does not; shell starts from the user profile. Avoid using it for `find`/`grep`/`cat`/`head`/`tail`/`sed`/`awk`/`echo` — prefer the dedicated tools. Quote spaced paths; avoid `cd` (use absolute paths; never prepend `cd` to git). Prefer new commits over amend; avoid destructive git (`reset --hard`, `push --force`) and `--no-verify` unless asked. Don't sleep between runnable commands; use `run_in_background` for long tasks; a long leading `sleep` is blocked (poll via Monitor with an `until`-loop). Params: `timeout` (ms, capped), `run_in_background`, `description`. Bash-first steering changes which "avoid X" lines and `find` tips appear.

**TaskStop** / KillShell — stops a running background task, teammate, or named agent by ID/name.

**TaskOutput** / BashOutput — reads output from a background shell/agent task; largely superseded here by the Monitor/notification pipeline.

**Monitor** — streams events from a background command or WebSocket; each stdout line / frame becomes a notification; exit or socket close ends the watch. Rules: for one-shot "wait until done" use Bash `run_in_background` instead; don't poll/sleep; an event is not the user's reply; stop a persistent monitor with TaskStop. Params: exactly one of `command` / `ws`; `description`, `timeout_ms` (re-arm on expiry), `persistent`, `allowed_domains`. Gated by background-feature flags.

### 3.3 Web

**WebFetch** — fetches a URL, converts HTML to markdown, answers a prompt about it with a small model. Rules: fails on authenticated/private URLs and on localhost/dotless hosts; HTTP upgraded to HTTPS; cross-host redirects are returned rather than followed (call again); responses cached per URL (TTL configurable); prefer connector web tools / VCS CLI; carries a short quote-limit copyright rider.

**WebSearch** — web search for post-cutoff info. Rules: a "Sources:" section of markdown links is mandatory after answering; use the current year; region-limited; `allowed_domains` / `blocked_domains`; wildcards rejected.

### 3.4 Agents / tasks / orchestration

**Task** (the agent-spawning tool) — spawns a subagent by type for delegated/parallel work. Rules: delegate for parallelism and non-blocking side-quests; non-fork subagents start fresh (give them context and file pointers); fork inherits the parent's full context/model (model override ignored for forks); subagents run in the background with completion notifications — don't fabricate results from unfinished agents; the subagent's final report is shown only to the caller; for single-fact lookups search directly. Params: `prompt`, `description` (3–5 words), `subagent_type`, `name` (makes it addressable), `model`, `isolation` (worktree). Nesting capped by a spawn-depth limit.

**Workflow** — runs a background multi-agent orchestration script. Rules: call only when the user has explicitly opted into multi-agent orchestration (keyword, session flag, explicit request, a skill, or a named/saved workflow); otherwise use Task. Scripts must begin with a pure-literal `meta` export. Gated by a feature flag and a session size guideline.

**TaskCreate / TaskUpdate / TaskGet / TaskList** — agent-team task tracker tools.

**SendMessage** — sends a message to another agent (teammate, named/background agent, or the main session). Rules: recipient sees only the first line as a preview; cross-machine routing goes through a central relay (blocked on third-party backends); idle-notification only from the main conversation. Params: `to`, `message`, `summary` (5–10 words), `attachments`, idle-notify flag.

**SendUserMessage** — delivers a message to the user with attachments.

**ScheduleWakeup** — schedules resumption in loop/dynamic mode; a `prompt` keeps the loop alive, `stop:true` ends it; delay clamped to [60, 3600] s.

**EnterWorktree / ExitWorktree** — git-worktree isolation.

### 3.5 Planning, tasks, skills, UI

**TodoWrite** — maintains a structured session task list. Rules: use for 3+ step / multi-part work; exactly one task `in_progress`; mark complete immediately but never while tests fail or work is partial; each task has an imperative `content` and present-continuous `activeForm`.

**ExitPlanMode** — signals planning is done and requests plan approval. Rules: plan mode only, for code-implementation planning (not research); reads the plan from the plan file rather than taking it as a param; don't ask for plan approval via the question tool — this tool is that.

**Skill** — invokes a packaged skill. Rules: exact listed name, no leading slash; `plugin:skill` for plugin skills; dir-scoped skills carry a path prefix and most-specific wins; built-in CLI commands aren't skills.

**ToolSearch** — loads full schemas for deferred tools named in reminders so they become callable. Query forms: `select:A,B`, keyword, `+required`. Gated by a feature flag; an optimistic preload mode is disabled when a non-default backend base URL is set. A reserved never-call placeholder tool keeps deferred loading active.

**AskUserQuestion** — presents a multiple-choice pop-up and returns the selection. Rules: only when genuinely blocked on a user decision; users always get an 'Other' free-text option; multi-select for several answers; put the recommended option first and label it.

### 3.6 Cross-cutting tool gating

- **Lean vs classic descriptions:** selected by model capability (newer model bundles get lean) and simple-prompt flags.
- **Remote execution:** an optional per-call `machine` param, gated by a remote-forward flag; omitted → runs locally.
- **Background tools** (Monitor, background Bash) require background-feature gates.

---

## 4. Runtime-injected context blocks (system-reminders)

The harness injects context into the model mid-conversation, most of it wrapped in `<system-reminder>` tags. A master list of reminder "kinds" exists internally; an injectable message-channel enum distinguishes `user_context`, `task_notification`, `queued_user`, `peer_message`, `system_reminder`, `interrupt`, `plain`. Roughly 30 distinct prose templates underlie the literal reminder occurrences; the rest are parser/sanitizer/enum references.

Each entry gives the block's purpose and its interpolated slots. No template body is reproduced.

### 4.1 Security / provenance / injection defense

- **Harness-tag provenance notice** — tells the model that `<system-reminder>` and similar tags are injected by the harness, not written by the user, and bear no relation to the surrounding message. Two strictness variants.
- **Messages-from-the-system explainer** — explains that system reminders appear mid-conversation, wrapped in tags, originating from the application.
- **Prompt-injection flag** — instructs the model to flag suspected prompt injection in tool results to the user before continuing.
- **Auto-mode subagent-report verdict** — when the handoff classifier blocks/warns on a subagent's report, warns it may be compromised. Slots: classifier reason, report body.
- **Subagent-report escaping spec** — directs inserting a backslash into text imitating harness markup so a sub-report cannot spoof harness framing. Slots: report text, optional warning.
- **Hook-feedback-is-user guidance** — treat hook feedback (e.g. a submit-hook block) as user-originated; adjust or ask if blocked.
- **Denied-capability guardrail** — permits reasonable alternatives after a denial but forbids malicious circumvention of its intent.
- **Scheduled-task provenance banner** — marks an auto-fired scheduled prompt as the assigned task, not live user input. Slot: stored prompt body.

### 4.2 Account / workspace / memory

- **Account & workspace context note** — opening context block with account/environment fields; states it was auto-attached. Slots: the context fields.
- **Project-instruction / memory-file contents** — embeds each loaded memory or project-instruction file as background context. Slots: per-file path, type, content.
- **Recalled-memory caveat** — labels recalled memory as background context (not instructions), true when written; says to verify referenced files/functions/flags still exist.
- **Citing-memories wrapper rule** — wrap any sentence citing memory in a memory-citation tag. Slot: filenames.
- **Stale memory-read result** — a cached memory read is older than the file; re-read before trusting it.
- **Memory-file write reject** — refuses writing a memory file containing invisible/control chars or harness-tag-shaped text. Slots: offending snippets.

### 4.3 File freshness / staleness

- **File already in context, unchanged** — reuse existing content instead of re-reading. Slot: file path.
- **Redundant re-Read** — refer to the earlier tool result.
- **File modified since read** — blocks a write/edit when the file changed since it was read; re-read first. Slot: host id, line count.
- **Notebook modified since read** — same guard for notebooks.
- **Empty-file warning** — the file exists but is empty.
- **Offset-past-end warning** — the Read offset exceeds length. Slots: offset, total lines.
- **Code-index staleness** — a repo map N days old; cached behavior claims / file:line citations may be outdated. Slot: age.

### 4.4 Todo / plan / task state

- **TodoWrite inactivity reminder** — nudge to track/clean tasks on multi-step work; appends the current todo list. Slot: todo list.
- **Task-list reminder** — nudge to create/resolve tasks in team/task mode; appends the task list. Slot: `#id [status] subject` list.
- **Plan-approved transition** — after plan approval, may start coding and update the todo list; notes where the plan was saved. Slots: saved path, plan text.
- **Plan-mode state-change block** — a state-changing/approval-requiring call attempted from plan mode is not approved. Slot: tool name.

### 4.5 Context-management / lifecycle

- **Compaction summarization instruction** — directs producing a detailed `<summary>` that will replace prior context. Slots: word-cap flag, keep-earlier-asks flag.
- **Compaction-replaces-context note** — the `<summary>` reply replaces everything above.
- **Session-continued-from-summary banner** — prior conversation was summarized/withheld; state may have changed. Slot: updated cwd (cross-machine variant).
- **Context auto-compression note** — prior messages auto-compress; the window doesn't limit the conversation.
- **Container-restarted notice** — background tasks were lost and may need recreating; lists tasks that finished before the restart without delivering. Slot: affected task list.
- **Orphaned background-commands notice** — earlier background commands are no longer running here; check their output before relying on them. Slot: command list + return timestamps.

### 4.6 Agents / teams / notifications

- **Team-coordination block** — supplies the agent's identity and team resources; directs updates to the team lead. Slots: agent name, team config path, optional task-list path.
- **Non-interactive team-shutdown requirement** — a non-interactive session with an active team must shut it down before the final response.
- **Worker/task-notification framing** — explains that worker results arrive as a user-role `<task-notification>` inside a `<system-reminder>` and must not be reproduced. Slots: agent id, status, summary, result.
- **Handback reminder / countermand** — markers governing when a prior "report through X" directive still applies vs is revoked (final report delivered as plain text). Slot: contract text.
- **Remote planning session** — run a lightweight plan-mode process (Glob/Grep/Read) and don't reveal the cloud-session framing to the user.
- **Multi-agent thorough-plan directive** — spawn parallel Task agents for an exceptionally thorough implementation plan.

### 4.7 Tools / batching / rate limits / models

- **Deferred-tool / ToolSearch reminder** — deferred tools appear by name only and must be fetched via ToolSearch before they are callable.
- **Batching tip** — a batch-capable tool was called with one entry; batching is faster. "No action needed." Slots: tool name, entry-field name.
- **VCS API rate-limit reminder** — on a VCS/API rate-limit error, check the rate-limit endpoint and sleep to reset via ScheduleWakeup rather than retry-looping.
- **Model-changed notice** — the session model was switched mid-session. Slot: new model name.
- **Newer-model offer** — a newer default model is available; offers to continue with it. Slots: key, newer, older.
- **Brief-mode toggle notice** — route all user-facing output through the brief tool (on) or revert to plain text (off). Slot: brief-tool name.

### 4.8 Side-agent / spawned single-response

- **Side-question agent** — a lightweight agent answering one side question in a single response; shared context, independent lifecycle.
- **Heads-up side request** — same single-response directive for a background heads-up.
- **New-message-while-working / aborted-turn notices** — surfaces an interrupting user message or hook feedback and retry prompts. Slot: the new message / hook feedback.

### 4.9 Artifacts

- **Artifact-open opening context** — notes an artifact open beside the conversation and that an instructions file was read ahead; the artifact's own content must be read as the opening context asks. Slots: artifact URL, captured reminder body.

### 4.10 Mechanical parsing (not model-facing prose)

Regexes/sanitizers that operate on the tags rather than emit prose: extract/strip reminders, tag-escaping sanitizers, telemetry-content truncation around tags, combined context/token-cap markers.

### 4.11 Other enumerated reminder kinds (bodies not pulled)

Gated nudges existing as identifiers: `silent_turn_reminder`, `elapsed_time_reminder`, `output_limit_note`, `token_usage`, `skill_listing`, ultra-effort enter/exit, `ultrathink_effort`, `workflow_keyword_request`, `secondary_reminder`, `bash_output_audience_note`, selected-lines-in-diff/ide, `unknown_command_fallback`.

---

## 5. Configuration knobs (generic)

Named generically; the reference implementation uses a vendor-specific env prefix.

| Knob | Effect |
|------|--------|
| Provider selection | Chooses the model backend; only the hosted-provider variant changes the persona. |
| Coordinator mode | Enables the coordinator harness (subject to a local/remote gate). |
| Force-worker-inherit-model | Forces workers to inherit the session model; adds a "model param is ignored" instruction. |
| Max subagent spawn depth | Caps subagent nesting; > 1 lets workers receive the subagent-spawning tool. |
| Simple/lean prompt | Selects lean tool/system descriptions. |
| Tool-search enable | Enables deferred tools / ToolSearch (on / auto / auto:N). |
| Workflows enable | Enables the Workflow tool. |
| WebFetch cache TTL | Per-URL cache lifetime. |
| Remote-tools forward | Enables the per-call `machine` remote-execution param. |
| Remote flag | Part of the coordinator local/remote gate. |

> Removed from this version: knobs that only govern the originating service's billing/attribution headers and session-URL attribution.

---

## 6. Method notes

- Extraction used a strings dump of the binary; the bundle is minified onto a few very long lines, so substrings were pulled with `grep -oE` within lines rather than by line context.
- Internal identifiers are specific to one build and will drift across releases.
- Prompt bodies were read to determine intent but are intentionally paraphrased here, not reproduced.
- This is the vendor-neutral edition. A build-specific edition with provider names, line numbers, exact environment-variable names, and the billing/attribution subsystem exists alongside it.
