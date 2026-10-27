//! The prompt cache's clock, and keeping it warm.
//!
//! Anthropic keeps a cached prompt for a fixed time after its last use (5 minutes, or 1 hour when
//! the request asks for it). A prompt sent after that re-writes the whole context at the higher
//! cache-write price. The session tracks when the cache was last used so the UI can count down,
//! and `/keepwarm` (or its auto mode) sends a tiny request over the same prefix to restart the
//! clock for the price of a cache read.

use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use patchbay_llm::Message;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::reqlog::ReqTimer;
use crate::session::Session;
use crate::store::Entry;

/// Auto mode refreshes once the cache has this long left.
pub const WARN_SECS: u64 = 300;
/// A context smaller than this is not worth keeping warm: re-caching it costs next to nothing.
pub const BIG_CONTEXT_TOKENS: u64 = 30_000;
/// Refreshes auto mode makes per idle stretch (reset by a real request).
pub const AUTO_LIMIT: u32 = 3;
/// How often auto mode looks at the clock.
const TICK: Duration = Duration::from_secs(30);
/// The refresh request's only message.
const ASK: &str = "Reply with just: ok";

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `85000` as `85k`.
pub fn kilo(tokens: u64) -> String {
    format!("{}k", (tokens as f64 / 1000.0).round() as u64)
}

/// When the session's prompt cache was last used and what to do about it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CacheState {
    /// Unix milliseconds of the last request that read or wrote the cache. `None`: not known to
    /// be warm.
    pub last_at_ms: Option<u64>,
    /// How long the model's cache lives, when the client can know (Anthropic).
    pub ttl_secs: Option<u64>,
    /// Refresh automatically near expiry.
    pub auto: bool,
    /// Refreshes made since the last real request.
    pub auto_runs: u32,
}

impl CacheState {
    /// Milliseconds left before the cache expires; negative once it has.
    pub fn remaining_ms(&self, now_ms: u64) -> Option<i64> {
        let (at, ttl) = (self.last_at_ms?, self.ttl_secs?);
        Some((at + ttl * 1000) as i64 - now_ms as i64)
    }

    /// A request used the cache. `refresh`: it was a keep-warm request, not the user's work.
    pub fn touch(&mut self, now_ms: u64, ttl_secs: Option<u64>, refresh: bool) {
        self.ttl_secs = ttl_secs;
        self.last_at_ms = ttl_secs.map(|_| now_ms);
        if !refresh {
            self.auto_runs = 0;
        }
    }

    /// Forget that the cache is warm (the history or the model changed under it).
    pub fn reset(&mut self) {
        self.last_at_ms = None;
        self.auto_runs = 0;
    }

    /// Whether auto mode should refresh now: it is on, the cache is warm but close to expiry, the
    /// context is big enough to matter, and this idle stretch has refreshed fewer than
    /// [`AUTO_LIMIT`] times. Takes a refresh slot when it says yes.
    pub fn take_auto_slot(&mut self, now_ms: u64, context_tokens: u64) -> bool {
        let close = self.remaining_ms(now_ms).is_some_and(|left| left > 0 && left <= (WARN_SECS * 1000) as i64);
        let due = self.auto && close && context_tokens >= BIG_CONTEXT_TOKENS && self.auto_runs < AUTO_LIMIT;
        if due {
            self.auto_runs += 1;
        }
        due
    }

    /// What the client shows (`usage_update`'s `_meta.cache`); `None` when there is no clock.
    pub fn meta(&self) -> Option<Value> {
        let ttl = self.ttl_secs?;
        Some(json!({"lastAt": self.last_at_ms, "ttlSecs": ttl, "auto": self.auto}))
    }
}

/// What a refresh found.
#[derive(Debug, PartialEq)]
pub struct Warmed {
    pub read: u64,
    pub written: u64,
    pub ttl_secs: u64,
}

impl Warmed {
    /// The line `/keepwarm` answers with.
    pub fn say(&self) -> String {
        let minutes = self.ttl_secs / 60;
        if self.read + self.written == 0 {
            "The endpoint reported no cache use, so it may not support prompt caching for this model.".to_owned()
        } else if self.read > 0 {
            format!("Cache refreshed: {} tokens read from cache, good for another {minutes}m.", kilo(self.read))
        } else {
            format!("The cache had gone cold, so this re-cached {} tokens. It is good for {minutes}m.", kilo(self.written))
        }
    }
}

impl Session {
    /// Tell the client the cache clock (a `usage_update` with only `_meta.cache`).
    pub fn announce_cache(&self) {
        if let Some(meta) = self.cache.lock().unwrap().meta() {
            self.update(json!({"sessionUpdate": "usage_update", "_meta": {"cache": meta}}));
        }
    }

    /// A model request used the cache. `ttl_secs` is the model's cache lifetime, if it has one.
    pub fn cache_used(&self, ttl_secs: Option<u64>, refresh: bool) {
        self.cache.lock().unwrap().touch(now_ms(), ttl_secs, refresh);
    }

    /// The history or the model changed, so the cache no longer matches.
    pub fn cache_reset(&self) {
        self.cache.lock().unwrap().reset();
        self.announce_cache();
    }
}

