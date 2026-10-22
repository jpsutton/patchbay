//! Token usage bookkeeping shared by Chat (per message) and Code (per agent session).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Token counts for one call or a running total. `input` includes the cached buckets.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Tokens {
    pub input: u64,
    pub cached_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cost_usd: Option<f64>,
    pub api_ms: u64,
}

impl Tokens {
    pub fn add(&self, o: &Tokens) -> Tokens {
        Tokens {
            input: self.input + o.input,
            cached_read: self.cached_read + o.cached_read,
            cache_write: self.cache_write + o.cache_write,
            output: self.output + o.output,
            reasoning: self.reasoning + o.reasoning,
            cost_usd: match (self.cost_usd, o.cost_usd) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
            },
            api_ms: self.api_ms + o.api_ms,
        }
    }

    pub fn from_llm(u: &patchbay_llm::Usage) -> Tokens {
        Tokens {
            input: u.input,
            cached_read: u.cached_read,
            cache_write: u.cache_write,
            output: u.output,
            reasoning: u.reasoning,
            cost_usd: u.cost_usd.filter(|c| *c > 0.0),
            api_ms: 0,
        }
    }
}

/// Persisted usage for a code session. Grok Build's ledger resets with each agent process, so
/// totals from earlier processes are folded into `baseline` when a new one starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CodeUsageState {
    pub baseline: Tokens,
    pub current: Tokens,
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub auto_compact_percent: Option<u64>,
    /// This process reports totals through Grok's status line, so per-turn usage would double
    /// count.
    #[serde(skip)]
    pub status_line: bool,
}

/// What the UI renders below the composer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageView {
    pub totals: Tokens,
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub auto_compact_percent: Option<u64>,
}

impl CodeUsageState {
    pub fn view(&self) -> UsageView {
        UsageView {
            totals: self.baseline.add(&self.current),
            context_tokens: self.context_tokens,
            context_window: self.context_window,
            auto_compact_percent: self.auto_compact_percent,
        }
    }

    /// A new agent process starts its own ledger at zero.
    pub fn roll_over(&mut self) {
        self.baseline = self.baseline.add(&self.current);
        self.current = Tokens::default();
        self.status_line = false;
    }

    /// Fold in ACP's `usage_update`: context fill and the process's cumulative cost. Ignored
    /// once Grok's status line reports the same numbers.
    pub fn apply_context(&mut self, update: &Value) {
        if self.status_line {
            return;
        }
        if let Some(t) = update["used"].as_u64() {
            self.context_tokens = Some(t);
        }
        if let Some(w) = update["size"].as_u64().filter(|w| *w > 0) {
            self.context_window = Some(w);
        }
        // Patchbay's own agent says where it compacts; Grok reports that in its status line.
        if let Some(p) = update["_meta"]["autoCompactPercent"].as_u64() {
            self.auto_compact_percent = Some(p);
        }
        if let Some(c) = update["cost"]["amount"].as_f64()
            && update["cost"]["currency"].as_str().is_none_or(|c| c.eq_ignore_ascii_case("USD"))
        {
            self.current.cost_usd = Some(c);
        }
    }

    /// Add one turn's ACP `PromptResponse.usage`.
    pub fn apply_turn(&mut self, usage: &Value) {
        if self.status_line {
            return;
        }
        let n = |k: &str| usage[k].as_u64().unwrap_or(0);
        let (input, output, thought) = (n("inputTokens"), n("outputTokens"), n("thoughtTokens"));
        let (cached_read, cache_write) = (n("cachedReadTokens"), n("cachedWriteTokens"));
        // `input` here includes the cache buckets. Agents differ on whether ACP's
        // `inputTokens` already does; when the total only adds up with the buckets on top,
        // it doesn't.
        let cache = cached_read + cache_write;
        let input = if cache > 0 && n("totalTokens") >= input + output + cache {
            input + cache
        } else {
            input
        };
        self.current = self.current.add(&Tokens {
            input,
            cached_read,
            cache_write,
            output,
            reasoning: thought,
            cost_usd: None,
            api_ms: 0,
        });
    }

