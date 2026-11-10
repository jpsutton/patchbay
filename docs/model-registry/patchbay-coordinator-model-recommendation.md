# Coordinator model: cheap coordinator, strong seats

Recommendation for the Patchbay triage desk (hand-run today) and the Teams
implementation (`~/patchbay-teams`, branch `feat/teams`). Written 2026-11-10
from DESK.md, ROSTER.md, the ledger, `docs/design/teams.md`, and the phase-4
security review. The databases (`teams.db`, `patchbay.sqlite`) were not
queried — `.sqlite` files cannot be copied off the Mac and there is no shell
access from here — so this rests on the written design and the desk's own
record, not on live DB state.

## Terminology update (2026-11-10, voice)

David endorsed **"reasoning capacity"** as the working term for the
tier labels — the roster assigns by reasoning capacity, not model
name, so providers swap without editing templates. It replaces
"capability" for this purpose: "capability" collides with the
benchmark/model-menu meaning (multimedia in/out, audio, JAV/CLEF-type
decision models), while the distinction here is reasoning capacity —
Fable/Argon reason far beyond read-a-line/write-a-line/send-a-message.
Not yet written into any template.

## Verdict

**Keep the coordinator cheap-to-mid. Do not put Claude Fable 5.2 in the
coordinator seat.** The strong models belong at the judgment seats and as a
per-item worker escalation, never as the coordinator default.

- Hand-run desk today: coordinator stays `opencode-go/mimo-v2.6-flash`.
- Teams built-in Triage desk template: coordinator is Claude Code Sonnet —
  that is already the template default. Confirm it; do not change it.
- Fable 5.2 (and Opus) live at the Advisor seat, at Ship Lead for complex
  reviews, and as a named per-item escalation for workers.
- The "long session" argument does not favour a strong coordinator: the
  coordinator does not run long sessions — workers do. Longevity comes from
  the ledger / `teams.db` plus checkpoint-and-resume, not from a bigger
  context window.

## Terminology — "reasoning tier", not "reasoning capacity"

Correction to the 2026-11-10 voice call, where "reasoning capacity" was
endorsed as the sharper term: it is **not an industry term**. No provider,
benchmark, or standard defines it. It is intelligible and narrower than
"capability" — which provably also covers modalities, tool use, context,
and languages in provider docs — but it is our word, not the industry's,
and it will not map onto provider docs when a new model ships. Use it
only as a defined internal term, never as if it were standard.

What the industry does define:

- **Reasoning model** (OpenAI, documented): a model that uses internal
  reasoning tokens before responding.
- **Reasoning effort / effort / thinking level** (OpenAI `reasoning.effort`,
  Anthropic `effort`, Google `thinking_level`, DeepSeek `reasoning_effort`):
  the per-request dial — none/minimal/low/medium/high/xhigh/max. This is
  the dimension you actually set.
- **Test-time compute** (Snell et al., arXiv:2408.03314): the research term
  for the mechanism — spending more compute per prompt can beat a 14×
  larger model. Correct in design docs; not a product/tier label.

**Use "reasoning tier" internally, defined as a measured (pinned model,
effort) pair** — not a model name. A tier is {pinned model ID, effort
setting, benchmark evidence (suite + version + harness + date + score),
cost/latency, role}. `reasoning_capacity` may be the *score* behind a
tier, never the tier name. This is the vocabulary the registry below
uses, and it is what the appendix table means by "tier".

## Why — the evidence

1. **The coordinator's work is structured, not judgment.** Intake, classify
   type/severity, write a one-line outcome, append a ledger row, write a
   self-contained brief, dispatch, update status, spot-audit. In Teams this
   is enforced by capability flags (`writes_items`, `dispatches`) checked by
   the tools, not by trusting the model's instructions. Checklist work does
   not get better with a flagship model; it gets more expensive.
   **But the routing decision itself is judgment, and it is the failure
   point.** Routing research is explicit that the quality estimator drives
   most of the result: a coordinator that cannot tell hard from easy, or
   cannot judge a worker's output, silently ships weak answers or wastes
   money, and a cost-only router "collapses quality nobody approved".
   The cheap seat must therefore still be able to assess difficulty and
   verify outputs reliably; if it cannot, the fix is a stronger
   coordinator (or a dedicated judge), not more workers. Claude Code's own
   pattern is the reverse split — `opusplan` puts Opus on planning and
   Sonnet on execution — which is the shape to copy for planning-heavy
   work, not the cheap-coordinator default.
