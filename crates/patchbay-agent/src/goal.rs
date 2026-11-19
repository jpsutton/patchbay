//! `/goal`: a standing objective the harness keeps working toward across rounds.
//!
//! The model does not decide that the goal is met. When a round ends (the model stopped asking
//! for tools), the harness makes one separate, plain-text model call, the check, that reads the
//! objective and the recent transcript and answers with a JSON verdict. `continue` sends the
//! model back to work with the gaps the check found; `complete` ends the goal; `blocked` pauses
//! it. Nothing here needs tool calling or a provider feature, so it works on every wire format.
//!
//! A goal never ends as complete by accident: a check that fails, or answers in a form that does
//! not parse, pauses the goal instead.

use std::sync::Arc;

use patchbay_llm::{Message, Part, Request, Role, SystemBlock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::prompt;
use crate::reqlog::ReqTimer;
use crate::session::{Mode, Session, Todo, Totals};
use crate::store::Entry;
use crate::tools::clip_middle;

/// Most characters of the transcript the check reads.
const EVIDENCE_CHARS: usize = 32_000;
/// Most characters of one transcript item the check reads.
const ITEM_CHARS: usize = 4_000;
/// Times in a row the check may name the same gaps before the goal pauses.
const SAME_GAPS_LIMIT: u32 = 3;
/// Tries at the check before the goal pauses.
const CHECK_TRIES: usize = 2;
/// Model requests between two reminders of the objective (they keep it in view after a compaction).
const REMIND_EVERY: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    /// Stopped for a reason in `Goal::reason`; `/goal resume` goes on.
    Paused,
    /// The check found something only the user can fix.
    Blocked,
    Complete,
}

impl GoalStatus {
    fn label(self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::Blocked => "blocked",
            GoalStatus::Complete => "complete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Goal {
    pub objective: String,
    pub status: GoalStatus,
    /// Why the goal is paused or blocked, or the evidence that it is complete.
    #[serde(default)]
    pub reason: String,
    /// Checks made so far.
    #[serde(default)]
    pub rounds: u32,
    /// Checks made since the goal started or was last resumed (the round cap counts these).
    #[serde(default)]
    pub stretch_rounds: u32,
    /// Most rounds one start or resume may take.
    #[serde(default)]
    pub max_rounds: u32,
    /// Most tokens (input and output, checks included) one start or resume may use.
    #[serde(default)]
    pub token_budget: Option<u64>,
    #[serde(default)]
    pub tokens_used: u64,
    /// `tokens_used` when the goal started or was last resumed.
    #[serde(default)]
    pub tokens_base: u64,
    /// What the last check said was missing.
    #[serde(default)]
    pub last_gaps: String,
    /// Checks in a row that named the same gaps.
    #[serde(default)]
    pub same_gaps: u32,
    /// Model requests since the objective was last put in front of the model.
    #[serde(skip)]
    pub since_note: u32,
}

impl Goal {
    pub fn new(objective: String, token_budget: Option<u64>, max_rounds: usize) -> Goal {
        Goal {
            objective,
            status: GoalStatus::Active,
            reason: String::new(),
            rounds: 0,
            stretch_rounds: 0,
            max_rounds: max_rounds.min(u32::MAX as usize) as u32,
            token_budget,
            tokens_used: 0,
            tokens_base: 0,
            last_gaps: String::new(),
            same_gaps: 0,
            since_note: 0,
        }
    }

    fn pause(&mut self, reason: impl Into<String>) {
        self.status = GoalStatus::Paused;
        self.reason = reason.into();
    }

    /// The goal as the client sees it.
    pub fn view(&self) -> Value {
        json!({
            "objective": self.objective,
            "status": self.status,
            "reason": self.reason,
            "rounds": self.rounds,
            "stretchRounds": self.stretch_rounds,
            "maxRounds": self.max_rounds,
            "tokensUsed": self.tokens_used,
            "budget": self.token_budget,
            "lastGaps": self.last_gaps,
        })
    }

    /// A paused or blocked goal starts a new stretch: a fresh round count and token allowance.
    fn begin_stretch(&mut self, max_rounds: usize) {
        self.status = GoalStatus::Active;
        self.reason.clear();
        self.stretch_rounds = 0;
        self.max_rounds = max_rounds.min(u32::MAX as usize) as u32;
        self.tokens_base = self.tokens_used;
        self.same_gaps = 0;
        self.since_note = 0;
    }
}

/// What the user typed after `/goal`.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Status,
    Pause,
    Resume,
    Clear,
    Set { objective: String, budget: Option<u64> },
}

