//! Timing a model call for the session's request log (see [`ReqRec`]).

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use patchbay_llm::{Config, Event, Response, wire};

use crate::store::{ReqOutcome, ReqRec};

/// Started when a call begins; fed its events; turned into a record when the call ends.
pub struct ReqTimer {
    started: Instant,
    at: u64,
    first_ms: Option<u64>,
    retries: u32,
}

impl ReqTimer {
    pub fn start() -> Self {
        let at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        ReqTimer { started: Instant::now(), at, first_ms: None, retries: 0 }
    }

    /// Call for every event the client emits.
    pub fn event(&mut self, ev: &Event) {
        match ev {
            Event::Text(_) | Event::Reasoning(_) => {
                self.first_ms.get_or_insert_with(|| self.started.elapsed().as_millis() as u64);
            }
            Event::Retrying { attempt, .. } => self.retries = *attempt,
            Event::ToolCall(_) | Event::Search(_) => {}
        }
    }

    /// The record for a finished call. `kind` names what the call was for, unless it was a step
    /// of the turn.
    pub fn finish(self, cfg: &Config, kind: Option<&str>, result: &Result<Response, patchbay_llm::Error>) -> ReqRec {
        let mut rec = ReqRec {
            at: self.at,
            kind: kind.map(str::to_owned),
            model: cfg.model.clone(),
            max_tokens: wire::max_output_tokens(cfg),
            outcome: ReqOutcome::Ok,
            stop: None,
            usage: None,
            first_ms: self.first_ms,
            total_ms: self.started.elapsed().as_millis() as u64,
            retries: self.retries,
            error: None,
            status: None,
        };
        match result {
            Ok(r) => {
                rec.stop = Some(r.stop.clone());
                rec.usage = r.usage;
            }
            Err(e) if e.cancelled => rec.outcome = ReqOutcome::Cancelled,
            Err(e) => {
                rec.outcome = ReqOutcome::Error;
                rec.error = Some(e.message.clone());
                rec.status = e.status;
            }
        }
        rec
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use patchbay_llm::{Message, StopReason, Usage, WireFormat};

    fn cfg() -> Config {
        Config::new(WireFormat::Anthropic, "https://api.anthropic.com/v1", "claude-fable-5-1")
    }

    #[test]
    fn records_a_reply_with_its_cap_stop_and_usage() {
        let mut t = ReqTimer::start();
        t.event(&Event::Retrying { attempt: 1, max: 2, reason: "x".into() });
        t.event(&Event::Reasoning("hm".into()));
        let usage = Usage { input: 10, output: 3, ..Default::default() };
        let ok = Ok(Response {
            message: Message::assistant("hi"),
            usage: Some(usage),
            stop: StopReason::MaxTokens,
            model: None,
            plan: None,
        });
        let rec = t.finish(&cfg(), None, &ok);
        assert_eq!(rec.outcome, ReqOutcome::Ok);
        assert_eq!(rec.max_tokens, Some(64000));
        assert_eq!(rec.stop, Some(StopReason::MaxTokens));
        assert_eq!(rec.usage, Some(usage));
        assert_eq!(rec.retries, 1);
        assert!(rec.first_ms.is_some());
        assert_eq!(rec.model, "claude-fable-5-1");
    }

    #[test]
    fn records_a_failure_and_a_stop() {
        let err = Err(patchbay_llm::Error {
            message: "no data for 180s".into(),
            status: None,
            retryable: true,
            cancelled: false,
        });
        let rec = ReqTimer::start().finish(&cfg(), Some("compact"), &err);
        assert_eq!(rec.outcome, ReqOutcome::Error);
        assert_eq!(rec.error.as_deref(), Some("no data for 180s"));
        assert_eq!(rec.kind.as_deref(), Some("compact"));
        assert_eq!(rec.first_ms, None);

        let stopped = Err(patchbay_llm::Error::cancelled());
        assert_eq!(ReqTimer::start().finish(&cfg(), None, &stopped).outcome, ReqOutcome::Cancelled);
    }
}
