# Model Classification Standards — How the AI Industry Tiers, Measures, Routes and Re-classifies LLMs by Reasoning Capacity

**Research date:** 10 Nov 2026. **Snapshot warning:** model names, prices and benchmark versions in this report are as published on/around that date. Multiple sources stress line-ups move in weeks (e.g. Anthropic Fable 5.2 replaced Fable 5.1 on 9 Nov 2026; OpenAI's page now leads with GPT-6 Astra / GPT-5.6). Treat every price/score as dated, and re-check the provider page before acting. Where a fact is marked `verified live` it was fetched 10 Nov 2026; `index` means taken from search-result text of the cited page.

## Summary

- There is **no single industry-standard term** for the dimension that separates a small fast model from a large reasoning-heavy one. Providers use their own tier names and qualitative phrases ("best combination of speed and intelligence", "demanding reasoning", "most capable"). The closest standardised vocabulary is **"reasoning model"** (OpenAI's documented term, defined by internal reasoning tokens) and the per-request control **"reasoning effort" / "effort" / "thinking level"** — i.e. reasoning is now a *dial on a model*, not only a property of a model. "Capability tier" / "intelligence tier" are used by aggregators and in conversation, not as a defined provider standard; "capability" alone is ambiguous (it also covers modalities, tool use, context, languages).
- Every major provider tiers 3–4 rungs on the same axis — **flagship / balanced / light** — with roughly 10–100× price spread, but the *capability gap between rungs is narrowing* (Sonnet 5.5 within ~2 points of Opus 5.5 on Artificial Analysis' Intelligence Index; Gemini Flash beating Pro on some benchmarks). Tier ≠ fixed reasoning capacity: the same model at `high`/`max` effort can out-reason a larger model at `low`/`medium`.
- The reputable measurement stack is: **SWE-bench Verified (+ Pro), Terminal-Bench, GPQA Diamond, AIME (via fresh contests), MMLU-Pro, Humanity's Last Exam, LiveCodeBench, Aider Polyglot, LMArena** — but most static public benchmarks are saturated and/or contaminated at the frontier, so credible teams use composites with private/held-out sets (Artificial Analysis Intelligence Index v5, 60% private weight), fresh/rolling benchmarks, and their own task-specific evals in a pinned harness (Inspect / HELM / lm-eval-harness).
- Routing is a real, proven practice (router = predict-then-route on input; cascade = try cheap, escalate on a verifier), with academic results of 2× to 98% cost reduction at matched quality — but those headline numbers are benchmark-specific; independent re-evaluation (RouterBench / LLMRouterBench / RouterArena) finds ~20–25% honest production savings and routers that over-select expensive models. Cursor routes per-request with an ML classifier; Claude Code deliberately does **not** route per task — it uses fixed role splits (`opusplan`: Opus plans, Sonnet executes).
- Churn is governed by **dated snapshot pinning + deprecation notice floors** (Anthropic ≥60 days, OpenAI ≥6 months GA / ≥3 months specialised / ~2 weeks preview, Google preview ≥2 weeks) plus eval-gated swaps. No provider guarantees a tier's capability stays put; floating aliases silently move.
- Governance gives you the *documentation* shape, not the classification: **model cards (Mitchell et al. 2019), NIST AI RMF 1.0 (Govern/Map/Measure/Manage) + GenAI Profile, ISO/IEC 42001:2023, EU AI Act Art. 53**. None prescribes "reasoning tiers" — they prescribe versioned documentation, measured evaluation, risk assessment and monitoring on change.

---

## 1. Terminology — what the industry actually calls the dimension

### 1.1 "Reasoning model" is the one defined provider term
- **OpenAI — "Reasoning models" (provider docs, verified live 10 Nov 2026):** "Reasoning models use internal reasoning tokens before producing a response. This helps the model plan, use tools effectively, inspect alternatives, recover from ambiguity, and solve harder multi-step tasks." Source: OpenAI, *Reasoning models*, https://platform.openai.com/docs/guides/reasoning — verified live. Reasoning tokens are billed as output tokens and occupy context window space.
- The same page defines the control, not the model, as the unit of reasoning: "`reasoning.effort` … guides the model on how much to think" — values `none, minimal, low, medium, high, xhigh, max`, model-dependent, with model-dependent defaults (`gpt-5.5` defaults to `medium`). Effort table maps `none` → classification / fast retrieval, `low` → efficient reasoning, `medium` → default / agentic coding, `high` → hard reasoning / complex debugging, `xhigh`/`max` → deep research / most complex tasks. GPT-5.6/GPT-6 add an independent `reasoning.mode` (`standard` vs `pro`).
- **Anthropic — "adaptive thinking" + "effort":** "Adaptive thinking lets Claude dynamically determine when and how much to use extended thinking based on the complexity of each request" and "`effort` controls how much work Claude puts into the whole response". Effort ladder `low → medium → high (default) → xhigh → max`; thinking tokens billed as output. Source: Anthropic adaptive-thinking docs via mirror, https://github.com/sullivan-street-projects/anthropic-docs-local/blob/HEAD/api/adaptive-thinking.md (index); corroborated by Claude Code effort docs, https://developertoolkit.ai/en/claude-code/quick-start/deep-reasoning/ (index). Note: Anthropic calibrates effort per model, so `high` on one model ≠ `high` on another.
- **Google — "thinking level":** Gemini 3 introduced `thinking_level: low / medium / high` replacing the older `thinking_budget` token count; Deep Think is a named high-compute reasoning mode on Pro. Source: https://www.neurals.ca/tech/gemini/gemini-models/ and https://raw.ashitaorbis.com/understanding-ai/wiki/gemini-deep-think (index).
- **DeepSeek — "thinking mode":** formerly a separate model (`deepseek-reasoner` = thinking mode, `deepseek-chat` = non-thinking), now a request parameter (`thinking`, `reasoning_effort: low/high/max`) on `deepseek-v4-flash` / `deepseek-v4-pro`. Source: https://github.com/krol05/eth-lecture-copilot/blob/HEAD/docs/providers/deepseek.md (index). This is the clearest illustration that "reasoning model vs chat model" was a *transitional* distinction that providers have collapsed into a parameter.

### 1.2 "Test-time compute" is the research term, not a product term
- Snell et al., *Scaling LLM Test-Time Compute Optimally can be More Effective than Scaling Model Parameters* (arXiv:2408.03314, Aug 2024): allocating compute per prompt can beat a 14× larger model on problems where a smaller model already has some success; "the effectiveness of different approaches to scaling test-time compute critically varies depending on the difficulty of the prompt". Cited via https://www.ai-news.media/posts/what-are-reasoning-models-how-they-differ-from-standard-llms (index) and https://hub.cissychen.com/ai-ml/reasoning-models-day28.en.html (index). Use it in design docs / papers; do not expect it in provider pricing or model pickers.

### 1.3 "Capability tier" / "intelligence tier" — used, but not standardised
- No provider publishes a definition of "capability tier" or "intelligence tier". Providers publish *tier names* (below) plus qualitative positioning. Aggregators normalise them: e.g. "Rungs are each provider's own tiers normalized (flagship / mid / light)" — https://pricing.aitokenomics.app/ (index); "Anthropic ships Claude as a three-tier family — a flagship, a workhorse, and a lightweight model — and each tier trades capability against cost and latency" — https://ccapi.ai/blog/claude-opus-vs-sonnet-vs-haiku (index).
- **"Intelligence Index" is a specific product, not a generic term:** Artificial Analysis' *Intelligence Index* is a weighted composite (see §3). Quoting an "intelligence score" without naming the index/version is meaningless — versions are rebuilt repeatedly and scores across versions are not comparable (see §3).
- **"Capability" is ambiguous — the mission's concern is confirmed:** in provider docs and model tables "capabilities" routinely means the *feature set*: text/image input, vision, tool use, multilingual support, context window, structured outputs. Anthropic's own overview: "All current models support text and image input, text output, multilingual capabilities, vision, and tool use" — https://platform.claude.com/docs/en/models/overview?ref=toolarium.ru (index). A model can therefore be "more capable" in the multimodal/tool sense while being weaker at reasoning, and vice versa. A tier label that only says "capability" does not tell a router whether the model can *reason harder*.

### 1.4 Verdict on the candidates
| Term | Status |
|---|---|
| **Reasoning model** | Defined (OpenAI). Means: uses internal reasoning tokens. Now blurred — most flagships are reasoning models with effort dialled down. |
| **Reasoning effort / effort / thinking level** | Defined per provider (OpenAI `reasoning.effort`, Anthropic `effort`, Google `thinking_level`, DeepSeek `reasoning_effort`). The *operational* dimension. |
| **Test-time compute** | Standard research term (Snell et al. 2024). Correct mechanism; not a product/tier label. |
| **Capability tier / intelligence tier** | Conversational / aggregator normalisation. No provider definition; "capability" also = modalities/tools/context. Usable internally only if defined. |
| **Reasoning capacity** | **Not a provider term.** No provider docs use it as a defined label (searched). It is intelligible and narrower than "capability", but it is *your* term, not the industry's — see Synthesis (a). |
| **Chat model vs reasoning model** | Obsolete as a hard split: OpenAI folded reasoning into GPT-5.x; DeepSeek folded `reasoner` into a parameter on V4. Keep only as a historical description. |

---

## 2. Provider tier systems

Common pattern (all providers, Nov 2026 snapshot): **flagship (hardest reasoning, slowest, most expensive) / balanced workhorse / light (fastest, cheapest, high-volume)** — plus, at Anthropic/Google, a top "beyond flagship" rung. Prices are USD per 1M tokens, input/output, standard API rates; check the provider pricing page before budgeting.

### 2.1 Anthropic — Fable / Opus / Sonnet / Haiku
Source: Anthropic, *Models overview*, https://platform.claude.com/docs/en/models/overview?ref=toolarium.ru (index, crawled 10 Nov 2026); Claude Code docs, https://code.claude.com/docs/en/model-config (verified live 10 Nov 2026).

| Tier (current, 10 Nov 2026) | Anthropic's wording | Price $/MTok in/out | Speed (Anthropic label) | Default effort |
|---|---|---|---|---|
| Claude Fable 5.2 | "For demanding reasoning and long-horizon agentic work" | $8 / $40 | Moderate | high |
| Claude Opus 5.5 | "For long-running agentic coding and knowledge work" — "start with Opus 5.5 for most workloads" | $4 / $20 | Moderate | medium |
| Claude Sonnet 5.5 | "The best combination of speed and intelligence" | $2 / $10 | Fast | high |
| Claude Haiku 5.5 | "For high-volume, latency-sensitive tasks such as classification, extraction, and routing" | from $0.10 / $0.50 (≤100K-token prompts; $0.50/$2.50 above) | Fastest | medium |

All four: 1M-token context, 128K max output, June 2026 knowledge cutoff, text+image input, vision, tool use. Fable 5.1 ($10/$50) was top until 9 Nov 2026 and is now legacy, with a retirement floor of 1 Sep 2027 (per https://geotoolbox.ai/blog/claude-models-compared, index). Anthropic's own guidance: use Fable only "when your evals on Claude Opus 5.5 at higher effort still fall short" — i.e. escalate on *measured* shortfall, not on tier name. Claude Code aliases: `opus` = complex reasoning, `sonnet` = daily coding, `haiku` = simple tasks, `fable` = hardest/longest, `opusplan` = Opus plans / Sonnet executes (verified live).

### 2.2 OpenAI — GPT-6 / GPT-5.x full / mini / nano (+ o-series legacy)
Sources: OpenAI, *Reasoning models*, https://platform.openai.com/docs/guides/reasoning (verified live); pricing summaries https://www.memetik.ai/guides/gpt-5-api-pricing and https://www.secondtalent.com/resources/every-openai-model-explained-compared/ (index, Sep 2026 — predate the GPT-6 lead, so treat as history).

- Current lead (Nov 2026, OpenAI docs): start with `gpt-6-astra` for most reasoning workloads; `gpt-5.6-terra` for lower cost; `gpt-5.6-luna` for lowest cost/latency. OpenAI's own guidance quoted in secondary sources: "mini GPT models are fast and inexpensive for simpler tasks" and choose a smaller variant "if you're optimizing for latency and cost".
- GPT-5 generation pricing (Sep 2026, OpenAI pricing page via Memetik): `gpt-5-nano` $0.05/$0.40 → `gpt-5-mini` $0.25/$2.00 → `gpt-5` $1.25/$10 → `gpt-5.5` $5/$30 → `gpt-5.5-pro` / `gpt-5.4-pro` $30/$180. GPT-5.4 workhorse $2.50/$15; mini $0.75/$4.50; nano $0.20/$1.25 (Second Talent). Output price is the driver (up to 75× spread).
- The o-series (o1/o3/o4-mini) was the original dedicated reasoning line; reasoning is now built into GPT-5.x/GPT-6, which "decides when to think" — the standalone reasoning-model line has largely been absorbed.
- **Key structural point:** within one model family, `reasoning.effort` (none→max) plus `reasoning.mode` (standard/pro) gives a continuous reasoning/cost dial, so the mini/nano/full split is a *price/performance starting point*, not a fixed reasoning ceiling.

### 2.3 Google — Argon / Pro / Flash / Flash-Lite (+ Nano on-device)
Sources: https://geroundtable.com/which-gemini-model-to-use/ ; https://www.neurals.ca/tech/gemini/gemini-models/ ; https://www.ai-toolbox.co/gemini-models/gemini-3-1-pro-vs-flash-vs-flash-lite-2026 (all index, Oct–Nov 2026).

- Gemini app tiers: Flash-Lite = "fast, cost-efficient … quick answers … lightweight tasks"; Flash = "high-speed, capable … balances performance and responsiveness"; Pro = "advanced reasoning tier, designed for complex prompts, sophisticated coding, in-depth problem-solving"; Argon = "top-tier intelligence with dynamic deep reasoning … highest performance for complex workflows" (Pro/Ultra plans only). Free accounts get Auto (Google routes) or Flash-Lite.
- API (Oct 2026): Gemini 3.1 Pro (preview, $2/$12 ≤200K, $4/$18 above), Gemini 3.8 Flash (GA 2 Sep 2026, $0.75/$3.75 intro to 31 Dec 2026, then $1.50/$7.50), Gemini 3.5 Flash-Lite (GA 21 Jul 2026, $0.30/$2.50). Gemini 4 Argon GA 13 Oct 2026 is now the top model. All 1M-token context. `thinking_level` low/medium/high across tiers.
- **Narrowing-gap evidence:** Gemini 3 Flash was reported as outperforming the original Gemini 3 Pro on SWE-bench Verified at roughly a quarter of the price — the mid tier can beat the previous flagship (https://www.ai-toolbox.co/gemini-models/gemini-3-1-pro-vs-flash-vs-flash-lite-2026, index).

### 2.4 xAI — Grok (single flagship + Heavy multi-agent)
Sources: https://kunalvohra.com/blog/gemini-vs-claude-vs-grok-vs-chatgpt-api-for-business ; https://ai-tools-compared.com/articles/deepseek-vs-grok.html (index, Oct 2026).
- Grok 4.7 flagship $2/$6 (cached $0.50), 500K context, configurable reasoning, prompts >200K billed double; older Grok 4.3/4.20 $1.25/$2.50. Grok 4 Heavy is a multi-agent reasoning model (SuperGrok Heavy $300/mo). xAI tiers less by size (no mini/nano equivalent) and more by *reasoning mode / multi-agent* — a different axis from the other three providers.

### 2.5 DeepSeek — V4 Pro / V4 Flash (open weights)
Sources: https://www.aidevops.us/deepseek/ ; https://github.com/krol05/eth-lecture-copilot/blob/HEAD/docs/providers/deepseek.md (index).
- `deepseek-v4-pro` $1.32/$3.96 peak ($0.66/$1.98 off-peak), `deepseek-v4-flash` $0.30/$1.20 peak ($0.15/$0.60 off-peak), both 1M context, 384K max output; thinking on/off per request. Earlier `deepseek-chat`/`deepseek-reasoner` retired 24 Jul 2026. DeepSeek is the price disruptor (roughly 10–13× cheaper than Grok at list) and the only major provider with MIT-licensed open weights in this set — relevant if a tier needs self-hosting rather than API price.

### 2.6 Cross-provider takeaway
The tier *names* do not align (Opus≠GPT-5.5≠Pro≠Grok in any formal sense) and the *price order* does not align with the *capability order* across providers or even across generations within one provider (newer mid-tier models beat older flagships; Haiku 5.5 is 40× cheaper than Opus 5.5 on input yet shares its context window and cutoff). Any scheme that hardcodes "Opus = strong, Haiku = weak" will be wrong within months; the durable object is a *measured* tier, not a name.

---

## 3. Measurement — benchmarks for reasoning / coding

### 3.1 The working canon (what labs and independent evaluators actually cite)
| Benchmark | What it measures | Who runs / maintains | Size / format | Status at frontier (Nov 2026) |
|---|---|---|---|---|
| **SWE-bench Verified** | Real GitHub issues → patch passes maintainer tests (agentic, execution-graded) | Princeton/Stanford SWE-bench team; Verified subset curated with OpenAI | 500 tasks, 12 Python repos | Most decision-relevant for coding; heavily saturated (frontier agents 70–80%+, now low–mid 90s in some harnesses); very high contamination (public repos/issues/commits pre-cutoff) |
| **SWE-bench Pro** | Same, broader | SWE-bench team | 1,865 tasks, Python/Go/TS/JS | Newer, harder variant |
| **Terminal-Bench** | Terminal-based agentic coding tasks | Terminal-Bench team (Stanford / Laude Institute) | 66 tasks in v4.0 | Used inside Artificial Analysis index; harness-sensitive |
| **GPQA Diamond** | Graduate science reasoning, "Google-proof" MCQs | David Rein et al. (NYU/Anthropic/Cohere) | 198 hardest of 448 Qs, bio/physics/chem | Saturated — frontier >65% PhD-expert baseline, leaders into 90s; very high contamination despite canary string; effectively unmaintained as a leaderboard, re-run by Epoch AI / Artificial Analysis |
| **AIME** | Competition maths (integer answers) | MAA; run fresh via MathArena | 15 problems/paper | Largely saturated at frontier; fresh contests (HMMT, olympiad) still discriminate; near-zero contamination *if* evaluated within days of the contest |
| **MMLU-Pro** | Harder, reasoning-heavy general knowledge, 10 options | MMLU-Pro authors | ~12K Qs | Successor to saturated MMLU (frontier >88–90% on MMLU); still rewards elimination heuristics |
| **Humanity's Last Exam (HLE)** | Expert-curated, deliberately frontier-defeating Qs | CAIS / Anthropic (2025); HLE-Diamond subset in AA v5 | 3,000 Qs (2,158 text-only in AA) | Private set component restores signal; used in AA index |
| **LiveCodeBench** | Contamination-free coding, continuously collected from recent contests | LiveCodeBench team | Rolling monthly | Designed to resist contamination; self-repair / test-output prediction |
| **Aider Polyglot** | Code editing: correctness + valid applyable edit format, with retry | Paul Gauthier / Aider project | 225 Exercism exercises, 6 languages, 2 attempts | Approaching saturation (GPT-5 high 88.0% top); high contamination; leaderboard last updated 20 Nov 2025 — stale, use with care |
| **HumanEval** | Single-function Python code vs hidden tests | OpenAI (Chen et al. 2021) | 164 problems | Saturated (frontier 95–98%); leaked into pretraining; do not use to separate frontier models |
| **LMArena (Arena, ex-Chatbot Arena)** | Blind pairwise human preference → Bradley-Terry / Elo | LMArena Inc. (ex-LMSYS / UC Berkeley) | >5.7M votes, 330+ models (May 2026); 13 category boards | Never saturates by construction (relative), but top-10 CIs overlap; measures *preference*, not correctness — style/verbosity bias; gaming risk |
| **Artificial Analysis Intelligence Index** | Composite of 9 evals, 4 categories ×25% | Artificial Analysis (independent company) | v5.0.1: AA-Briefcase, AutomationBench, GDP.pdf, FrontierCode 2.0, Terminal-Bench 4.0, AA-Omniscience, HLE-Diamond, CritPt v2, Terminal-Bench-Science | 60% of weight on private/held-out sets; components/graders swapped as they saturate, so **scores are not comparable across index versions**; primarily text-based, English |

Sources for the table: https://seminal.ai/reference/categories/benchmarks-and-leaderboards/ ; https://memx.app/blog/ai-benchmarks-explained-mmlu-gpqa-swe-bench/ ; https://aiwiki.ai/wiki/llm_rankings ; https://www.scaler.com/topics/artificial-analysis-explained/ ; https://www.iodigest.com/article/artificial-analysis-intelligence-index-v5-0-50269651 ; https://www.rugvailabs.com/tracks/track-05-transformers/evaluation-frontiers-2026 (all index).

### 3.2 Who keeps them current, and how
- **Rolling / fresh:** LiveCodeBench (monthly new contest problems), MathArena (evaluate within days of contest), HLE's private set, ARC-AGI-2's semi-private set, FrontierMath (Epoch AI). The defence is *freshness + held-out data*, not a better static set.
- **Composite re-weighting:** Artificial Analysis rebuilds its index repeatedly (v4.0 Jan 2026 → v4.2 → v4.3 Sep 2026 → v5 Oct 2026), dropping saturated components (MMLU-Pro, LiveCodeBench, AIME 2025, GDPval-AA, SciCode, AA-LCR) and adding private-set ones. Consequence: always cite the index *version* with the score.
- **Harness standardisation:** scores are not portable across harnesses. "A 10-point gap between two models often collapses when both run under the same scaffold" (Seminal AI); "frontier models are trained in their own harness — evaluating them in a different one under-reports capability" and Opus 4.6 scored 0% with no harness vs 97.1% with a hand-crafted one on one ARC-AGI-3 environment (RLHF-book Lecture 12, https://b2aab36f.rlhf-book.pages.dev/teach/course/lec12-chap16-evals/, index). Always record harness + effort + sampling alongside the score.

### 3.3 Known limits
- **Contamination:** every public static benchmark is contaminated by default (training corpora scrape the web; HumanEval on GitHub, MMLU on exam sites, GPQA widely mirrored). "You cannot tell by how much" (Seminal AI).
- **Saturation:** benchmark half-life ~6 months; "the benchmark you rely on today will be saturated in 12–18 months — plan accordingly" (https://www.institutepm.com/knowledge-hub/ai-benchmark-literacy-guide, index). MMLU, HumanEval, GSM8K, GPQA Diamond, AIME are saturated or near it at the frontier; SWE-bench Verified is heading there.
- **Gaming / Goodhart:** "When a benchmark becomes a target, it ceases to be a good measure." Labs optimise for cited benchmarks; LMArena can be gamed by voter-pleasing style; scaffolding (multi-turn loops, majority voting) can inflate scores by up to +43% vs raw API calls (AKI atlas, https://aki1k.com/benchmarks, index — treat as secondary, not primary).
- **Narrowness:** no single benchmark measures reasoning + tool use + long context + multilingual together; benchmarks measure neither cost nor latency ("a model scoring 95% on GPQA but taking 40 seconds at $0.08/query may be far worse … than one scoring 88% at 1.2 seconds and $0.003/query" — Institute PM).
- **Variance:** at saturation, rankings are dominated by seed / prompt-format noise; GPQA Diamond's remaining spread is "label noise and sampling luck" (Seminal AI). Report confidence intervals, not point ranks.

---

## 4. Routing practice

### 4.1 The two patterns (and their combination)
Source: https://blog.prompt20.com/posts/llm-routing-and-model-cascades-complete-guide/ ; https://www.justmalhar.com/blog/what-is-llm-routing/ ; https://www.tmls.nyc/research/model-routing-cascades (index).
- **Router (predict-then-route):** decides *before* generation from the input (prompt, metadata, user plan, task type). One model call; lowest latency; failure mode is a wrong prediction shipping a weak answer unchecked. Needs a good predictor of difficulty / model fit.
- **Cascade (generate-then-verify):** tries the cheapest model first, inspects the answer with a verifier/scorer, escalates only if it fails. Knows more when it decides (it has an answer to judge) but escalated requests pay for cheap + check + expensive calls, in sequence (p95 latency doubles). Needs a good, cheap verifier. Best when answers are cheap to verify (tests pass/fail, structured output, classification) — which is exactly the coding case.
- **Cascade routing (hybrid):** router sends clearly-easy → small, clearly-hard → large, uncertain middle → cascade. Dekoninck, Baader & Vechev formalised this; it "consistently outperformed either approach alone by a large margin" (Prompt20 guide, citing the paper).
- **Signals used:** predicted query difficulty, predicted win-rate / quality, model confidence / self-consistency, verifier score, task type, cost budget ("willingness to pay" / cost-quality tradeoff dial 0–10), latency budget, user plan, conversation state. The quality estimator "drives most of the result" (Prompt20).

### 4.2 Academic evidence (the numbers, with their limits)
- **FrugalGPT** (Chen, Zaharia & Zou, Stanford/UC Berkeley, arXiv:2305.05176, May 2023): LLM cascade with a learned (DistilBERT) scorer; matched GPT-4 performance with **up to 98% cost reduction**, or +4% accuracy at the same cost, on its benchmarks. Gains strongest on classification / structured-output tasks where correctness is definable; more modest on open-ended generation.
- **RouteLLM** (Ong et al., LMSYS/Berkeley, arXiv:2406.18665, ICLR 2025): routers trained on human-preference data; **>2× cost reduction** without quality loss; 95% of GPT-4's MT-Bench quality while calling GPT-4 on only 26% of prompts (14% with data augmentation). Transfers across model pairs. Savings were 85% on MT-Bench but only 45% on MMLU and 35% on GSM8K — dataset-dependent.
- **Hybrid LLM** (Ding et al., arXiv, Apr 2024): difficulty-based routing, **up to 40% fewer calls to the large model, no quality drop**.
- **AutoMix / uncertainty systems:** ~97% of frontier accuracy at ~25% of cost (TMLS summary, citing the literature).

### 4.3 Concrete products
| Product | How it routes | Signals / controls |
|---|---|---|
| **OpenRouter Auto Router** (`openrouter/auto`) | Meta-model powered by NotDiamond reads the prompt, weighs complexity, task type, model strengths, routes to best fit, reports which model answered | Cost-vs-quality dial 0–10 (default 7), allow-list (e.g. `anthropic/*`), session stickiness; no router fee — pay the routed model's rate. Source: https://www.instagram.com/reel/DatCiNwIgJn/ (index, via search) + OpenRouter docs as summarised in routing surveys. Also **Fusion**: decomposes a request, runs several cheap models in parallel, aggregates — vendor-claimed to beat single frontier models on reasoning/coding; self-reported benchmarks, treat with caution. |
| **NotDiamond** | Hosted meta-model predicts best LLM per input (<50ms claimed; 10–100ms); pre-trained chat + code routers; custom routers trained on your own {input, candidate responses, eval scores} | `tradeoff="cost"\|"latency"`, continuous `cost_quality_tradeoff` 0–10; claims 25% accuracy improvement + up to 10× cost reduction. Repositioned to "Model Routing for Coding Agents". Sources: https://github.com/mayanpathak/gateway/blob/HEAD/docs/research/routing-specialists.md ; https://dreaming.press/posts/2026-06-21-routellm-vs-notdiamond-vs-martian.html (index). |
| **Martian** | Router using mechanistic interpretability / "model mapping" — predict a model's behaviour on a prompt without running it; 200+ models | Max cost per request, cost-vs-quality "willingness to pay" (Pareto frontier); claims 20–97% cost cuts, "often beating GPT-4". Authored RouterBench (arXiv:2403.12031), the field's routing benchmark. Same sources as above. |
| **RouteLLM** (open source, LMSYS) | Trainable routers (matrix factorisation, BERT, causal-LLM, SW-ranking) on preference data; win-rate vs threshold | Open, Apache-2.0; servable behind LiteLLM. https://github.com/lm-sys/RouteLLM (via routing surveys). |
| **Cursor — Cursor Router** (relaunched Auto) | ML classifier runs on *each agent request*, routes by task type and complexity; goal = cheapest model that still produces comparable results | Optimisation mode: Cost (former Auto) / Balance (default for new users) / Intelligence (complex multi-step); blocked models skipped; bills at routed model's list price; `/best-of-n` runs several models and keeps the result that works. Source: https://aq.dev/guides/model-routing-for-coding-agents-explained/ (index, Oct 2026). |
| **Claude Code** | **No per-task router.** You set a model alias; `opusplan` is a fixed plan/execute split (Opus plans, Sonnet executes); fallback models only take over on overload/unavailability (reliability, not difficulty). Subagents can set `model` in their definition or inherit the session model. Source: https://code.claude.com/docs/en/model-config (verified live); https://aq.dev/guides/model-routing-for-coding-agents-explained/ (index). |
| **GitHub Copilot / ChatGPT** | Copilot ships auto model selection across Chat, CLI and cloud agent; ChatGPT has routed between fast and reasoning models since GPT-5. Source: AQ guide (index). |
| **OpenCode** | `small_model` option sends lightweight work (e.g. title generation) to a cheaper model — fixed role split, not per-request routing. Source: AQ guide (index). |

### 4.4 The honest caveat
Independent re-evaluation (RouterBench / LLMRouterBench / RouterArena, summarised at https://seangeng.com/writing/the-honest-guide-to-llm-routing and https://dreaming.press/posts/2026-06-21-routellm-vs-notdiamond-vs-martian.html, index) found **no router optimal across all metrics**; a leading commercial router ranked 12th for over-selecting expensive models; one commercial router did *worse than always using the single best model*. Honest production savings are ~20–25%, not 85–98%. Routing pays only when (a) the price gap between strong and weak models is large, and (b) a meaningful fraction of traffic is genuinely routable to the cheap one — and a router tuned for cost alone "will collapse quality nobody approved" (TMLS). Guard quality per route, with a quality floor.

---

## 5. Keeping classifications current — churn, registries, deprecation

### 5.1 Deprecation notice floors (provider-published)
| Provider | Minimum notice | Notes |
|---|---|---|
| Anthropic | **≥60 days** for publicly released models (email + docs to active deployments) | Recent practice sits *at* the floor: 60–62 days for the last five retirements (Haiku 3, Haiku 3.5, Sonnet 4/Opus 4, Opus 4.1, Sonnet 4.5); earlier models got 76–189 days. Plan for 60, not for the historical average. Requests to retired models fail. |
| OpenAI | **≥6 months** GA models; **≥3 months** specialised variants (chat, Codex, deep-research); **~2 weeks** preview | Safety/compliance cases can be faster ("as much notice as reasonably possible"). 25+ model IDs retired in Jul/Oct 2026 waves; `gpt-3.5-turbo` retired 23 Oct 2026. |
| Google Gemini | **≥2 weeks** for preview models; stable models "usually don't change" | Model page lists earliest possible shutdown dates; exact date communicated in advance. Gemini 3.1 Flash-Lite shutdown announced for 7 May 2027. |
| DeepSeek | No general notice policy found in this research | `deepseek-chat` / `deepseek-reasoner` retired 24 Jul 2026 15:59 UTC (announced ~Apr 2026) — hardcoded IDs then fail outright. |
| OpenRouter | No published general retirement-notice policy for concrete slugs | "Latest Model Resolution" page carries no general policy. |

Sources: https://www.digitalapplied.com/blog/model-pinning-policy-production-automations-one-page ; https://www.flowverify.co/blog/llm-model-retirement-notice-periods-2026 ; https://intuitionlabs.ai/pdfs/changing-ai-models-validated-workflows-regression-testing.pdf ; https://github.com/dizzymii/ai-engineering-brain/blob/HEAD/23%20-%20Adoption%20&%20Blockers/Concept%20-%20Vendor%20and%20Model%20Churn%20Risk.md (all index).

### 5.2 The established practice: pin, gate, abstract
1. **Pin dated snapshots in production, never a floating alias.** Anthropic: "the underlying model remains constant for the lifetime of that [dated] ID" (e.g. `claude-opus-4-5-20251101`); a floating alias (`gpt-4o`, `claude-opus-4-5`, or Claude Code's `opus`/`sonnet` aliases, which "update over time") lets the vendor's release calendar change behaviour without a code change. Keep aliases for experiments only. (IntuitionLabs PDF; Claude Code docs verified live.)
2. **Gate every swap behind your eval suite.** "A model swap, forced or voluntary, is a deploy and gets the same regression check as a code change." Build the eval set from production traces (≥1 set per feature, scored on the same metric as today), shadow-run the replacement on live traffic (quality/latency/cost vs incumbent), then cut over. (FlowVerify; Dizzymii churn note.)
3. **Abstract the model ID behind one internal client / config** with one owner, so the ID is configuration, not a string in forty files; inventory every call site and owner. (FlowVerify; Digital Applied.)
4. **Watch for silent change, not just deprecation.** "Even without a formal deprecation, a provider can change what a model ID serves, and its behavior, with zero notice" — Chen et al. documented GPT-3.5/GPT-4 behaviour drift over months (Mar–Jun 2023). A launch-only eval cannot catch a regression introduced six months later; only a *continuous* regression eval can. (Dizzymii note, citing Chen/Zaharia/Zou and follow-up drift studies.)
5. **Budget for re-benchmarking:** "the cost of a fixed unit of capability fell roughly an order of magnitude per year across 2023–2026, which creates pressure to re-benchmark and re-optimize on a cadence most teams don't budget for" (Dizzymii note).

### 5.3 Evaluation harnesses to run the re-evaluation in
- **Inspect** (UK AI Security Institute, `UKGovernmentBEIS/inspect_ai`): composable datasets/solvers/scorers, 200+ prebuilt evals, sandboxed tool use with approval policies; adopted by METR, Apollo and other AISIs; the AISI Autonomous Systems standard mandates Inspect. The most rigorous general framework for agentic evals.
- **HELM** (Stanford CRFM): holistic — 7 core metrics (accuracy, calibration, robustness, fairness, bias, toxicity, efficiency) across 42 scenarios; standardises a multi-scenario, multi-metric reporting matrix so a single number cannot mislead.
- **lm-evaluation-harness** (EleutherAI): the standard since the GPT-3 era; versioned task configs standardise prompt rendering (prompt formatting alone can move a model between near-random and competent on the same items).
- **METR** (Model Evaluation and Threat Research): pre-deployment evals; measures capability as *task length completable at 50% success* (human baselines) — task horizon doubles roughly every 7 months (METR Time Horizon 1.1). Vivaria is being wound down in favour of Inspect.
- **LightEval** (Hugging Face), **OLMES** (Ai2), **verifiers / Environments Hub** (Prime Intellect, 2,500+ verifiable environments) are the newer reproducible runners.
Sources: https://chierhu.medium.com/building-evaluation-and-benchmarking-systems-defining-what-good-means-for-agent-quality-cc5858a81dea ; https://github.com/neurarch-ai/awesome-llm-system-design/blob/HEAD/topics/16-benchmark-evaluation.md ; https://b2aab36f.rlhf-book.pages.dev/teach/course/lec12-chap16-evals/ (index).

### 5.4 What "a model registry" looks like in practice
No provider publishes a turnkey "model registry" product for this purpose; the pattern is assembled from the practices above plus model-card documentation (§6). The repeatable mechanism is specified in Synthesis (b) below. One concrete anchor: Anthropic publishes a *retirement floor* per model on its models overview ("not before 1 Sep 2027" etc.) — a planning horizon of about a year, not a guarantee — which is the closest thing to a registry field a provider gives you for free.

---

## 6. Standards and governance

### 6.1 Model cards — the documentation unit
- **Mitchell et al., "Model Cards for Model Reporting", FAT* 2019 (arXiv:1810.03993):** "short documents accompanying trained machine learning models" reporting benchmarked evaluation across relevant conditions/groups, intended use, and out-of-scope uses. Nine sections: model details, intended use, factors, metrics, evaluation data, training data, quantitative analyses, ethical considerations, caveats and recommendations. Purpose: "clarify a model's intended use cases … and minimize use in contexts for which a model is not well suited."
- Adoption: Hugging Face Hub renders a model card as each repo's README (YAML metadata + Markdown); labs publish cards per major release (e.g. Claude 3 model card, 2024). Caveat: cards may omit compute, dataset composition or methodology; providers can invoke trade-secret claims.
- Sources: https://policywindow.org/wiki/model-card ; https://www.ultralytics.com/glossary/model-card ; https://www.snowflake.com/en/artificial-intelligence/ai-governance/model-card/ (index).

### 6.2 Frameworks
| Framework | What it is | What it says about documenting / keeping current |
|---|---|---|
| **NIST AI RMF 1.0** (Jan 2023) + **GenAI Profile (NIST-AI-600-1)** | Voluntary US framework: four functions **Govern, Map, Measure, Manage**; GenAI Profile adds 12 risk categories (incl. confabulation, dangerous content, IP, infosec) | GOVERN 1.4 cites Mitchell et al. — model cards as a transparency tool. MAP 1.1/1.6: document system characteristics; MEASURE: evaluate with documented metrics and thresholds; MANAGE: monitor, respond to incidents, update on change. No tier taxonomy. Sources: https://www.infosecguide.in/ai-governance ; https://www.layer3labs.io/guides/ai-model-safety-rules (index). |
| **ISO/IEC 42001:2023** | First certifiable international AI Management System standard (like ISO 27001 for AI) | Requires documented information (Clause 7.5), AI risk assessment + impact assessments (Clause 6), AI lifecycle processes and transparency (Clause 8, Annex A.6 system documentation), monitoring, internal audit and management review (Clause 9). Implies a *maintained* model inventory and review cadence; does not prescribe capability tiers or re-evaluation intervals. |
| **ISO/IEC 23894:2023** (AI risk management) and **38507:2022** (governance of AI) | Companion standards | Cited alongside 42001 in transparency rubrics (e.g. AI Clear methodology, https://aiclear.org/methodology, index). |
| **EU AI Act, Reg. (EU) 2024/1689, Art. 53** | Binding for general-purpose AI models (since 2 Aug 2025) | Providers must maintain technical documentation (Annex XI: intended tasks, training/testing data, evaluation results) and publish a training-data summary; the GPAI Code of Practice supplies a Model Documentation Form (intended uses, training data, energy use). This is the binding equivalent of a model card. High-risk systems need a full technical file (Art. 11). |

### 6.3 What they do *not* do
None of these frameworks defines reasoning tiers, a reclassification cadence, or a "which model for which role" rule. They require that *whatever* selection rationale you use is **documented, measured, versioned, monitored, and reviewed on change** — i.e. they govern the *process* around your registry, and the registry (§5 / Synthesis b) is how you satisfy them. A common auditor-ready mapping (per https://www.getwidget.dev/services/ai-governance/, index): model inventory + eval framework + audit logs + red-team findings + monitoring dashboard for drift/refusal/groundedness, with a versioned model/system card per deployed model (intended use, eval results, limits).

---

## Synthesis

### (a) Is "reasoning capacity" a defensible term — or is there a better one?

**Defensible as an *internal* term, with a definition attached; not an industry term, and not sufficient on its own.**

- **For it:** it is narrower and less ambiguous than "capability" (which provably also covers modalities, tool use, context, languages — Anthropic's own usage) and narrower than "intelligence" (which invites the Artificial Analysis Intelligence Index confusion and a general-intelligence reading). It names the thing you actually route on: how much multi-step, judgement-heavy work a model can do reliably. No provider term contradicts it.
- **Against it / better alternatives:** no provider, benchmark or standard defines "reasoning capacity", so it will not be understood without a glossary, and it will not map cleanly onto provider docs when a new model ships. The terms the industry *does* define and that you will have to record anyway are:
  1. **Reasoning effort** (`reasoning.effort` / `effort` / `thinking_level` / `reasoning_effort`) — the per-request dial, with provider-defined levels. This is the dimension you actually *set*.
  2. **Reasoning model** (OpenAI's defined term) — the model class that spends internal reasoning tokens.
  3. **Test-time compute** — the mechanism (research term).
- **Recommendation:** use a two-part label internally, e.g. **"reasoning tier"** defined as *a (model, effort) pair's measured performance on your coding/reasoning evals* — not a model name. Define it in the registry as: tier = {pinned model ID, effort setting, benchmark evidence (suite + version + harness + date + score), cost/latency, role}. That keeps the word "reasoning" (accurate), avoids the overloaded "capability", and makes the tier *measured* rather than *named*. If you must use one word in code, `reasoning_tier` is better than `capability_tier`; `reasoning_capacity` is acceptable as the *score* (a number) behind the tier, not as the tier name itself. Document the definition in the registry's glossary and cite OpenAI's reasoning-model definition as the anchor.

### (b) A repeatable reclassification mechanism

Assemble it from §5.2 + §6: **pin, measure, gate, review on triggers, not just on a calendar.**

**Registry fields (one row per (pinned model ID, effort) actually deployed):**
- Identity: provider, pinned model ID (dated snapshot), floating alias it was resolved from (if any), weights/API, context window, max output, knowledge cutoff, modalities / tool-use support.
- Tier assignment: reasoning tier (per (a)), role(s) it may serve, effort default + allowed range, cost ($/MTok in/out, cached, batch), observed latency (p50/p95 on your traffic).
- Evidence: benchmark scores *with* suite name + version + harness + effort + date + run ID (public: SWE-bench Verified, Terminal-Bench, GPQA Diamond, AIME, AA Intelligence Index version; plus your own task evals — these govern, public ones corroborate), owner, and a link to the raw run log.
- Lifecycle: release date, provider retirement floor / deprecation notice date, notice period applicable, replacement candidate, status (current / candidate / legacy / retired).
- Governance: model-card / system-card link, intended use + out-of-scope uses, known failure modes, risk notes (NIST AI RMF Map / ISO 42001 Annex A.6).

**Evidence sources (in priority order):** (1) your own evals in a pinned harness (Inspect / HELM / lm-eval-harness) on production-trace tasks — the only evidence that governs a swap; (2) independent composites with private sets (Artificial Analysis Intelligence Index, versioned; LMArena for preference, not correctness); (3) provider model cards / system cards and benchmark claims (treat as vendor claims, verify); (4) fresh/rolling benchmarks (LiveCodeBench, MathArena) to detect drift.

**Review triggers (any one forces a re-evaluation, regardless of cadence):** provider deprecation/retirement notice; new model release in the same family or a competitor at the same price point; price change; provider retirement-floor change; your own eval regression or a silent-drift signal (continuous regression eval); benchmark-suite version change (e.g. AA Index v4→v5) invalidating a cited score; a role's failure pattern changing (e.g. coordinator misroutes spike).

**Cadence:** monthly lightweight scan (provider model/pricing/deprecation pages + AA Index / LMArena movement, ~1 hour, logged); quarterly re-run of your own eval suite against all registry entries + any new candidates, with a written reclassification decision (keep / promote / demote / retire) per entry; annual governance review (model cards refreshed, NIST/ISO mapping checked). On a trigger, run the swap gate (§5.2 step 2: shadow → compare → cut over) before the provider's notice window expires — and plan for Anthropic's 60-day floor, the shortest credible window.

### (c) Industry practice that contradicts a cheap-coordinator / strong-escalation split

The split is a reasonable *default*, but four established patterns cut against it as a rigid rule:

1. **The strong model plans, the cheap model executes — the reverse split is the vendor's own pattern.** Claude Code's `opusplan` uses **Opus in plan mode and Sonnet in execution** (verified live, Claude Code docs): the expensive model does the *thinking* (architecture, decomposition) and the cheaper model does the *volume* (code generation). If your "coordinator" is the planner, the industry pattern puts the *strong* model there and the cheap model on the workers — the opposite of "cheap coordinator, strong workers". The same logic appears in OpenCode's `small_model` for lightweight work and in the general advice that Haiku is for "classification, extraction, and routing" (Anthropic) — i.e. cheap models are for *well-scoped, verifiable* sub-tasks, not for open-ended judgement.
2. **The coordinator's job *is* judgement — and router quality drives the whole result.** Routing research is explicit: the quality estimator "drives most of the result"; a misrouting coordinator silently ships weak answers or wastes money, and a cost-only router "collapses quality nobody approved". A weak coordinator that cannot tell hard from easy, or cannot judge a worker's output, is the single point of failure for the cascade. Cursor's answer is an ML classifier trained for the task, not a small general model asked to route ad hoc. If the coordinator cannot reliably *assess difficulty and verify outputs*, the split fails — put enough reasoning in the coordinator (or in a dedicated verifier/judge) to make the routing decision trustworthy, even if that costs more per coordinator call.
3. **The tier gap is narrowing and effort is a dial — "strong" is not a model, it's a setting.** Sonnet 5.5 sits within ~2 points of Opus 5.5 on the AA Intelligence Index; Gemini Flash beat Pro on SWE-bench Verified; a smaller model at `high`/`max` effort can out-reason a larger model at `low`/`medium` (Snell et al.: test-time compute can beat a 14× larger model). A rigid cheap/strong *model* split wastes the cheaper option of raising effort on the mid-tier model, and will be inverted by the next release. Route on *measured (model, effort)* performance, not on tier names.
4. **Cheap-model ensembles and verification can beat a single strong model — escalation is not always the answer.** FrugalGPT's cascade matched GPT-4 at up to 98% lower cost; OpenRouter's Fusion runs several cheap models in parallel and aggregates (vendor-claimed to beat single frontier models on reasoning/coding); RouteLLM kept 95% of GPT-4 quality while calling GPT-4 on only 14–26% of prompts. The implication: for *verifiable* coding work (tests pass/fail), a cheap worker + a good verifier/judge often beats a strong worker with no verification — so the scarce resource is verification/judgement capacity, not raw worker strength. Conversely, cascades double p95 latency on escalated requests, so for latency-sensitive coordination a pre-call router (or a strong coordinator that routes correctly first time) beats a cheap-first cascade.
5. **Harness > model.** The same model scores 0% vs 97% depending on harness (ARC-AGI-3 / Opus 4.6). A strong model in a poor harness underperforms a mid model in a good one. Investment in the harness, tools, context management and verification will move outcomes more than the coordinator/worker tier split.

**Net:** keep the cheap-coordinator / strong-escalation shape as a *cost default*, but (i) give the coordinator enough reasoning (or a dedicated judge) to route and verify reliably, (ii) treat `opusplan` as the counter-pattern to copy for planning-heavy work (strong plans, cheap executes), and (iii) re-derive the split from your own evals each quarter — it is a measured assignment, not an org chart.

---

## Could not verify / open questions

- **Exact current prices for OpenAI GPT-6 / GPT-5.6 and Google Gemini 4 Argon** from the providers' own pricing pages: this research verified OpenAI's reasoning guide live, but pricing was taken from secondary summaries (Memetik, Sep 2026; Second Talent) and Google pricing from secondary guides (Oct 2026). Re-check https://platform.openai.com/docs/pricing and Google Cloud pricing before budgeting.
- **xAI and DeepSeek deprecation policies:** no general published notice period was found for either in this research; DeepSeek's 24 Jul 2026 retirement is documented, xAI's is not.
- **"Reasoning capacity" as a term:** no provider or standards source was found using it as a defined label — the verdict in (a) rests on that absence (searched provider docs and secondary literature), not on a source that explicitly rejects it.
- **Model-specific benchmark scores** (e.g. exact SWE-bench Verified % per current model) are deliberately not quoted as a table here: scores depend on harness/effort/date and go stale in weeks; the cited leaderboards (Artificial Analysis, LMArena, SWE-bench) should be read live with their version noted.
- **RouterArena / LLMRouterBench primary papers** were reached via secondary summaries (Seangeng, Dreaming Press) rather than the primary benchmark papers; the "no router optimal / 12th place" finding should be confirmed against the primary source before citing it externally.

## Sources

Full list (title — publisher — URL — date read / status):

1. OpenAI, *Reasoning models* — OpenAI Platform Docs — https://platform.openai.com/docs/guides/reasoning — verified live 10 Nov 2026.
2. Anthropic, *Models overview* — Claude Platform Docs — https://platform.claude.com/docs/en/models/overview?ref=toolarium.ru — index, crawled 10 Nov 2026.
3. Anthropic, *Model configuration* (Claude Code Docs) — https://code.claude.com/docs/en/model-config — verified live 10 Nov 2026.
4. Anthropic, *Adaptive Thinking* (docs mirror) — https://github.com/sullivan-street-projects/anthropic-docs-local/blob/HEAD/api/adaptive-thinking.md — index.
5. *Deep reasoning in Claude Code: effort levels and extended thinking* — Developer Toolkit — https://developertoolkit.ai/en/claude-code/quick-start/deep-reasoning/ — index.
6. *Gemini models* — Neurals — https://www.neurals.ca/tech/gemini/gemini-models/ — index.
7. *Which Gemini Model Should You Use?* — GE Roundtable — https://geroundtable.com/which-gemini-model-to-use/ — index.
8. *Gemini 3.1 Pro vs Flash vs Flash-Lite Compared* — AI Toolbox — https://www.ai-toolbox.co/gemini-models/gemini-3-1-pro-vs-flash-vs-flash-lite-2026 — index.
9. *Gemini Deep Think* — Ashita Orbis — https://raw.ashitaorbis.com/understanding-ai/wiki/gemini-deep-think — index.
10. *OpenAI GPT-5 API pricing: every model, September 2026* — Memetik — https://www.memetik.ai/guides/gpt-5-api-pricing — index.
11. *Every OpenAI Model Explained and Compared (Sep, 2026)* — Second Talent — https://www.secondtalent.com/resources/every-openai-model-explained-compared/ — index.
12. *Gemini vs Claude vs Grok vs ChatGPT (and DeepSeek) APIs for Business* — Kunal Vohra — https://kunalvohra.com/blog/gemini-vs-claude-vs-grok-vs-chatgpt-api-for-business — index.
13. *DeepSeek V4 Guide: API, Thinking, Caching, Cost* — AIDevOps — https://www.aidevops.us/deepseek/ — index.
14. *DeepSeek API — Ground Truth* — eth-lecture-copilot (GitHub) — https://github.com/krol05/eth-lecture-copilot/blob/HEAD/docs/providers/deepseek.md — index.
15. *What Are Reasoning Models? How They Differ From Standard LLMs* — AI News Media — https://www.ai-news.media/posts/what-are-reasoning-models-how-they-differ-from-standard-llms — index (cites Snell et al. arXiv:2408.03314).
16. *Benchmarks and leaderboards* — Seminal AI — https://seminal.ai/reference/categories/benchmarks-and-leaderboards/ — index, data checked 2026-09-06.
17. *AI Benchmarks Explained: MMLU, GPQA* — Memx — https://memx.app/blog/ai-benchmarks-explained-mmlu-gpqa-swe-bench/ — index.
18. *Evaluating LLMs in 2026* — Rugvai Labs — https://www.rugvailabs.com/tracks/track-05-transformers/evaluation-frontiers-2026 — index.
19. *AI Benchmark Literacy* — Institute PM — https://www.institutepm.com/knowledge-hub/ai-benchmark-literacy-guide — index.
20. *LLM Rankings* — AI Wiki — https://aiwiki.ai/wiki/llm_rankings — index.
21. *Artificial Analysis Explained* — Scaler — https://www.scaler.com/topics/artificial-analysis-explained/ — index.
22. *How Artificial Analysis built its nine-test Intelligence Index v5* — IO Digest — https://www.iodigest.com/article/artificial-analysis-intelligence-index-v5-0-50269651 — index, 28 Oct 2026.
23. *LLM Routing and Model Cascades: The Complete Guide* — Prompt20 — https://blog.prompt20.com/posts/llm-routing-and-model-cascades-complete-guide/ — index.
24. *What is LLM routing? Model routers and cascades, explained* — Just Malhar — https://www.justmalhar.com/blog/what-is-llm-routing/ — index (cites FrugalGPT arXiv:2305.05176; RouteLLM arXiv:2406.18665; Hybrid LLM).
25. *Model Routing and Cascades for LLM Cost* — TMLS — https://www.tmls.nyc/research/model-routing-cascades — index.
26. *RouteLLM vs NotDiamond vs Martian* — Dreaming Press — https://dreaming.press/posts/2026-06-21-routellm-vs-notdiamond-vs-martian.html — index, 21 Jun 2026.
27. *The honest guide to LLM model routing* — Sean Geng — https://seangeng.com/writing/the-honest-guide-to-llm-routing — index.
28. *Model Routing for Coding Agents, Explained* — AQ — https://aq.dev/guides/model-routing-for-coding-agents-explained/ — index, 15 Oct 2026.
29. *Model-Routing Specialists — Martian, Not Diamond, Unify* — Gateway research (GitHub) — https://github.com/mayanpathak/gateway/blob/HEAD/docs/research/routing-specialists.md — index.
30. *A One-Page Model-Pinning Policy for Production Automations* — Digital Applied — https://www.digitalapplied.com/blog/model-pinning-policy-production-automations-one-page — index, 21 Aug 2026.
31. *LLM Model Retirement: 60 Days vs Six Months* — FlowVerify — https://www.flowverify.co/blog/llm-model-retirement-notice-periods-2026 — index.
32. *Changing AI Models in Validated Workflows: Regression Testing* — IntuitionLabs — https://intuitionlabs.ai/pdfs/changing-ai-models-validated-workflows-regression-testing.pdf — index, 5 Sep 2026.
33. *Concept — Vendor and Model Churn Risk* — AI Engineering Brain (GitHub) — https://github.com/dizzymii/ai-engineering-brain/blob/HEAD/23%20-%20Adoption%20&%20Blockers/Concept%20-%20Vendor%20and%20Model%20Churn%20Risk.md — index.
34. *Building evaluation and benchmarking systems* — Chier Hu (Medium) — https://chierhu.medium.com/building-evaluation-and-benchmarking-systems-defining-what-good-means-for-agent-quality-cc5858a81dea — index, 2 Jul 2026.
35. *Lecture 12: The Evolution of Frontier Model Evaluation* — RLHF Book — https://b2aab36f.rlhf-book.pages.dev/teach/course/lec12-chap16-evals/ — index.
36. *Benchmarking a model* (awesome-llm-system-design) — GitHub — https://github.com/neurarch-ai/awesome-llm-system-design/blob/HEAD/topics/16-benchmark-evaluation.md — index.
37. *Model Card* — Policy Window — https://policywindow.org/wiki/model-card — index (cites Mitchell et al. 2019, arXiv:1810.03993).
38. *What Is a Model Card?* — Snowflake — https://www.snowflake.com/en/artificial-intelligence/ai-governance/model-card/ — index.
39. *AI Governance | NIST AI RMF, ISO 42001, EU AI Act* — Infosec Guide — https://www.infosecguide.in/ai-governance — index.
40. *AI Model Safety Rules (2026): NIST, ISO 42001, EU AI Act* — Layer3 Labs — https://www.layer3labs.io/guides/ai-model-safety-rules — index.
41. *Methodology* (AI Clear rubric v3.1) — https://aiclear.org/methodology — index, Jun 2026.
42. *Claude Opus vs Sonnet vs Haiku* — CCAPI Blog — https://ccapi.ai/blog/claude-opus-vs-sonnet-vs-haiku — index.
43. *AI Model Pricing — tier by tier* — AITokenomics — https://pricing.aitokenomics.app/ — index.
44. *Claude Models Compared* — Geotoolbox — https://geotoolbox.ai/blog/claude-models-compared — index, 9 Nov 2026.

Working notes: `notes/source-01-provider-docs.md`, `notes/source-02-benchmarks-routing.md`, `notes/source-03-churn-governance.md`.