2. **Judgment is routed away from the coordinator by design.** Scope,
   priority, and whether a proof gap blocks a merge are `advisor_first` —
   they go to the Advisor (Claude Opus today) before they reach David. If
   the coordinator were the strong model, that routing would be pointless.
   If the advisor becomes the bottleneck, the fix is a second
   advisor-class seat or clearer routing, not a stronger coordinator.
3. **The 2026-11-10 leak is the cautionary tale.** A seat spawned without
   an explicit model silently inherited Claude Fable 5.2 from its
   orchestrator; David caught it, corrected it to Sonnet, and compacted its
   context. Making Fable the coordinator default bakes that leak in as
   policy instead of treating it as the accident it was.
4. **Context is a structural cap, not a model property.** The desk's rule is
   a 150,000-token budget; past it, checkpoint the ledger and start a fresh
   session that resumes from the ledger. A stronger model does not remove
   the checkpoint — state lives in the ledger, not in context. Fable would
   just make each turn before the checkpoint cost more.
5. **Cost scales with volume.** The desk does high-volume small operations
   (every intake, every dispatch, every status update). The recorded cost
   of the cheap desk session was ≈ $1.47 over two days. Flagship rates on
   every one of those turns buys nothing the advisor does not already
   provide.
6. **The hand-run desk's agent cannot be the Teams coordinator as-is.**
   The desk runs on OpenCode/mimo. In Teams, an OpenCode member waits on a
   permission prompt for *every* team-tool call — there is no allow rule
   for it, because its MCP access runs through an `execute` tool that would
   allow arbitrary code. An unattended coordinator must therefore be a
   hook agent (Claude Code, Cursor, Codex, Antigravity). Sonnet on Claude
   Code is the right default there: capable enough for triage, cheap
   enough for dispatch volume, and covered by the allow rule.

## Model assignments