/// `/goal ...` as the whole prompt.
pub fn parse(parts: &[Part]) -> Option<Command> {
    let [Part::Text(t)] = parts else { return None };
    let rest = t.trim().strip_prefix("/goal")?;
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    let rest = rest.trim();
    Some(match rest.to_ascii_lowercase().as_str() {
        "" | "status" => Command::Status,
        "pause" => Command::Pause,
        "resume" => Command::Resume,
        "clear" => Command::Clear,
        _ => {
            let (objective, budget) = split_budget(rest);
            Command::Set { objective, budget }
        }
    })
}

/// A trailing `--budget 500k` taken off the objective.
fn split_budget(text: &str) -> (String, Option<u64>) {
    let words: Vec<&str> = text.split_whitespace().collect();
    if let [.., flag, value] = words.as_slice()
        && *flag == "--budget"
        && let Some(n) = parse_tokens(value)
    {
        // Cut the flag off the text itself, so the objective keeps its own spacing and lines.
        let at = text.rfind("--budget").unwrap_or(text.len());
        return (text[..at].trim().to_owned(), Some(n));
    }
    (text.to_owned(), None)
}

/// `500000`, `500k` or `2m`.
fn parse_tokens(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (digits, mult) = match s.strip_suffix('k') {
        Some(d) => (d, 1_000),
        None => match s.strip_suffix('m') {
            Some(d) => (d, 1_000_000),
            None => (s.as_str(), 1),
        },
    };
    digits.parse::<u64>().ok().filter(|n| *n > 0).map(|n| n.saturating_mul(mult))
}

/// What a `/goal` command does to the turn.
pub enum Outcome {
    /// Answer with this text and end the turn.
    Reply(String),
    /// Work on the goal: the turn goes on with this note beside what the user typed.
    Start(String),
}

fn spent(t: &Totals) -> u64 {
    t.input + t.output
}

/// Send the goal to the client.
pub fn announce(session: &Session) {
    let goal = session.goal.lock().unwrap().clone();
    session.update(json!({"sessionUpdate": "goal_update", "goal": goal.as_ref().map(Goal::view)}));
}

/// Write the goal to the session file and send it to the client.
fn save(session: &Session) {
    let goal = session.goal.lock().unwrap().clone();
    session.store.append(&Entry::Goal { goal });
    announce(session);
}

fn commit(session: &Session, goal: &Goal) {
    *session.goal.lock().unwrap() = Some(goal.clone());
    save(session);
}

fn status_text(goal: &Goal) -> String {
    let mut s = format!("Goal: {}\nStatus: {}", goal.objective, goal.status.label());
    if !goal.reason.is_empty() {
        s.push_str(&format!(" ({})", goal.reason));
    }
    s.push_str(&format!("\nRounds: {} of {} this stretch, {} in all", goal.stretch_rounds, goal.max_rounds, goal.rounds));
    match goal.token_budget {
        Some(b) => s.push_str(&format!("\nTokens: {} of {b} this stretch", goal.tokens_used - goal.tokens_base)),
        None => s.push_str(&format!("\nTokens: {}", goal.tokens_used)),
    }
    if !goal.last_gaps.is_empty() && goal.status != GoalStatus::Complete {
        s.push_str(&format!("\nLast check found: {}", goal.last_gaps));
    }
    s
}

