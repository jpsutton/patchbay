//! Token usage bookkeeping shared by Chat (per message) and Code (per agent session).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use xai_grok_sampling_types::TokenUsage;

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

    pub fn from_sampler(u: &TokenUsage, cost_usd_ticks: Option<i64>) -> Tokens {
        Tokens {
            input: u.prompt_tokens as u64,
            cached_read: u.cached_prompt_tokens as u64,
            cache_write: u.cache_creation_prompt_tokens as u64,
            output: u.completion_tokens as u64,
            reasoning: u.reasoning_tokens as u64,
            // The sampler reports cost in ticks of 1e-10 USD.
            cost_usd: cost_usd_ticks.filter(|t| *t > 0).map(|t| t as f64 / 1e10),
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
    }

    /// Fold in Grok Build's `session_status` update (its status-line payload).
    pub fn apply_status(&mut self, status: &Value) {
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