    /// Fold in Grok Build's `session_status` update (its status-line payload).
    pub fn apply_status(&mut self, status: &Value) {
        self.status_line = true;
        let cw = &status["context_window"];
        let u = &cw["session_usage"];
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        if u.is_object() {
            let cached_read = n(&u["cache_read_input_tokens"]);
            let cache_write = n(&u["cache_creation_input_tokens"]);
            self.current = Tokens {
                // The status line reports fresh input separately from the cache buckets.
                input: n(&u["input_tokens"]) + cached_read + cache_write,
                cached_read,
                cache_write,
                output: n(&u["output_tokens"]),
                reasoning: 0,
                cost_usd: status["cost"]["total_cost_usd"].as_f64(),
                api_ms: n(&status["cost"]["total_api_duration_ms"]),
            };
        }
        if let Some(t) = cw["context_tokens"].as_u64() {
            self.context_tokens = Some(t);
        }
        if let Some(w) = cw["context_window_size"].as_u64() {
            self.context_window = Some(w);
        }
        if let Some(p) = cw["auto_compact_threshold_percent"].as_u64() {
            self.auto_compact_percent = Some(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(input: u64, cached: u64, out: u64, ctx: u64) -> Value {
        serde_json::json!({
            "cost": { "total_api_duration_ms": 10 },
            "context_window": {
                "context_window_size": 32000, "context_tokens": ctx,
                "auto_compact_threshold_percent": 85,
                "session_usage": {
                    "input_tokens": input, "output_tokens": out,
                    "cache_creation_input_tokens": 0, "cache_read_input_tokens": cached
                }
            }
        })
    }

    #[test]
    fn acp_usage_folds_into_totals() {
        let mut s = CodeUsageState::default();
        let turn = serde_json::json!({
            "totalTokens": 1150, "inputTokens": 100, "outputTokens": 50,
            "cachedReadTokens": 900, "cachedWriteTokens": 100
        });
        s.apply_turn(&turn);
        s.apply_turn(&serde_json::json!({ "totalTokens": 30, "inputTokens": 20, "outputTokens": 10 }));
        s.apply_context(&serde_json::json!({ "used": 1200, "size": 200000, "cost": { "amount": 0.25, "currency": "USD" } }));
        let v = s.view();
        assert_eq!(v.totals.input, 1120);
        assert_eq!(v.totals.cached_read, 900);
        assert_eq!(v.totals.output, 60);
        assert_eq!(v.totals.cost_usd, Some(0.25));
        assert_eq!(v.context_tokens, Some(1200));
        assert_eq!(v.context_window, Some(200000));
        // Once the status line reports totals, per-turn usage is ignored.
        s.apply_status(&status(10, 0, 5, 15));
        s.apply_turn(&turn);
        assert_eq!(s.view().totals.input, 10);
    }

    #[test]
    fn status_counts_cached_as_input() {
        let mut s = CodeUsageState::default();
        s.apply_status(&status(100, 400, 20, 520));
        let v = s.view();
        assert_eq!(v.totals.input, 500);
        assert_eq!(v.totals.cached_read, 400);
        assert_eq!(v.totals.output, 20);
        assert_eq!(v.context_tokens, Some(520));
        assert_eq!(v.context_window, Some(32000));
        assert_eq!(v.auto_compact_percent, Some(85));
    }

    #[test]
    fn totals_survive_agent_restart() {
        let mut s = CodeUsageState::default();
        s.apply_status(&status(100, 0, 10, 110));
        s.roll_over();
        // The new process's ledger starts over.
        s.apply_status(&status(50, 0, 5, 165));
        let v = s.view();
        assert_eq!(v.totals.input, 150);
        assert_eq!(v.totals.output, 15);
        assert_eq!(v.context_tokens, Some(165));
    }
}