/// Run a `/goal` command.
pub fn command(session: &Session, cmd: Command, mode: Mode) -> Outcome {
    let max_rounds = session.host.settings().limits.goal_max_rounds;
    let p = session.prompts();
    // The outcome, and whether the goal changed (so it is written and sent).
    let (outcome, changed) = {
        let mut slot = session.goal.lock().unwrap();
        let reply = |s: &str| (Outcome::Reply(s.to_owned()), false);
        match cmd {
            Command::Status => match slot.as_ref() {
                Some(g) => (Outcome::Reply(status_text(g)), false),
                None => reply("No goal is set. /goal <objective> sets one; add --budget 500k to cap the tokens."),
            },
            Command::Pause => match slot.as_mut() {
                Some(g) if g.status == GoalStatus::Active => {
                    g.pause("paused by the user");
                    (Outcome::Reply("Goal paused. /goal resume goes on.".into()), true)
                }
                Some(g) => (Outcome::Reply(format!("The goal is {}, not running.", g.status.label())), false),
                None => reply("No goal is set."),
            },
            Command::Clear => match slot.take() {
                Some(_) => (Outcome::Reply("Goal cleared.".into()), true),
                None => reply("No goal is set."),
            },
            Command::Resume => match slot.as_mut() {
                Some(g) if matches!(g.status, GoalStatus::Paused | GoalStatus::Blocked) => {
                    g.begin_stretch(max_rounds);
                    let gaps = if g.last_gaps.is_empty() { "nothing yet: check the state of the work first" } else { g.last_gaps.as_str() };
                    let note = p.msg("goal_continue", &[("objective", &g.objective), ("gaps", gaps), ("next", "pick up where the work stopped")]);
                    (Outcome::Start(note), true)
                }
                Some(g) => (Outcome::Reply(format!("The goal is {}: nothing to resume.", g.status.label())), false),
                None => reply("No goal is set."),
            },
            Command::Set { objective, budget } => {
                if mode == Mode::Plan {
                    reply("Leave plan mode first: a goal runs the agent on its own, and plan mode is read-only.")
                } else {
                    let note = p.msg("goal_start", &[("objective", &objective)]);
                    *slot = Some(Goal::new(objective, budget, max_rounds));
                    (Outcome::Start(note), true)
                }
            }
        }
    };
    if changed {
        save(session);
    }
    outcome
}

/// A turn that ends any way but through the check leaves an active goal paused, so nothing
/// stays "active" with no turn running.
pub fn interrupted(session: &Session, stop: &str) {
    let reason = match stop {
        "cancelled" => "stopped by the user",
        "max_turn_requests" => "a round hit the step limit",
        "max_tokens" => "a reply hit the output token limit",
        "refusal" => "the model refused",
        "error" => "a model call failed",
        _ => "the turn ended before the check ran",
    };
    let changed = {
        let mut slot = session.goal.lock().unwrap();
        match slot.as_mut() {
            Some(g) if g.status == GoalStatus::Active => {
                g.pause(reason);
                true
            }
            _ => false,
        }
    };
    if changed {
        save(session);
        session.text_chunk("agent_thought_chunk", &format!("Goal paused: {reason}. /goal resume goes on.\n"));
    }
}

/// A reminder of the objective, now and then, so a compaction does not lose it.
pub fn reminder(session: &Session) -> Option<String> {
    let mut slot = session.goal.lock().unwrap();
    let g = slot.as_mut().filter(|g| g.status == GoalStatus::Active)?;
    g.since_note += 1;
    if g.since_note < REMIND_EVERY {
        return None;
    }
    g.since_note = 0;
    Some(session.prompts().msg("goal_reminder", &[("objective", &g.objective), ("round", &(g.rounds + 1).to_string())]))
}

/// What a round's end leads to.
pub enum RoundEnd {
    /// No goal is running, or it stopped: the turn ends.
    Done,
    /// Not done: send the model back to work with this note.
    Continue(String),
    /// The user cancelled during the check.
    Cancelled,
}

