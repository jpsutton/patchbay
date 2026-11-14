# Model registry poll-and-diff

Polls the three public model feeds and diffs them against the last
snapshot, so the monthly registry scan is a diff, not research. Built
for the Patchbay model registry described in
`../patchbay-coordinator-model-recommendation.md` (Repeatable mechanism).

## What it watches

Pinned models in `watchlist.json` (the deployed (model, effort) rows),
plus any new release in the watched provider families (Anthropic,
Google, OpenAI, Xiaomi/MiMo, MiniMax).

Per pinned model it flags: price changes, status flips (especially
`-> deprecated`), reasoning-effort option changes, context-window
changes, Artificial Analysis Intelligence Index movement, and an
OpenRouter `expiration_date` appearing.

## Feeds

| Feed | URL | Auth | Covers |
|---|---|---|---|
| models.dev | `GET https://models.dev/api.json` | none | identity, pricing, context, reasoning/effort options, release/last-updated dates, `status` (alpha/beta/deprecated) |
| OpenRouter | `GET https://openrouter.ai/api/v1/models` | none | live availability, pricing, context, `expiration_date` |
| Artificial Analysis | `GET https://artificialanalysis.ai/api/v2/language/models/free` | `x-api-key` header | Intelligence Index v5 + headline indices, median performance, pricing (free tier) |

No feed assigns tiers or roles — that judgment stays in the registry,
from your own evals.

## Setup

Python 3.9+ stdlib only, no dependencies. Runs on the Mac or this VM.

```sh
cd ~/workspace/model-registry   # or wherever you put this folder
python3 poll_diff.py --check    # verify the feeds respond
python3 poll_diff.py            # first run: writes the baseline snapshot
python3 poll_diff.py            # later runs: prints changes since last run
```

### Artificial Analysis key (optional but recommended)

The AA feed is the only source of benchmark evidence, and it needs a
free API key:

1. Create a free account at https://artificialanalysis.ai and generate
   an API key (free tier covers the `/free` endpoint used here).
2. `export ARTIFICIAL_ANALYSIS_API_KEY=<key>` before running.
3. Attribution is required when you use AA data anywhere public.

Without the key the script skips AA and says so — it does not fail.
The full `/api/v2/language/models` endpoint is Pro+; this script
deliberately uses only the free endpoint.

## Files

- `poll_diff.py` — the script
- `watchlist.json` — pinned models + watched provider families (edit this
  when a model is promoted, demoted, or retired)
- `state/snapshot.json` — last poll's data (the diff baseline; keep it —
  deleting it makes the next run a fresh baseline with no diff)
- `state/snapshot.prev.json` — the snapshot before the last run

## Cadence

Monthly, per the registry doc — or on any review trigger (deprecation
notice, new release, price change, eval regression, benchmark-version
change). When a change is flagged, follow the doc's swap gate
(shadow → compare → cut over) before the provider's notice window
expires; plan for Anthropic's 60-day floor.

## Limitations (honest ones)

- models.dev is community-maintained: prices can lag or vary by
  provider entry (Argon shows $2/$10 on one provider and $4/$20 on
  Vertex — introductory vs post-introductory rate). It carries no
  benchmark scores.
- OpenRouter prices are its served prices, not first-party list prices.
- AA's free endpoint returns headline indices only; per-model detail
  and full pricing are Pro+. AA no longer publishes a Coding or Agentic
  Index — the script reads the Intelligence Index.
- A model "not found" in a feed means the id needs verifying (same
  problem as the Codex GPT-5/Astra ids) — it does not mean the model
  doesn't exist.