/// Send the session's history again with a one-line question, and drop the answer. The request
/// is the same as the next real one up to its last message, so the provider reads the cached
/// prefix and restarts its clock.
pub async fn keep_warm(session: &Arc<Session>, cancel: &CancellationToken) -> Result<Warmed, String> {
    let (model_id, mode, mut messages) = {
        let st = session.state.lock().await;
        (st.model_id.clone(), st.mode, st.messages.clone())
    };
    if messages.is_empty() {
        return Err("Nothing is cached yet.".into());
    }
    let mc = session.model_client(&model_id).await?;
    let Some(ttl_secs) = mc.client.config.cache_ttl_secs() else {
        return Err("The prompt cache can only be refreshed on Anthropic models.".into());
    };
    messages.push(Message::user(ASK));
    let (_, request) = crate::turn::build_request(session, &mc, &model_id, mode, messages).await;
    let mut timer = ReqTimer::start();
    let result = mc.client.complete(&request, cancel, |ev| timer.event(&ev)).await;
    session.store.append(&Entry::Req(timer.finish(&mc.client.config, Some("keepwarm"), &result)));
    let response = result.map_err(|e| if e.cancelled { "Cancelled.".to_owned() } else { format!("The refresh failed: {e}") })?;
    let usage = response.usage.unwrap_or_default();
    if usage.cached_read + usage.cache_write > 0 {
        session.cache_used(Some(ttl_secs), true);
        session.announce_cache();
    }
    Ok(Warmed { read: usage.cached_read, written: usage.cache_write, ttl_secs })
}

/// Look at the clock every [`TICK`] and refresh the cache in auto mode. Ends with the session.
pub fn spawn_auto(session: &Arc<Session>) {
    let weak: Weak<Session> = Arc::downgrade(session);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(session) = weak.upgrade() else { return };
            if session.busy.load(std::sync::atomic::Ordering::SeqCst) {
                continue;
            }
            let context = session.state.lock().await.context_tokens;
            let due = session.cache.lock().unwrap().take_auto_slot(now_ms(), context);
            if due {
                // A failed refresh still used its slot, so a broken endpoint is not hammered.
                let _ = keep_warm(&session, &CancellationToken::new()).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warm(at: u64, ttl: u64) -> CacheState {
        CacheState { last_at_ms: Some(at), ttl_secs: Some(ttl), auto: true, auto_runs: 0 }
    }

    #[test]
    fn counts_down_from_the_last_use() {
        let s = warm(1_000_000, 3600);
        assert_eq!(s.remaining_ms(1_000_000), Some(3_600_000));
        assert_eq!(s.remaining_ms(1_000_000 + 3_600_000), Some(0));
        assert_eq!(s.remaining_ms(1_000_000 + 3_700_000), Some(-100_000));
        assert_eq!(CacheState::default().remaining_ms(5), None);
    }

    #[test]
    fn a_real_request_restarts_the_clock_and_the_auto_budget() {
        let mut s = warm(0, 3600);
        s.auto_runs = 3;
        s.touch(10, Some(3600), true);
        assert_eq!((s.last_at_ms, s.auto_runs), (Some(10), 3));
        s.touch(20, Some(3600), false);
        assert_eq!((s.last_at_ms, s.auto_runs), (Some(20), 0));
        // A model with no known lifetime has no clock.
        s.touch(30, None, false);
        assert_eq!(s.last_at_ms, None);
        assert_eq!(s.meta(), None);
    }

    #[test]
    fn auto_refreshes_only_a_warm_big_cache_near_expiry_and_only_three_times() {
        let ttl_ms = 3_600_000;
        let near = ttl_ms - 200_000;
        let mut s = warm(0, 3600);
        // Plenty of time left.
        assert!(!s.take_auto_slot(1_000, 50_000));
        // Close to expiry, but the context is small.
        assert!(!s.take_auto_slot(near, 29_999));
        // Already cold: a refresh would re-cache, not refresh.
        assert!(!s.take_auto_slot(ttl_ms + 1, 50_000));
        for _ in 0..AUTO_LIMIT {
            assert!(s.take_auto_slot(near, 50_000));
        }
        assert!(!s.take_auto_slot(near, 50_000));
        // Auto off.
        let mut off = CacheState { auto: false, ..warm(0, 3600) };
        assert!(!off.take_auto_slot(near, 50_000));
    }

    #[test]
    fn reset_forgets_the_warm_cache_but_keeps_the_setting() {
        let mut s = warm(5, 300);
        s.reset();
        assert_eq!(s.last_at_ms, None);
        assert!(s.auto);
        assert_eq!(s.meta().unwrap(), json!({"lastAt": null, "ttlSecs": 300, "auto": true}));
    }

    #[test]
    fn says_what_the_refresh_found() {
        assert_eq!(
            Warmed { read: 85_400, written: 0, ttl_secs: 3600 }.say(),
            "Cache refreshed: 85k tokens read from cache, good for another 60m."
        );
        assert_eq!(
            Warmed { read: 0, written: 12_000, ttl_secs: 300 }.say(),
            "The cache had gone cold, so this re-cached 12k tokens. It is good for 5m."
        );
        assert!(Warmed { read: 0, written: 0, ttl_secs: 300 }.say().contains("no cache use"));
        assert_eq!(kilo(499), "0k");
        assert_eq!(kilo(1500), "2k");
    }
}