/// The check, at the end of a round. `seen` is how much of `totals` earlier checks counted.
pub async fn round_end(session: &Arc<Session>, mode: Mode, totals: &Totals, seen: &mut u64, cancel: &CancellationToken) -> RoundEnd {
    let spent_now = spent(totals);
    let mut goal = {
        let mut slot = session.goal.lock().unwrap();
        match slot.as_mut() {
            Some(g) if g.status == GoalStatus::Active => {
                g.tokens_used += spent_now.saturating_sub(*seen);
                g.clone()
            }
            _ => return RoundEnd::Done,
        }
    };
    *seen = spent_now;
    goal.rounds += 1;
    goal.stretch_rounds += 1;
    if mode == Mode::Plan {
        goal.pause("the session is in plan mode");
        commit(session, &goal);
        session.text_chunk("agent_thought_chunk", "Goal paused: the session is in plan mode. /goal resume goes on.\n");
        return RoundEnd::Done;
    }
    let model_id = session.state.lock().await.model_id.clone();
    let mut verdict = None;
    let mut failure = String::new();
    for _ in 0..CHECK_TRIES {
        match check(session, &model_id, &mut goal, cancel).await {
            Ok(v) => {
                verdict = Some(v);
                break;
            }
            Err(e) if e == "cancelled" => {
                goal.pause("stopped by the user");
                commit(session, &goal);
                return RoundEnd::Cancelled;
            }
            Err(e) => failure = e,
        }
    }
    let Some(verdict) = verdict else {
        let reason = format!("the completion check failed: {failure}");
        goal.pause(reason.clone());
        commit(session, &goal);
        session.text_chunk("agent_message_chunk", &format!("\n\nGoal paused: {reason}. /goal resume tries again."));
        return RoundEnd::Done;
    };
    let next = apply_verdict(&mut goal, verdict);
    commit(session, &goal);
    match next {
        Next::Finished => {
            let line = match goal.status {
                GoalStatus::Complete => format!("Goal complete after {} round(s): {}", goal.rounds, goal.reason),
                _ => format!("Goal {}: {}. /goal resume goes on once that is fixed.", goal.status.label(), goal.reason),
            };
            session.text_chunk("agent_message_chunk", &format!("\n\n{line}"));
            RoundEnd::Done
        }
        Next::Continue(directive) => {
            session.text_chunk(
                "agent_thought_chunk",
                &format!("\nGoal check, round {}: not complete yet: {}\n", goal.rounds, goal.last_gaps),
            );
            RoundEnd::Continue(session.prompts().msg(
                "goal_continue",
                &[("objective", &goal.objective), ("gaps", &goal.last_gaps), ("next", &directive)],
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Continue,
    Complete,
    Blocked,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub decision: Decision,
    pub evidence: String,
    pub gaps: String,
    pub next_step: String,
}

#[derive(Debug, PartialEq)]
pub enum Next {
    /// The goal is complete, blocked or paused.
    Finished,
    /// Go on, with this suggested next step.
    Continue(String),
}

/// Fold a verdict into the goal: its status, and what the next round is told.
pub fn apply_verdict(goal: &mut Goal, v: Verdict) -> Next {
    match v.decision {
        Decision::Complete => {
            goal.status = GoalStatus::Complete;
            goal.reason = v.evidence;
            goal.last_gaps.clear();
            return Next::Finished;
        }
        Decision::Blocked => {
            goal.status = GoalStatus::Blocked;
            goal.reason = if v.gaps.is_empty() { v.evidence } else { v.gaps.clone() };
            goal.last_gaps = v.gaps;
            return Next::Finished;
        }
        Decision::Continue => {}
    }
    let gaps = if v.gaps.is_empty() { "the check gave no details; compare the work with the objective again".to_owned() } else { v.gaps };
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    goal.same_gaps = if norm(&gaps) == norm(&goal.last_gaps) { goal.same_gaps + 1 } else { 1 };
    goal.last_gaps = gaps;
    if goal.same_gaps >= SAME_GAPS_LIMIT {
        goal.pause(format!("the check named the same gaps {SAME_GAPS_LIMIT} times in a row"));
        return Next::Finished;
    }
    if goal.max_rounds > 0 && goal.stretch_rounds >= goal.max_rounds {
        goal.pause(format!("reached the round cap ({})", goal.max_rounds));
        return Next::Finished;
    }
    if let Some(budget) = goal.token_budget
        && goal.tokens_used.saturating_sub(goal.tokens_base) >= budget
    {
        goal.pause(format!("reached the token budget ({budget})"));
        return Next::Finished;
    }
    let next = if v.next_step.is_empty() { "close those gaps".to_owned() } else { v.next_step };
    Next::Continue(next)
}

/// One call to the check. Its tokens count toward the goal and the turn.
async fn check(session: &Arc<Session>, model_id: &str, goal: &mut Goal, cancel: &CancellationToken) -> Result<Verdict, String> {
    let mc = session.model_client(model_id).await?;
    let (messages, todos) = {
        let st = session.state.lock().await;
        (st.messages.clone(), st.todos.clone())
    };
    let p = session.prompts();
    let mut system = mc.system_prefix.clone();
    system.push(SystemBlock::new(p.msg("goal_check_system", &[])));
    let request = Request { system, messages: vec![Message::user(evidence(&goal.objective, &messages, &todos))], ..Default::default() };
    let mut timer = ReqTimer::start();
    let result = mc.client.complete(&request, cancel, |ev| timer.event(&ev)).await;
    session.store.append(&Entry::Req(timer.finish(&mc.client.config, Some("goal"), &result)));
    let response = result.map_err(|e| if e.cancelled { "cancelled".to_owned() } else { e.to_string() })?;
    if let Some(u) = response.usage {
        goal.tokens_used += u.input + u.output;
        let mut extra = session.extra_usage.lock().unwrap();
        extra.input += u.input;
        extra.cached_read += u.cached_read;
        extra.cache_write += u.cache_write;
        extra.output += u.output;
        extra.reasoning += u.reasoning;
        drop(extra);
        session.add_cost(u.cost_usd);
    }
    parse_verdict(&response.message.text())
}

/// What the check reads: the objective, the task list and the end of the transcript.
pub fn evidence(objective: &str, messages: &[Message], todos: &[Todo]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for m in messages {
        for part in &m.parts {
            let line = match (m.role, part) {
                (Role::User, Part::Text(t)) if !prompt::is_reminder(t) => format!("USER: {}", clip_middle(t, ITEM_CHARS)),
                (Role::Assistant, Part::Text(t)) => format!("ASSISTANT: {}", clip_middle(t, ITEM_CHARS)),
                (_, Part::ToolCall(c)) => format!("TOOL CALL {}: {}", c.name, clip_middle(&c.arguments.to_string(), ITEM_CHARS)),
                (_, Part::ToolResult(r)) => format!(
                    "TOOL RESULT {}{}: {}",
                    r.name,
                    if r.is_error { " (error)" } else { "" },
                    clip_middle(&r.joined_text(), ITEM_CHARS)
                ),
                _ => continue,
            };
            lines.push(line);
        }
    }
    // The end of the transcript is what counts: take lines from the back until the room is used.
    let mut used = 0;
    let mut start = lines.len();
    while start > 0 && used + lines[start - 1].len() <= EVIDENCE_CHARS {
        start -= 1;
        used += lines[start].len();
    }
    let mut out = format!("<goal>\n{}\n</goal>\n\n", objective.trim());
    if !todos.is_empty() {
        out.push_str("<task-list>\n");
        for t in todos {
            out.push_str(&format!("- [{}] {}\n", t.status, t.content));
        }
        out.push_str("</task-list>\n\n");
    }
    out.push_str("<transcript>\n");
    if start > 0 {
        out.push_str("[earlier transcript left out]\n");
    }
    out.push_str(&lines[start..].join("\n\n"));
    out.push_str("\n</transcript>\n\nGive your verdict as the JSON object.");
    out
}

/// The first JSON object in `text` that carries a `decision`. The reply may wrap it in prose or a
/// code fence. Text with no such object is an error: it never reads as `complete`.
pub fn parse_verdict(text: &str) -> Result<Verdict, String> {
    for (at, _) in text.match_indices('{') {
        let Some(Ok(Value::Object(o))) = serde_json::Deserializer::from_str(&text[at..]).into_iter::<Value>().next() else { continue };
        let Some(decision) = o.get("decision").and_then(Value::as_str) else { continue };
        let decision = match decision.trim().to_ascii_lowercase().as_str() {
            "continue" => Decision::Continue,
            "complete" => Decision::Complete,
            "blocked" => Decision::Blocked,
            other => return Err(format!("unknown decision {other:?}")),
        };
        let text_of = |key: &str| match o.get(key) {
            Some(Value::String(s)) => s.trim().to_owned(),
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("; "),
            _ => String::new(),
        };
        let v = Verdict { decision, evidence: text_of("evidence"), gaps: text_of("gaps"), next_step: text_of("next_step") };
        if v.decision == Decision::Complete && v.evidence.is_empty() {
            return Err("the check called the goal complete without evidence".into());
        }
        return Ok(v);
    }
    Err("the check did not answer with a verdict".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &str) -> Option<Command> {
        parse(&[Part::Text(s.into())])
    }

    fn verdict(decision: Decision, gaps: &str) -> Verdict {
        Verdict { decision, evidence: "ran the tests".into(), gaps: gaps.into(), next_step: String::new() }
    }

    #[test]
    fn parses_the_subcommands_and_the_objective() {
        assert_eq!(cmd("/goal"), Some(Command::Status));
        assert_eq!(cmd("/goal status"), Some(Command::Status));
        assert_eq!(cmd("/goal  Pause "), Some(Command::Pause));
        assert_eq!(cmd("/goal resume"), Some(Command::Resume));
        assert_eq!(cmd("/goal clear"), Some(Command::Clear));
        assert_eq!(cmd("/goal fix the build"), Some(Command::Set { objective: "fix the build".into(), budget: None }));
        // Only the exact word is a subcommand.
        assert_eq!(cmd("/goal pause the deploy"), Some(Command::Set { objective: "pause the deploy".into(), budget: None }));
        assert_eq!(cmd("/goals"), None);
        assert_eq!(cmd("goal x"), None);
    }

    #[test]
    fn a_budget_flag_counts_only_at_the_end() {
        let set = |o: &str, b| Some(Command::Set { objective: o.into(), budget: b });
        assert_eq!(cmd("/goal ship it --budget 500k"), set("ship it", Some(500_000)));
        assert_eq!(cmd("/goal ship it --budget 2m"), set("ship it", Some(2_000_000)));
        assert_eq!(cmd("/goal ship it --budget 1234"), set("ship it", Some(1234)));
        assert_eq!(cmd("/goal ship --budget 5k it"), set("ship --budget 5k it", None));
        assert_eq!(cmd("/goal ship it --budget lots"), set("ship it --budget lots", None));
        assert_eq!(cmd("/goal ship it --budget 0"), set("ship it --budget 0", None));
        // More than one part is not a command.
        assert_eq!(parse(&[Part::Text("/goal x".into()), Part::Text("y".into())]), None);
    }

    #[test]
    fn reads_a_verdict_out_of_prose_and_fences() {
        let v = parse_verdict("Here you go:\n```json\n{\"decision\": \"continue\", \"gaps\": [\"tests fail\", \"no docs\"], \"next_step\": \"fix tests\"}\n```").unwrap();
        assert_eq!(v.decision, Decision::Continue);
        assert_eq!(v.gaps, "tests fail; no docs");
        assert_eq!(v.next_step, "fix tests");
        let v = parse_verdict("{\"note\": 1} then {\"decision\": \"Complete\", \"evidence\": \"cargo test passed\"}").unwrap();
        assert_eq!(v.decision, Decision::Complete);
        assert_eq!(v.evidence, "cargo test passed");
        let v = parse_verdict("{\"decision\":\"blocked\",\"gaps\":\"needs an API key\"}").unwrap();
        assert_eq!(v.decision, Decision::Blocked);
    }

    #[test]
    fn a_reply_that_is_not_a_verdict_is_an_error_never_complete() {
        assert!(parse_verdict("The goal is complete, great work!").is_err());
        assert!(parse_verdict("").is_err());
        assert!(parse_verdict("{\"decision\": \"done\"}").is_err());
        assert!(parse_verdict("{\"decision\": \"complete\"}").is_err());
        assert!(parse_verdict("{\"decision\": \"complete\", \"evidence\": \"  \"}").is_err());
        assert!(parse_verdict("{\"decision\": \"continue\"").is_err());
    }

    #[test]
    fn complete_ends_the_goal_with_the_evidence() {
        let mut g = Goal::new("x".into(), None, 20);
        assert_eq!(apply_verdict(&mut g, verdict(Decision::Complete, "")), Next::Finished);
        assert_eq!(g.status, GoalStatus::Complete);
        assert_eq!(g.reason, "ran the tests");
    }

    #[test]
    fn blocked_stops_with_the_reason() {
        let mut g = Goal::new("x".into(), None, 20);
        assert_eq!(apply_verdict(&mut g, verdict(Decision::Blocked, "needs a key")), Next::Finished);
        assert_eq!(g.status, GoalStatus::Blocked);
        assert_eq!(g.reason, "needs a key");
    }

    #[test]
    fn continue_goes_on_until_a_limit_pauses_it() {
        let mut g = Goal::new("x".into(), None, 3);
        for (i, gap) in ["a", "b"].iter().enumerate() {
            g.stretch_rounds = i as u32 + 1;
            assert!(matches!(apply_verdict(&mut g, verdict(Decision::Continue, gap)), Next::Continue(_)));
            assert_eq!(g.status, GoalStatus::Active);
        }
        g.stretch_rounds = 3;
        assert_eq!(apply_verdict(&mut g, verdict(Decision::Continue, "c")), Next::Finished);
        assert_eq!(g.status, GoalStatus::Paused);
        assert!(g.reason.contains("round cap"), "{}", g.reason);
    }

    #[test]
    fn the_same_gaps_three_times_pause_the_goal() {
        let mut g = Goal::new("x".into(), None, 20);
        assert!(matches!(apply_verdict(&mut g, verdict(Decision::Continue, "Tests  fail")), Next::Continue(_)));
        assert!(matches!(apply_verdict(&mut g, verdict(Decision::Continue, "tests fail")), Next::Continue(_)));
        assert_eq!(apply_verdict(&mut g, verdict(Decision::Continue, "TESTS FAIL")), Next::Finished);
        assert_eq!(g.status, GoalStatus::Paused);
        assert!(g.reason.contains("same gaps"));
        // Different gaps in between reset the count.
        let mut g = Goal::new("x".into(), None, 20);
        for gap in ["a", "a", "b", "b", "a"] {
            assert!(matches!(apply_verdict(&mut g, verdict(Decision::Continue, gap)), Next::Continue(_)), "{gap}");
        }
    }

    #[test]
    fn the_token_budget_counts_from_the_start_of_the_stretch() {
        let mut g = Goal::new("x".into(), Some(1000), 20);
        g.tokens_base = 5000;
        g.tokens_used = 5900;
        assert!(matches!(apply_verdict(&mut g, verdict(Decision::Continue, "a")), Next::Continue(_)));
        g.tokens_used = 6000;
        assert_eq!(apply_verdict(&mut g, verdict(Decision::Continue, "b")), Next::Finished);
        assert_eq!(g.status, GoalStatus::Paused);
        assert!(g.reason.contains("token budget"));
        g.begin_stretch(20);
        assert_eq!(g.status, GoalStatus::Active);
        assert_eq!(g.tokens_base, 6000);
        assert!(matches!(apply_verdict(&mut g, verdict(Decision::Continue, "c")), Next::Continue(_)));
    }

    #[test]
    fn complete_wins_over_a_spent_budget() {
        let mut g = Goal::new("x".into(), Some(10), 1);
        g.tokens_used = 99;
        g.stretch_rounds = 1;
        assert_eq!(apply_verdict(&mut g, verdict(Decision::Complete, "")), Next::Finished);
        assert_eq!(g.status, GoalStatus::Complete);
    }

    #[test]
    fn the_evidence_keeps_the_end_of_the_transcript_and_trims_items() {
        use patchbay_llm::{ToolCall, ToolResult};
        let call = ToolCall { id: "1".into(), name: "Bash".into(), arguments: json!({"command": "cargo test"}), signature: None };
        let mut messages = vec![Message::user("old request ".repeat(100))];
        for i in 0..40 {
            messages.push(Message::assistant(format!("step {i}: {}", "x".repeat(2000))));
        }
        messages.push(Message::user(prompt::reminder("hidden hint")));
        messages.push(Message::with_parts(Role::Assistant, vec![Part::ToolCall(call.clone())]));
        messages.push(Message::with_parts(Role::User, vec![Part::ToolResult(ToolResult::text(&call, "y".repeat(10_000), true))]));
        let todos = vec![Todo { content: "write it".into(), status: "completed".into(), active_form: String::new() }];
        let e = evidence("make it pass", &messages, &todos);
        assert!(e.starts_with("<goal>\nmake it pass\n</goal>"));
        assert!(e.contains("- [completed] write it"));
        assert!(e.contains("[earlier transcript left out]"));
        assert!(!e.contains("old request"));
        assert!(!e.contains("hidden hint"));
        assert!(e.contains("TOOL CALL Bash"));
        assert!(e.contains("TOOL RESULT Bash (error)"));
        assert!(e.contains("step 39"));
        // Room for the transcript plus the frame, not the whole history.
        assert!(e.len() < EVIDENCE_CHARS + 2_000, "{}", e.len());
        // A short transcript is kept whole.
        let short = evidence("g", &messages[..2], &[]);
        assert!(!short.contains("left out"));
        assert!(short.contains("USER: old request"));
    }
}