| Seat | Hand-run desk today | Teams built-in template | Notes |
|---|---|---|---|
| Coordinator | `opencode-go/mimo-v2.6-flash` | Claude Code, Sonnet | Keep. No change in either place. |
| Advisor | Claude Opus (David's session) | Claude Code, Opus | The strong-model seat. Fable 5.2 is the alternate here, not at coordinator. |
| Ship Lead | David's session (model unrecorded) | Claude Code, Sonnet | Fable on request for a diff too large or tangled for Sonnet's review. |
| Workers | `claude-code` / `sonnet` (default) | Pool of 4: Claude Code Sonnet, Cursor Auto, Antigravity Gemini, OpenCode | Fable only as a per-item escalation (below). |

## When Fable 5.2 *is* the right call

Escalation only, decided per item, never by default or by inheritance:

- P1 with an ambiguous root cause, a worker bounced twice on the same
  item, or Ship flags a diff too large to review in Sonnet's context.
- The coordinator opens an `advisor_first` decision; advisor CLEAR
  approves Fable for that item; the dispatch writes the exact agent and
  model; the worker's first reply states the agent+model it is actually
  running and the coordinator compares it before work starts (the
  dispatch agent+model rule, DESK.md 2026-11-10).
- Never by silent inheritance; never as a seat default; never because the
  item "seems hard" without the advisor's CLEAR.

### Escalation across providers — Fable is not the only strong seat

The escalation tier exists in more than one provider. Order of
preference, cheapest-headroom first:

1. **Antigravity Gemini Argon (`gemini-4-argon`) — first choice.**
   Trialled 2026-11-07 as a strong model for the desk, and Antigravity's
   quota response carries a dedicated Argon weekly bucket (see
   `docs/design/agents.md`, Antigravity plan usage). It is plan/quota
   based, and its utilization is not full — that headroom is the reason
   to spend it before spending billed flagship tokens.
2. **Claude Fable 5.2 — second choice.** Use it when Argon is exhausted
   for the week, or when the work plays to Claude-specific strengths
   (a Claude-Code-shaped task, or a diff the Claude reviewer needs to
   own end to end).
3. **Codex GPT-5 / Astra — candidate, not yet verified.** Codex is
   installed and its model list/switch path is verified in the
   `agents_live` tests, but no GPT-5 or Astra model id appears in the
   desk record, the roster, or the Teams docs. Before it joins the
   escalation list, confirm the exact model id in a Codex session's
   model picker and record it in the table below. Do not dispatch on a
   guessed id.

Escalation workers should be hook agents (Claude Code, Codex,
Antigravity, Cursor) — an OpenCode member waits on a permission prompt
for every team-tool call, including `item_note`, so an OpenCode
escalation worker stalls on reporting even if it can do the work.

## Industry alignment — what the wider world does (researched 2026-11-10)

- **Provider tiers, Nov 2026 snapshot.** Every major provider tiers 3–4
  rungs on the same axis — flagship / balanced / light — with roughly
  10–100× price spread. Anthropic: Fable 5.2 ("demanding reasoning and
  long-horizon agentic work", $8/$40 per MTok in/out), Opus 5.5 ($4/$20),
  Sonnet 5.5 ("best combination of speed and intelligence", $2/$10),
  Haiku 5.5 (from $0.10/$0.50, "classification, extraction, and
  routing") — all 1M context. OpenAI: GPT-6 Astra leads, GPT-5.6
  Terra/Luna cheaper; GPT-5 nano $0.05/$0.40 up to pro $30/$180.
  Google: Gemini 4 Argon top, then Pro, Flash, Flash-Lite. Prices are a
  Nov 2026 snapshot — re-check the provider pricing page before
  budgeting; GPT-6 and Argon prices could not be confirmed from the
  providers' own pricing pages in this pass.
- **The gap between rungs is narrowing.** Sonnet 5.5 sits within ~2
  points of Opus 5.5 on Artificial Analysis' Intelligence Index; Gemini
  Flash was reported beating Pro on SWE-bench Verified. And effort is a
  dial: the same model at high/max effort can out-reason a larger model
  at low/medium. Route on measured (model, effort) performance, not on
  tier names — a newer mid-tier beats an older flagship.
- **Measurement — the working canon and its limits.** SWE-bench Verified
  (+ Pro), Terminal-Bench, GPQA Diamond, AIME (fresh contests), MMLU-Pro,
  Humanity's Last Exam, LiveCodeBench, Aider Polyglot, LMArena,
  Artificial Analysis Intelligence Index v5 (60% private weight; scores
  not comparable across versions). Limits: saturation (benchmark
  half-life ~6 months), contamination (every public static set
  contaminated by default), harness-dependence (a 10-point gap can
  collapse under one scaffold; one model scored 0% vs 97% by harness on
  the same task), no cost/latency in scores. Credible teams use composites
  with private/held-out sets, fresh/rolling benchmarks, and their own
  task evals in a pinned harness (Inspect, HELM, lm-eval-harness).
- **Routing is real, with honest numbers.** Two patterns: router
  (predict-then-route before generation) and cascade (try cheap, escalate
  on a verifier); hybrid cascade-routing beats either alone. Academic
  results claim up to 98% cost cuts at matched quality (FrugalGPT) and
  >2× (RouteLLM); independent re-evaluation finds honest production
  savings ~20–25%, no router optimal, one commercial router worse than
  always using the single best model. Products: OpenRouter Auto Router
  (NotDiamond meta-model, cost/quality dial), NotDiamond, Martian,
  Cursor Router (ML classifier per request, Cost/Balance/Intelligence
  modes). Claude Code deliberately does not route per task — fixed
  `opusplan` split + overload fallback. Guard quality per route, with a
  quality floor.
- **Churn — deprecation notice floors.** Anthropic ≥60 days (recent
  practice sits at the floor: 60–62 days for the last five retirements —
  plan for 60); OpenAI ≥6 months GA / ≥3 months specialised / ~2 weeks
  preview; Google ≥2 weeks preview. DeepSeek retired
  deepseek-chat/reasoner 24 Jul 2026. Practice: pin dated snapshots,
  never floating aliases; gate every swap behind your own evals + shadow
  run; abstract the model ID behind one client/config; watch for silent
  drift with a continuous regression eval.
- **Governance gives the documentation shape, not the classification.**
  Model cards (Mitchell et al. 2019, 9 sections); NIST AI RMF 1.0
  (Govern/Map/Measure/Manage) + GenAI Profile; ISO/IEC 42001:2023
  (certifiable AI management system); EU AI Act Art. 53 (GPAI
  documentation). None prescribes reasoning tiers — they require that
  whatever selection rationale you use is documented, measured,
  versioned, monitored, and reviewed on change. The registry below is
  how you satisfy them.

## Repeatable mechanism — the model registry

Models change in weeks (Fable 5.1 was replaced by 5.2 on 9 Nov 2026).
The tier table in the appendix is a snapshot, not a fixture. Keep it
current with a registry — one row per (pinned model ID, effort) actually
deployed — plus a review cadence. Owner: the triage desk (David), until
the Teams integration owns it.

**Registry fields (per row):**

- Identity: provider, pinned model ID (dated snapshot, never a floating
  alias), floating alias it was resolved from (if any), context window,
  max output, knowledge cutoff, modalities/tool-use support.
- Tier assignment: reasoning tier (per the definition above), role(s) it
  may serve, effort default + allowed range, cost ($/MTok in/out,
  cached), observed latency (p50/p95 on your traffic).
- Evidence: benchmark scores with suite + version + harness + effort +
  date + run ID (public: SWE-bench Verified, Terminal-Bench, GPQA
  Diamond, AIME, AA Intelligence Index version; your own task evals
  govern, public ones corroborate), owner, link to the raw run log.
- Lifecycle: release date, provider retirement floor / deprecation
  notice date, notice period applicable, replacement candidate, status
  (current / candidate / legacy / retired).
- Governance: model-card / system-card link, intended use +
  out-of-scope uses, known failure modes, risk notes.

**Evidence priority:** (1) your own evals in a pinned harness (Inspect /
HELM / lm-eval-harness) on production-trace tasks — the only evidence
that governs a swap; (2) independent composites with private sets
(Artificial Analysis Intelligence Index, versioned; LMArena for
preference, not correctness); (3) provider model cards / system cards
and benchmark claims (vendor claims — verify); (4) fresh/rolling
benchmarks (LiveCodeBench, MathArena) to detect drift.

**Review triggers (any one forces a re-evaluation, regardless of
cadence):** provider deprecation/retirement notice; new model release in
the same family or a competitor at the same price point; price change;
retirement-floor change; your own eval regression or silent-drift
signal; benchmark-suite version change invalidating a cited score; a
role's failure pattern changing (e.g. coordinator misroutes spike).

**Cadence:** monthly lightweight scan (provider model/pricing/deprecation
pages + AA Index / LMArena movement, ~1 hour, logged); quarterly re-run
of your own eval suite against all registry entries + any new
candidates, with a written reclassification decision (keep / promote /
demote / retire) per entry; annual governance review (model cards
refreshed, NIST/ISO mapping checked). On a trigger, run the swap gate
(shadow → compare → cut over) before the provider's notice window
expires — and plan for Anthropic's 60-day floor, the shortest credible
window.

**Pollable sources (verified 2026-11-10) — the monthly scan is mostly a
diff, not research:** the registry's factual fields come from three
public feeds. (1) **models.dev** — `GET https://models.dev/api.json`,
no key, open-source (SST/opencode project). Per model: pinned ID,
reasoning flag + effort options, tool calling, modalities,
release/last-updated dates, context/output limits, pricing
(input/output/reasoning/cache), and a `status` field
(alpha/beta/deprecated). Verified live 2026-11-10: Fable 5.2,
gemini-4-argon, mimo-v2.6-flash, and Sonnet 5.5 are all present with
correct prices and effort options. Caveats: community-maintained (prices
can lag or vary by provider entry — Argon shows $2/$10 on one provider
and $4/$20 on Vertex, the introductory vs post-introductory rate); no
benchmark scores. (2) **Artificial Analysis Data API** — Intelligence
Index v5 + per-benchmark scores, speed, pricing. Free tier per a 29 Oct
2026 check: 1,000 requests/day with a free account key, attribution
required; commercial API is Pro ($499/mo). Caveats: an older summary
says API needs Pro — confirm on artificialanalysis.ai before building on
the free tier; scores are their composite, so always record the Index
version. (3) **OpenRouter** — `GET
https://openrouter.ai/api/v1/models`, no key. Live availability, pricing,
context, modalities, supported parameters, reasoning support; models
carry an `expiration_date` (a deprecation signal). Caveats: no benchmark
scores; prices are OpenRouter's served prices, not first-party list.
**LiteLLM's model cost map** (`model_prices_and_context_window.json`,
raw GitHub JSON, no key) is a fourth, largely redundant cross-check for
pricing + capability flags — no benchmarks, no lifecycle, community PRs
can lag. **What no feed does:** assign tiers or roles — which (model,
effort) serves coordinator / advisor / escalation is the judgment from
your own evals, and stays in the registry. Same for the provider notice
floors, which still need watching the provider pages.

**Script (built and verified 2026-11-10):**
`~/workspace/model-registry/poll_diff.py` (stdlib-only, no deps) +
`watchlist.json` (pinned models seeded from the appendix) + `README.md`.
`python3 poll_diff.py --check` validates the feeds; `python3 poll_diff.py`
diffs against `state/snapshot.json` and saves the new snapshot. AA feed
needs `ARTIFICIAL_ANALYSIS_API_KEY` set. Verified live: baseline written,
diff path works (minimax-m3 first-seen), and the new-model detector
flagged **Gemini 4 Carbon, released 2026-11-10** — a candidate for the
next quarterly review (it took the top of the AA Index v5). Live caveat
found by the build: **OpenRouter's list endpoint is not stable run to
run** — the three Claude 5.x models were absent from several fetches and
present in others minutes apart. The script re-fetches once and carries
previous values forward with a note, never reporting a removal on one
missing fetch. minimax-m3 has no first-party models.dev provider (16
relays carry it at $0.2–$0.6 in); the watchlist pins it to opencode-go
($0.3/$1.2, matching OpenRouter).

**Where it lives:** a `models-registry.md` (or registry table) next to
this note — start it from the appendix table, one row per deployed
(model, effort), with the evidence and lifecycle columns filled in at
the next quarterly review. The appendix table becomes a view of the
registry, not a second source of truth.

## Scaffolding

### A. DESK.md — fill the open "Strong" TODO

Proposed replacement for the Models section's Strong line:

```markdown
- Strong (verify, merge, ambiguous triage): the Advisor seat, Claude Opus
  (David's session); Fable 5.2 is the alternate for the Advisor seat and a
  per-item worker escalation with advisor CLEAR (see escalation rule). The
  coordinator seat itself stays on the default cheap model — see
  coordinator-model note 2026-11-10.
- Worker default, exact ids: patchbay seats = agent `claude-code`, model
  `sonnet`. (Already the rule; unchanged.)
```

### B. Teams template — three small additions, no redesign

1. **Confirm the coordinator seat as Claude Code Sonnet** in the built-in
   Triage desk template (already the default — this is a confirmation, not
   a change).
2. **Worker seat choices:** add `Claude Code, Fable 5.2` as a named choice
   labelled "escalation only — needs advisor CLEAR", never the default
   choice. `dispatch_item` already takes the agent/model only from the
   seat or its choices, never from item text — that rule is what makes
   this safe.
3. **Coordinator seat constraint:** record in the template that the
   coordinator seat must be a hook agent (Claude Code, Cursor, Codex,
   Antigravity), not OpenCode, for unattended operation — an OpenCode
   coordinator waits on a prompt per team-tool call. Carry the desk's
   first-reply agent+model check into `dispatch_item`: the coordinator
   verifies the worker's first reply against the requested agent+model
   before work starts.

### C. Longevity — verify what is already built, do not rebuild it

- Desk: the 150k checkpoint + fresh-session-from-ledger rule already
  exists. Keep it. It is the coordinator's longevity mechanism.
- Teams: the standing coordinator seat plus per-turn preamble / hook
  instruction delivery already exists in the design (the preamble is
  re-sent every turn precisely because members lost instructions on
  compaction in the reference desk). Verify in a live turn that a
  compacted coordinator still quotes its role — phase 2 verified this
  for members; re-verify it for the coordinator seat specifically.
- Pin hygiene stays a desk rule until the rostering/team integration
  inside Patchbay replaces it (already decided 2026-11-10).

## Appendix: model equivalence across providers

Built from the desk record (DESK.md, ROSTER.md, the ledger), the Teams
design docs, and the agent/model architecture (`docs/design/agents.md`,
`crates/patchbay-llm/src/types.rs`). Status marks what is verified in
David's own setup versus what is only known to exist in principle.

How to read it: a tier is a measured reasoning tier (a pinned model at
a given effort, with benchmark evidence — see Terminology), not a brand
and not a fixed capability. Any cell in the same row is an acceptable
substitute for the same seat *as of the evidence date*; the agent column
matters too, because instruction delivery, steering, and the team-tools
allow rule differ per agent. This table is a snapshot of the registry —
see Repeatable mechanism — not a second source of truth. Tier gaps
narrow every release; re-derive the rows each quarter from your own
evals.

| Tier | Claude Code (Anthropic) | Antigravity (Google) | Codex (OpenAI) | OpenCode / Go | Cursor | Patchbay own / Grok |
|---|---|---|---|---|---|---|
| **1 — Cheap coordinator** (high-volume checklist) | — (no cheap Claude id in the record; do not assume Haiku) | — (Gemini buckets are standard-tier; no cheap id evidenced) | — (no id evidenced) | `mimo-v2.6-flash` ✓ in use — the desk runs on it | Auto is plan-based, not a cheap-tier pick | `pb-<model>` catalog lives in Settings (DB) — not enumerable from here |
| **2 — Standard worker** | `sonnet` (`claude-sonnet-5-5`) ✓ default | Gemini (5h/weekly buckets) ✓ template choice | ? (model id unverified) | — (no standard-tier id evidenced) | Auto ✓ template choice | `pb-<model>` — see above |
| **3 — Strong / escalation** | `Fable 5.2` ✓ configured (proven by the 2026-11-10 leak); `Opus` ✓ Advisor | `gemini-4-argon` (Argon weekly bucket) ✓ trialled 2026-11-07 | GPT-5 / Astra — candidate, unverified | `gemini-4-argon` ✓, `minimax-m3` ✓ (both trialled as strong) | Auto draws from the plan pool, not a fixed strong id | `pb-<model>` — see above |

Notes and gaps:

- **Verified vs assumed.** A ✓ means the model id appears in David's
  own records as configured or used. Everything else is either a
  candidate to confirm (Codex GPT-5/Astra) or a gap to fill (the
  `pb-<model>` list, which lives in Patchbay Settings inside
  `patchbay.sqlite` — a read-only shell pass on the Mac could dump the
  Settings table, or the list can be read off the Settings → Providers
  screen).
- **Kilo lists ~330 models** (per `agents.md`) — the count is verified,
  the ids are not enumerated anywhere I read. It is a fallback shelf,
  not a tier mapping, until someone lists the ids.
- **Cursor Auto is not a tier.** It draws from the Cursor plan pool
  (included + bonus + grants + slow pool, polled from
  `api2.cursor.sh`). It works as a standard worker, but its cost and
  capability vary by what the plan serves that day — do not treat it
  as the cheap tier.
- **Argon headroom is real but bounded.** The Argon weekly bucket is a
  quota window; when it is spent, escalation falls through to Fable
  5.2. The sidebar gauge shows the bucket — the coordinator cannot see
  it, so a worker that hits an Argon limit must report it as a bounce,
  not silently retry.
- **Wire formats.** Patchbay's own provider layer speaks Chat
  Completions, Responses, Anthropic, and Gemini (`types.rs`
  `WireFormat`) — OpenAI-compatible endpoints (Ollama, LM Studio,
  OpenRouter, Groq, xAI, DeepSeek, Mistral) are all addressable the
  same way, so a future cheap-tier entry from any of those is a
  Settings change, not a code change.

## Open items (not decided here)

- Phase 4 still needs its security review of the *diff* before any real
  (non-sandbox) use, and nobody has seen the Settings > Teams screens
  rendered in a browser yet (both noted in the phase-4 review).
- The `decision_open` class loophole (a `writes_items` role can open a
  question as class `coordinator`, resolving with no gate) is accepted
  for now in the design — dispatch must not wait on a decision without a
  host-side check of who opened it and what class it took.
- The Ship Lead seat's model on the hand-run desk is unrecorded; worth
  one line in ROSTER.md the next time it is touched.
