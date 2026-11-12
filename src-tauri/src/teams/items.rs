//! Teams phase 3: items, decisions, the team tools agents call, and the UI's board commands.
//!
//! Every tool call is checked on the host against the caller's role capabilities in `teams.db`
//! (`Caller::need`), never against what the role's instructions say.

use std::path::Path;

use anyhow::Context;
use serde_json::{Value, json};

use super::*;

pub const STATUSES: &[&str] = &["noticed", "triaged", "blocked", "dispatched", "in-progress", "awaiting-review", "done", "dropped"];

/// Refused when `item_set` moves an item to awaiting-review or done with no dispatch row.
pub const DISPATCH_REQUIRED_FOR_FINISH: &str =
    "Cannot move item to awaiting-review or done without a worker dispatch. Coordinators do not implement items directly.";

/// Refused when `item_set` marks done on a dirty worktree without a PR URL.
pub const DONE_REQUIRES_CLEAN_WORKTREE: &str =
    "Cannot mark item done: worktree has uncommitted changes. Worker must commit all changes.";

/// Refused when `item_set` marks done with no commits ahead of the team base and no PR URL.
pub const DONE_REQUIRES_COMMITS_AHEAD: &str =
    "Cannot mark item done: branch has no new commits ahead of base branch. Worker must commit changes.";

pub const MIGRATION: &str = r#"
CREATE TABLE items (
    team_id TEXT NOT NULL REFERENCES teams(id),
    id TEXT NOT NULL,
    title TEXT NOT NULL,
    type TEXT NOT NULL,
    severity TEXT NOT NULL,
    status TEXT NOT NULL,
    member_session TEXT,
    plan TEXT NOT NULL,
    pr_url TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (team_id, id)
);
CREATE TABLE item_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    team_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    actor_session TEXT,
    kind TEXT NOT NULL,
    text TEXT NOT NULL
);
CREATE INDEX item_events_item ON item_events(team_id, item_id);
-- The changelog is append-only.
CREATE TRIGGER item_events_no_update BEFORE UPDATE ON item_events BEGIN SELECT RAISE(ABORT, 'item_events is append-only'); END;
CREATE TRIGGER item_events_no_delete BEFORE DELETE ON item_events BEGIN SELECT RAISE(ABORT, 'item_events is append-only'); END;
-- Counters behind item and decision ids; a number is handed out once and never again.
CREATE TABLE id_seq (
    team_id TEXT NOT NULL,
    scope TEXT NOT NULL,
    n INTEGER NOT NULL,
    PRIMARY KEY (team_id, scope)
);
CREATE TABLE decisions (
    team_id TEXT NOT NULL REFERENCES teams(id),
    id TEXT NOT NULL,
    item_id TEXT,
    question TEXT NOT NULL,
    options_json TEXT NOT NULL,
    recommendation TEXT,
    class TEXT NOT NULL,
    opened_by TEXT,
    advisor_verdict TEXT,
    advisor_choice TEXT,
    advisor_text TEXT,
    user_answer TEXT,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    resolved_at INTEGER,
    PRIMARY KEY (team_id, id)
);
"#;

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub team_id: String,
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub severity: String,
    pub status: String,
    pub member_session: Option<String>,
    pub plan: String,
    pub pr_url: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ItemEvent {
    pub id: i64,
    pub item_id: String,
    pub at: i64,
    pub actor_session: Option<String>,
    pub kind: String,
    pub text: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub team_id: String,
    pub id: String,
    pub item_id: Option<String>,
    pub question: String,
    pub options: Vec<String>,
    pub recommendation: Option<String>,
    pub class: String,
    pub opened_by: Option<String>,
    pub advisor_verdict: Option<String>,
    pub advisor_choice: Option<String>,
    pub advisor_text: Option<String>,
    pub user_answer: Option<String>,
    /// awaiting_advisor, awaiting_user, cleared (the advisor decided) or resolved.
    pub status: String,
    /// The advisor chose something other than what the coordinator recommended.
    pub disagrees: bool,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
}

fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

fn today() -> String {
    chrono::Utc::now().format("%Y%m%d").to_string()
}

impl TeamsDb {
    /// The next number of `scope` for a team. Inside the caller's transaction.
    fn next_n(&self, team_id: &str, scope: &str) -> Result<i64> {
        Ok(self.0.query_row(
            "INSERT INTO id_seq(team_id, scope, n) VALUES (?1, ?2, 1) ON CONFLICT(team_id, scope) DO UPDATE SET n = n + 1 RETURNING n",
            params![team_id, scope],
            |r| r.get(0),
        )?)
    }

    fn event(&self, team_id: &str, item_id: &str, actor: Option<&str>, kind: &str, text: &str) -> Result<()> {
        self.0.execute(
            "INSERT INTO item_events(team_id, item_id, at, actor_session, kind, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![team_id, item_id, now_ms(), actor, kind, text],
        )?;
        Ok(())
    }

    /// `<prefix>-YYYYMMDD-NN` for `day`; NN restarts each day and is never reused.
    pub fn add_item(&self, team: &Team, actor: Option<&str>, day: &str, title: &str, kind: &str, severity: &str, plan: &str) -> Result<Item> {
        let title = title.trim();
        if title.is_empty() {
            bail!("an item needs a title");
        }
        let tx = self.0.unchecked_transaction()?;
        let id = format!("{}-{day}-{:02}", team.id_prefix, self.next_n(&team.id, &format!("items:{day}"))?);
        let now = now_ms();
        self.0.execute(
            "INSERT INTO items(team_id, id, title, type, severity, status, plan, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'noticed', ?6, ?7, ?7)",
            params![team.id, id, title, kind, severity, plan, now],
        )?;
        self.event(&team.id, &id, actor, "created", title)?;
        tx.commit()?;
        self.item(&team.id, &id)
    }

    fn item_from(r: &rusqlite::Row) -> rusqlite::Result<Item> {
        Ok(Item {
            team_id: r.get(0)?,
            id: r.get(1)?,
            title: r.get(2)?,
            kind: r.get(3)?,
            severity: r.get(4)?,
            status: r.get(5)?,
            member_session: r.get(6)?,
            plan: r.get(7)?,
            pr_url: r.get(8)?,
            created_at: r.get(9)?,
            updated_at: r.get(10)?,
        })
    }

    const ITEM_COLS: &'static str = "team_id, id, title, type, severity, status, member_session, plan, pr_url, created_at, updated_at";

    pub fn item(&self, team_id: &str, id: &str) -> Result<Item> {
        self.0
            .query_row(&format!("SELECT {} FROM items WHERE team_id = ?1 AND id = ?2", Self::ITEM_COLS), params![team_id, id], Self::item_from)
            .optional()?
            .ok_or_else(|| anyhow!("no item {id} on this team"))
    }

    pub fn items(&self, team_id: &str, status: Option<&str>) -> Result<Vec<Item>> {
        let mut st = self.0.prepare(&format!(
            "SELECT {} FROM items WHERE team_id = ?1 AND (?2 IS NULL OR status = ?2) ORDER BY created_at, id",
            Self::ITEM_COLS
        ))?;
        Ok(st.query_map(params![team_id, status], Self::item_from)?.collect::<rusqlite::Result<_>>()?)
    }

    /// True when `item_id` has at least one `dispatches` row (any worker was started for it).
    fn item_has_dispatch(&self, team_id: &str, item_id: &str) -> Result<bool> {
        Ok(self
            .0
            .query_row(
                "SELECT COUNT(*) FROM dispatches WHERE team_id = ?1 AND item_id = ?2",
                params![team_id, item_id],
                |r| r.get::<_, i64>(0),
            )?
            > 0)
    }

    /// Change named fields of an item; every change leaves an event. `patch` is a JSON object.
    pub fn set_item(&self, team_id: &str, id: &str, actor: Option<&str>, patch: &Value) -> Result<Item> {
        let fields = patch.as_object().filter(|o| !o.is_empty()).ok_or_else(|| anyhow!("name at least one field to change"))?;
        let before = self.item(team_id, id)?;
        // Resolve PR URL from the whole patch up front: field order in the object is not stable.
        let pr_url_after = match fields.get("prUrl") {
            Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.trim().to_owned(),
            Some(_) => bail!("prUrl must be text (or null to clear it)"),
            None => before.pr_url.clone().unwrap_or_default(),
        };
        let tx = self.0.unchecked_transaction()?;
        let mut changed = 0;
        for (key, value) in fields {
            let (column, old) = match key.as_str() {
                "title" => ("title", before.title.clone()),
                "type" => ("type", before.kind.clone()),
                "severity" => ("severity", before.severity.clone()),
                "status" => ("status", before.status.clone()),
                "assignee" => ("member_session", before.member_session.clone().unwrap_or_default()),
                "plan" => ("plan", before.plan.clone()),
                "prUrl" => ("pr_url", before.pr_url.clone().unwrap_or_default()),
                other => bail!("unknown field {other}; use title, type, severity, status, assignee, plan or prUrl"),
            };
            let new = match value {
                Value::Null => String::new(),
                Value::String(s) => s.trim().to_owned(),
                _ => bail!("{key} must be text (or null to clear it)"),
            };
            if key == "status" && !STATUSES.contains(&new.as_str()) {
                bail!("status must be one of {}", STATUSES.join(", "));
            }
            if key == "title" && new.is_empty() {
                bail!("an item needs a title");
            }
            if key == "assignee" && !new.is_empty() && !self.members(team_id)?.iter().any(|m| m.session_id == new) {
                bail!("{new} is not a member of this team");
            }
            if new == old {
                continue;
            }
            // Coordinators own status, but cannot skip the worker path to finish an item.
            if key == "status" && matches!(new.as_str(), "awaiting-review" | "done") && !self.item_has_dispatch(team_id, id)? {
                bail!("{DISPATCH_REQUIRED_FOR_FINISH}");
            }
            // Done needs durable evidence: a PR URL, or a clean worktree with commits ahead of base.
            if key == "status" && new == "done" && pr_url_after.is_empty() {
                ensure_done_is_durable(&self.team(team_id)?, id)?;
            }
            let stored: Option<&str> = if matches!(column, "member_session" | "pr_url") && new.is_empty() { None } else { Some(new.as_str()) };
            self.0.execute(
                &format!("UPDATE items SET {column} = ?3, updated_at = ?4 WHERE team_id = ?1 AND id = ?2"),
                params![team_id, id, stored, now_ms()],
            )?;
            self.event(team_id, id, actor, "set", &format!("{key}: {old:?} -> {new:?}"))?;
            changed += 1;
        }
        tx.commit()?;
        if changed == 0 {
            bail!("nothing changed: the item already has those values");
        }
        self.item(team_id, id)
    }

    pub fn note_item(&self, team_id: &str, id: &str, actor: Option<&str>, text: &str) -> Result<()> {
        self.item(team_id, id)?;
        if text.trim().is_empty() {
            bail!("a note needs text");
        }
        let tx = self.0.unchecked_transaction()?;
        self.event(team_id, id, actor, "note", text.trim())?;
        self.0.execute("UPDATE items SET updated_at = ?3 WHERE team_id = ?1 AND id = ?2", params![team_id, id, now_ms()])?;
        tx.commit()?;
        Ok(())
    }

    /// Events of one item, or the team's newest `limit` when `item` is None; oldest first.
    pub fn item_events(&self, team_id: &str, item: Option<&str>, limit: i64) -> Result<Vec<ItemEvent>> {
        let mut st = self.0.prepare(
            "SELECT id, item_id, at, actor_session, kind, text FROM (SELECT * FROM item_events WHERE team_id = ?1 AND (?2 IS NULL OR item_id = ?2) ORDER BY id DESC LIMIT ?3) ORDER BY id",
        )?;
        let rows = st.query_map(params![team_id, item, limit], |r| {
            Ok(ItemEvent { id: r.get(0)?, item_id: r.get(1)?, at: r.get(2)?, actor_session: r.get(3)?, kind: r.get(4)?, text: r.get(5)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn decision_from(r: &rusqlite::Row) -> rusqlite::Result<Decision> {
        let options: String = r.get(4)?;
        let recommendation: Option<String> = r.get(5)?;
        let advisor_choice: Option<String> = r.get(9)?;
        let disagrees = matches!((&recommendation, &advisor_choice), (Some(a), Some(b)) if !same(a, b));
        Ok(Decision {
            team_id: r.get(0)?,
            id: r.get(1)?,
            item_id: r.get(2)?,
            question: r.get(3)?,
            options: serde_json::from_str(&options).unwrap_or_default(),
            recommendation,
            class: r.get(6)?,
            opened_by: r.get(7)?,
            advisor_verdict: r.get(8)?,
            advisor_choice,
            advisor_text: r.get(10)?,
            user_answer: r.get(11)?,
            status: r.get(12)?,
            disagrees,
            created_at: r.get(13)?,
            resolved_at: r.get(14)?,
        })
    }

    const DECISION_COLS: &'static str = "team_id, id, item_id, question, options_json, recommendation, class, opened_by, advisor_verdict, advisor_choice, advisor_text, user_answer, status, created_at, resolved_at";

    pub fn decision(&self, team_id: &str, id: &str) -> Result<Decision> {
        self.0
            .query_row(&format!("SELECT {} FROM decisions WHERE team_id = ?1 AND id = ?2", Self::DECISION_COLS), params![team_id, id], Self::decision_from)
            .optional()?
            .ok_or_else(|| anyhow!("no decision {id} on this team"))
    }

    pub fn decisions(&self, team_id: &str) -> Result<Vec<Decision>> {
        let mut st = self.0.prepare(&format!("SELECT {} FROM decisions WHERE team_id = ?1 ORDER BY created_at, id", Self::DECISION_COLS))?;
        Ok(st.query_map([team_id], Self::decision_from)?.collect::<rusqlite::Result<_>>()?)
    }

    /// Open a decision. Routing by class: `human` waits for the user, `advisor_first` for the
    /// advisor, `coordinator` needs no gate and is recorded as resolved.
    #[cfg(test)]
    pub fn open_decision(&self, team: &Team, actor: Option<&str>, item: Option<&str>, question: &str, options: &[String], recommendation: Option<&str>, class: &str) -> Result<Decision> {
        self.open_decision_routed(team, actor, item, question, options, recommendation, class, true)
    }

    /// `advisor_available` is determined from live team membership before the decision is
    /// inserted, so an advisor-first decision with no eligible decider waits for the user.
    fn open_decision_routed(&self, team: &Team, actor: Option<&str>, item: Option<&str>, question: &str, options: &[String], recommendation: Option<&str>, class: &str, advisor_available: bool) -> Result<Decision> {
        if question.trim().is_empty() {
            bail!("a decision needs a question");
        }
        if let Some(i) = item {
            self.item(&team.id, i)?;
        }
        let known: Vec<String> = self.template(&team.template_id)?.routing.into_iter().map(|r| r.class).collect();
        if !known.iter().any(|k| k == class) {
            bail!("class must be one of {}", known.join(", "));
        }
        let status = match class {
            "human" => "awaiting_user",
            "advisor_first" if advisor_available => "awaiting_advisor",
            "advisor_first" => "awaiting_user",
            _ => "resolved",
        };
        let tx = self.0.unchecked_transaction()?;
        let id = format!("{}-D{}", team.id_prefix, self.next_n(&team.id, "decisions")?);
        let now = now_ms();
        self.0.execute(
            "INSERT INTO decisions(team_id, id, item_id, question, options_json, recommendation, class, opened_by, status, created_at, resolved_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![team.id, id, item, question.trim(), serde_json::to_string(options)?, recommendation, class, actor, status, now, (status == "resolved").then_some(now)],
        )?;
        if let Some(i) = item {
            self.event(&team.id, i, actor, "decision", &format!("{id} opened ({class}): {}", question.trim()))?;
        }
        tx.commit()?;
        self.decision(&team.id, &id)
    }

    /// The advisor's answer: CLEAR (it decided, naming a choice) or HUMAN (the user must).
    pub fn advise(&self, team_id: &str, id: &str, actor: Option<&str>, verdict: &str, choice: Option<&str>, text: Option<&str>) -> Result<Decision> {
        let d = self.decision(team_id, id)?;
        if d.status != "awaiting_advisor" {
            bail!("decision {id} is not waiting for the advisor (it is {})", d.status);
        }
        let verdict = verdict.trim().to_ascii_uppercase();
        let choice = choice.map(str::trim).filter(|c| !c.is_empty());
        let status = match verdict.as_str() {
            "CLEAR" => {
                let c = choice.ok_or_else(|| anyhow!("CLEAR needs a choice: the option you decide on"))?;
                if !d.options.is_empty() && !d.options.iter().any(|o| same(o, c)) {
                    bail!("choice must be one of the options: {}", d.options.join(" | "));
                }
                "cleared"
            }
            "HUMAN" => "awaiting_user",
            _ => bail!("verdict must be CLEAR or HUMAN"),
        };
        let tx = self.0.unchecked_transaction()?;
        self.0.execute(
            "UPDATE decisions SET advisor_verdict = ?3, advisor_choice = ?4, advisor_text = ?5, status = ?6, resolved_at = ?7 WHERE team_id = ?1 AND id = ?2",
            params![team_id, id, verdict, choice, text, status, (status == "cleared").then(now_ms)],
        )?;
        if let Some(i) = &d.item_id {
            self.event(team_id, i, actor, "decision", &format!("{id}: advisor {verdict}{}", choice.map(|c| format!(" ({c})")).unwrap_or_default()))?;
        }
        tx.commit()?;
        self.decision(team_id, id)
    }

    /// The user's answer, from the UI. Allowed while the decision is open.
    pub fn answer_user(&self, team_id: &str, id: &str, answer: &str) -> Result<Decision> {
        let d = self.decision(team_id, id)?;
        if matches!(d.status.as_str(), "resolved" | "cleared") {
            bail!("decision {id} is already closed");
        }
        if answer.trim().is_empty() {
            bail!("an answer can't be empty");
        }
        let tx = self.0.unchecked_transaction()?;
        self.0.execute(
            "UPDATE decisions SET user_answer = ?3, status = 'resolved', resolved_at = ?4 WHERE team_id = ?1 AND id = ?2",
            params![team_id, id, answer.trim(), now_ms()],
        )?;
        if let Some(i) = &d.item_id {
            self.event(team_id, i, None, "decision", &format!("{id}: user answered {}", answer.trim()))?;
        }
        tx.commit()?;
        self.decision(team_id, id)
    }
}

// ---------------- the caller, checked against its role ----------------

pub(super) struct Caller {
    pub(super) team: Team,
    pub(super) me: Member,
    pub(super) role: Role,
}

impl Caller {
    pub(super) fn of(tdb: &TeamsDb, session: &str) -> Result<Self> {
        let (team, me) = tdb.membership(session)?.ok_or_else(|| anyhow!("this session is not on a live team"))?;
        let role = tdb.role(&me.role_id)?;
        Ok(Self { team, me, role })
    }

    /// Refuse unless the role holds capability `cap` (named as in the role editor).
    pub(super) fn need(&self, cap: &str, allowed: bool, to: &str) -> Result<()> {
        if !allowed {
            bail!("refused: the {} role has no `{cap}` capability, which is needed to {to}", self.role.name);
        }
        Ok(())
    }
}

/// Host-side git for durable-done checks. Mirrors dispatch's safe flags (no hooks / system config).
fn git_in(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", "-C"])
        .arg(cwd)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.first().copied().unwrap_or(""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// When marking done without a PR URL, the item's worktree must be clean and ahead of `pr_base`.
/// Missing worktree folders are skipped (no local evidence to inspect).
fn ensure_done_is_durable(team: &Team, item_id: &str) -> Result<()> {
    let path = team.effective_worktree_root()?.join(item_id.to_lowercase());
    if !path.is_dir() {
        return Ok(());
    }
    let status = git_in(&path, &["status", "--porcelain"])?;
    if !status.is_empty() {
        bail!("{DONE_REQUIRES_CLEAN_WORKTREE}");
    }
    let ahead = git_in(&path, &["rev-list", "--count", &format!("{}..HEAD", team.pr_base)])?;
    let n: u64 = ahead.parse().unwrap_or(0);
    if n == 0 {
        bail!("{DONE_REQUIRES_COMMITS_AHEAD}");
    }
    Ok(())
}

/// True when `session` fills a seat on a live team (it then gets the team tools).
pub fn is_member(state: &AppState, session: &str) -> bool {
    TeamsDb::open(&state.data_dir).and_then(|t| t.membership(session)).map(|m| m.is_some()).unwrap_or(false)
}

/// Guidance when an active team member tries to spawn an ad-hoc session.
pub const TEAM_MEMBER_CREATE_REFUSAL: &str = "Team members must dispatch workers through dispatch_item, not ad-hoc sessions. If work is blocked, raise the blocker with the user or advisor.";

/// Refuse `create_agent_session` when `session` is an active team member. Non-members and
/// environments without a teams database are unaffected.
pub fn refuse_create_if_team_member(data_dir: &Path, session: &str) -> anyhow::Result<()> {
    // No teams.db means no membership to enforce; don't create one just to look.
    if !data_dir.join("teams.db").is_file() {
        return Ok(());
    }
    if TeamsDb::open(data_dir)?.membership(session)?.is_some() {
        anyhow::bail!("{TEAM_MEMBER_CREATE_REFUSAL}");
    }
    Ok(())
}

pub const TOOLS: &[&str] = &[
    "team_roster", "team_send", "item_add", "item_set", "item_note", "item_list", "item_get", "decision_open", "decision_answer",
    "dispatch_item", "member_release",
];

pub fn is_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

/// MCP definitions of the team tools, listed only for team members.
pub fn tool_defs() -> Vec<Value> {
    let tool = |name: &str, description: &str, props: Value, required: &[&str]| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": { "type": "object", "properties": props, "required": required, "additionalProperties": false }
        })
    };
    let text = |d: &str| json!({ "type": "string", "description": d });
    vec![
        tool("team_roster", "List the members of your team and open seats: role, seat, session id, agent, model, what each is doing now, and the item it holds.", json!({}), &[]),
        tool("team_send", "Message a teammate by role name (e.g. \"Advisor\") or session id. Same delivery rules as send_agent_message.", json!({ "to": text("A role name or a session id from team_roster."), "message": text("The message.") }), &["to", "message"]),
        tool("item_add", "Add an item to the team's board (status noticed). Needs the writes_items capability.", json!({ "title": text("What the item is."), "type": text("bug, feature, polish, chore... Default task."), "severity": text("Default normal."), "plan": text("Optional plan or notes.") }), &["title"]),
        tool("item_set", "Change named fields of an item: title, type, severity, status (noticed, triaged, blocked, dispatched, in-progress, awaiting-review, done, dropped), assignee (a member's session id, or null), plan, prUrl. Needs the writes_items capability.", json!({ "itemId": text("The item id."), "title": text("New title."), "type": text("New type."), "severity": text("New severity."), "status": text("New status."), "assignee": text("A member's session id; empty to clear."), "plan": text("New plan text."), "prUrl": text("Pull request URL.") }), &["itemId"]),
        tool("item_note", "Append a note to an item's history. Needs the writes_items capability, or the item must be assigned to you (a dispatched worker notes its own item only).", json!({ "itemId": text("The item id."), "text": text("The note.") }), &["itemId", "text"]),
        tool("item_list", "List the team's items, optionally only those in one status.", json!({ "status": text("Optional status filter.") }), &[]),
        tool("item_get", "Read one item with its history and decisions.", json!({ "itemId": text("The item id.") }), &["itemId"]),
        tool("decision_open", "Open a decision. Class advisor_first goes to the advisor (CLEAR or HUMAN), human goes to the user, coordinator needs no gate. Needs the writes_items capability. State your recommendation.", json!({ "question": text("The question."), "options": { "type": "array", "items": { "type": "string" }, "description": "The choices." }, "recommendation": text("Which option you recommend, and why."), "class": text("human, advisor_first (default) or coordinator."), "itemId": text("Optional item the decision is about.") }), &["question"]),
        tool("decision_answer", "Answer an advisor_first decision as the advisor: verdict CLEAR (you decide; name the choice) or HUMAN (the user must decide). Needs the decides capability. The user answers from Patchbay, not here.", json!({ "decisionId": text("The decision id."), "verdict": text("CLEAR or HUMAN."), "choice": text("For CLEAR: the option you decide on."), "text": text("Your reason, short.") }), &["decisionId", "verdict"]),
        tool("dispatch_item", "Start a worker on an item: makes a git worktree from the team's base branch, starts an agent from the worker seat, sends it the brief, and marks the item dispatched. Needs the dispatches capability. Refused when the seat's pool is full.", json!({ "itemId": text("A noticed, triaged or blocked item."), "seat": text("The worker seat; optional when the template has one."), "agent": text("One of the seat's agents; default the seat's own."), "model": text("The model of that agent's entry, when it has several.") }), &["itemId"]),
        tool("member_release", "Release a dispatched worker once its item is done, dropped or awaiting-review: stops its agent and frees its place. Needs the dispatches capability.", json!({ "sessionId": text("The worker's session id.") }), &["sessionId"]),
    ]
}

/// What a tool call did, and the messages it still has to send once the database is closed
/// (a SQLite connection can't be held across an await).
struct Outcome {
    value: Value,
    /// (from session, to session, text)
    tells: Vec<(String, String, String)>,
    /// A team_send: (from, to, message).
    send: Option<(String, String, String)>,
}

/// Run a team tool for `session`. The role check happens here, on the host.
pub async fn call_tool(state: &AppState, session: &str, name: &str, args: &Value) -> Result<Value> {
    match name {
        "dispatch_item" => return super::dispatch::dispatch_item(state, session, args).await,
        "member_release" => return super::dispatch::member_release(state, session, args).await,
        _ => {}
    }
    let out = run_tool(state, session, name, args)?;
    for (from, to, text) in out.tells {
        tell(state, Some(&from), &to, &super::dispatch::strip_forged_tags(&text)).await;
    }
    if let Some((from, to, message)) = out.send {
        crate::agent_messaging::check_send_limits(&from, &to)?;
        let node = state.mesh.node_id().to_owned();
        return crate::send_agent_message(state, &node, &from, &to, &super::dispatch::strip_forged_tags(&message)).await;
    }
    Ok(out.value)
}

fn run_tool(state: &AppState, session: &str, name: &str, args: &Value) -> Result<Outcome> {
    let tdb = TeamsDb::open(&state.data_dir)?;
    let mut tells = Vec::new();
    let c = Caller::of(&tdb, session)?;
    let s = |k: &str| args[k].as_str().map(str::trim).filter(|v| !v.is_empty());
    let need_s = |k: &str| s(k).ok_or_else(|| anyhow!("{k} is required"));
    let result = match name {
        "team_roster" => roster(state, &tdb, &c)?,
        "team_send" => {
            let target = resolve_target(&tdb, &c, need_s("to")?)?;
            return Ok(Outcome { value: Value::Null, tells, send: Some((session.to_owned(), target, need_s("message")?.to_owned())) });
        }
        "item_list" => {
            let mut v = json!(tdb.items(&c.team.id, s("status"))?);
            v.as_array_mut().into_iter().flatten().for_each(fence_item);
            v
        }
        "item_get" => {
            let item = tdb.item(&c.team.id, need_s("itemId")?)?;
            let events = tdb.item_events(&c.team.id, Some(&item.id), 200)?;
            let decisions: Vec<_> = tdb.decisions(&c.team.id)?.into_iter().filter(|d| d.item_id.as_deref() == Some(&item.id)).collect();
            let mut v = json!({ "item": item, "events": events, "decisions": decisions });
            fence_item(&mut v["item"]);
            for e in v["events"].as_array_mut().into_iter().flatten() {
                fence_field(e, "text");
            }
            for d in v["decisions"].as_array_mut().into_iter().flatten() {
                for k in ["question", "recommendation", "advisorText", "userAnswer"] {
                    fence_field(d, k);
                }
            }
            v
        }
        "item_add" => {
            c.need("writes_items", c.role.caps.writes_items, "create items")?;
            let item = tdb.add_item(&c.team, Some(session), &today(), need_s("title")?, s("type").unwrap_or("task"), s("severity").unwrap_or("normal"), s("plan").unwrap_or(""))?;
            changed(state, &tdb, &c.team.id, &format!("{} added by {}: {}", item.id, c.role.name, item.title));
            json!(item)
        }
        "item_set" => {
            c.need("writes_items", c.role.caps.writes_items, "change items")?;
            let id = need_s("itemId")?;
            let mut patch = args.clone();
            patch.as_object_mut().map(|o| o.remove("itemId"));
            let item = tdb.set_item(&c.team.id, id, Some(session), &patch)?;
            changed(state, &tdb, &c.team.id, &format!("{id} changed by {}: {}", c.role.name, patch));
            json!(item)
        }
        "item_note" => {
            let id = need_s("itemId")?;
            // The authority is the item's assignee (`items.member_session`), set by dispatch or the coordinator.
            if !c.role.caps.writes_items && tdb.item(&c.team.id, id)?.member_session.as_deref() != Some(session) {
                bail!("refused: the {} role may note only the item assigned to it", c.role.name);
            }
            tdb.note_item(&c.team.id, id, Some(session), need_s("text")?)?;
            changed(state, &tdb, &c.team.id, &format!("{id} noted by {}", c.role.name));
            json!({ "ok": true })
        }
        "decision_open" => {
            c.need("writes_items", c.role.caps.writes_items, "open decisions")?;
            let options: Vec<String> = args["options"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
            let class = s("class").unwrap_or("advisor_first");
            let advisors = if class == "advisor_first" { deciders(&tdb, &c.team.id)? } else { Vec::new() };
            let d = tdb.open_decision_routed(&c.team, Some(session), s("itemId"), need_s("question")?, &options, s("recommendation"), class, class != "advisor_first" || !advisors.is_empty())?;
            changed(state, &tdb, &c.team.id, &format!("{} opened ({}): {}", d.id, d.class, d.question));
            let mut warning = None;
            if class == "advisor_first" && advisors.is_empty() {
                warning = Some("No team member has the `decides` capability, so this advisor-first decision is waiting for the user to answer in Patchbay.");
            }
            if d.status == "awaiting_advisor" {
                let text = format!(
                    "Decision {} needs your verdict.\nQuestion: {}\nOptions: {}\nCoordinator recommends: {}\n\nAnswer with the decision_answer tool: verdict CLEAR (name your choice) or HUMAN (the user must decide), and a short reason.",
                    d.id, d.question, if d.options.is_empty() { "(open)".into() } else { d.options.join(" | ") }, d.recommendation.as_deref().unwrap_or("(none given)")
                );
                for a in advisors {
                    tells.push((session.to_owned(), a, text.clone()));
                }
            }
            return Ok(Outcome { value: json!({ "decision": d, "warning": warning }), tells, send: None });
        }
        "decision_answer" => {
            c.need("decides", c.role.caps.decides, "answer decisions")?;
            let d = tdb.advise(&c.team.id, need_s("decisionId")?, Some(session), need_s("verdict")?, s("choice"), s("text"))?;
            changed(state, &tdb, &c.team.id, &format!("{}: advisor {}", d.id, d.advisor_verdict.as_deref().unwrap_or("")));
            if let Some(opener) = &d.opened_by {
                let text = format!(
                    "Decision {}: the advisor says {}{}{}.{}",
                    d.id, d.advisor_verdict.as_deref().unwrap_or(""),
                    d.advisor_choice.as_deref().map(|c| format!(", choice \"{c}\"")).unwrap_or_default(),
                    d.advisor_text.as_deref().map(|t| format!(" ({t})")).unwrap_or_default(),
                    if d.status == "awaiting_user" { " It is now waiting for the user." } else { "" }
                );
                tells.push((session.to_owned(), opener.clone(), text));
            }
            json!(d)
        }
        _ => bail!("unknown team tool {name}"),
    };
    Ok(Outcome { value: result, tells, send: None })
}

/// Free text in tool output was written by other agents or people; fence it as data.
fn fence_field(v: &mut Value, key: &str) {
    if let Some(t) = v[key].as_str().map(str::to_owned) {
        v[key] = json!(super::dispatch::fence(key, &t));
    }
}

fn fence_item(v: &mut Value) {
    for k in ["title", "plan", "type", "severity"] {
        fence_field(v, k);
    }
}

fn deciders(tdb: &TeamsDb, team_id: &str) -> Result<Vec<String>> {
    let roles = tdb.roles()?;
    Ok(tdb
        .members(team_id)?
        .into_iter()
        .filter(|m| roles.iter().any(|r| r.id == m.role_id && r.caps.decides))
        .map(|m| m.session_id)
        .collect())
}

/// Best-effort message to a teammate from `from` (a session) or from the user (None). A failure
/// is logged: the decision or item is already recorded.
async fn tell(state: &AppState, from: Option<&str>, to: &str, text: &str) {
    let r = match from {
        Some(f) => {
            let node = state.mesh.node_id().to_owned();
            match crate::agent_messaging::check_send_limits(f, to) {
                Ok(()) => crate::send_agent_message(state, &node, f, to, text).await.map(|_| ()),
                Err(e) => Err(e),
            }
        }
        None => async {
            if !state.code.is_running(to) {
                crate::code_start_local(state, to.to_owned()).await?;
            }
            let me = state.mesh.node_id().to_owned();
            crate::code_prompt_local(state, to.to_owned(), text.to_owned(), Vec::new(), Vec::new(), me, None, false).await.map(|_| ())
        }
        .await,
    };
    if let Err(e) = r {
        tracing::warn!("team message to {to}: {e:#}");
    }
}

pub(super) fn changed(state: &AppState, tdb: &TeamsDb, team_id: &str, event: &str) {
    export(tdb, &state.db, team_id, event);
    // The Teams page refreshes by polling; nothing else to tell.
}

fn roster(state: &AppState, tdb: &TeamsDb, c: &Caller) -> Result<Value> {
    let team = &c.team;
    let roles = tdb.roles()?;
    let items = tdb.items(&team.id, None)?;
    let raw_members = tdb.members(&team.id)?;
    let members: Vec<Value> = raw_members
        .iter()
        .map(|m| {
            let s = state.db.get_code_session(&m.session_id).ok().flatten();
            let local = s.as_ref().is_some_and(|s| s.host_node.as_deref() == Some(state.mesh.node_id()));
            let held = items.iter().find(|i| i.member_session.as_deref() == Some(&m.session_id) && !matches!(i.status.as_str(), "done" | "dropped"));
            json!({
                "sessionId": m.session_id,
                "role": roles.iter().find(|r| r.id == m.role_id).map(|r| super::dispatch::strip_forged_tags(&r.name)),
                "seat": super::dispatch::strip_forged_tags(&m.seat),
                "agent": s.as_ref().map(|s| super::dispatch::strip_forged_tags(&s.agent)),
                "model": s.as_ref().and_then(|s| s.agent_model.as_deref().or(s.model_ref.as_deref()).map(super::dispatch::strip_forged_tags)),
                "activity": local.then(|| session_activity(state, &m.session_id).as_str()),
                "item": held.map(|i| i.id.clone()),
            })
        })
        .collect();

    let seats: Vec<Value> = match tdb.template(&team.template_id) {
        Ok(tpl) => tpl
            .seats
            .iter()
            .map(|s| {
                let filled = tdb.seat_load(&team.id, &s.seat).unwrap_or(0) as u32;
                let open = s.count.saturating_sub(filled);
                let role_name = roles
                    .iter()
                    .find(|r| r.id == s.role_id)
                    .map_or("", |r| r.name.as_str());
                let mut agents: Vec<String> = vec![s.agent.clone()];
                for ch in &s.choices {
                    if !agents.contains(&ch.agent) {
                        agents.push(ch.agent.clone());
                    }
                }
                json!({
                    "seat": super::dispatch::strip_forged_tags(&s.seat),
                    "role": super::dispatch::strip_forged_tags(role_name),
                    "agents": agents.iter().map(|a| super::dispatch::strip_forged_tags(a)).collect::<Vec<_>>(),
                    "count": s.count,
                    "filled": filled,
                    "open": open,
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    let mut out = serde_json::Map::new();
    out.insert("members".into(), json!(members));
    out.insert("seats".into(), json!(seats));
    if c.role.caps.dispatches {
        out.insert("hint".into(), json!("Start a worker in an open seat with dispatch_item(itemId)."));
    }
    Ok(Value::Object(out))
}

/// The session `to` names: a member's session id, or the one other member holding that role.
fn resolve_target(tdb: &TeamsDb, c: &Caller, to: &str) -> Result<String> {
    let roles = tdb.roles()?;
    let members = tdb.members(&c.team.id)?;
    if let Some(m) = members.iter().find(|m| m.session_id == to) {
        return Ok(m.session_id.clone());
    }
    let hits: Vec<&Member> = members
        .iter()
        .filter(|m| m.session_id != c.me.session_id && roles.iter().any(|r| r.id == m.role_id && same(&r.name, to)))
        .collect();
    match hits.as_slice() {
        [one] => Ok(one.session_id.clone()),
        [] => {
            if c.role.caps.dispatches {
                if let Ok(tpl) = tdb.template(&c.team.template_id) {
                    if let Some(s) = tpl.seats.iter().find(|s| {
                        (same(&s.seat, to) || roles.iter().any(|r| r.id == s.role_id && same(&r.name, to)))
                            && tdb.seat_load(&c.team.id, &s.seat).unwrap_or(0) == 0
                    }) {
                        bail!("no member in seat {}; start one with dispatch_item", super::dispatch::strip_forged_tags(&s.seat));
                    }
                }
            }
            bail!("no teammate with role or session id {to}; see team_roster")
        }
        many => bail!("{} members hold the role {to}; use one of their session ids: {}", many.len(), many.iter().map(|m| m.session_id.as_str()).collect::<Vec<_>>().join(", ")),
    }
}

// ---------------- commands for the UI ----------------

/// Items, decisions and recent events of a team, for the Teams page.
pub(crate) async fn team_board(state: &AppState, team_id: String) -> CmdResult<Value> {
    let tdb = open(state)?;
    tdb.team(&team_id)?;
    Ok(json!({
        "statuses": STATUSES,
        "items": tdb.items(&team_id, None)?,
        "decisions": tdb.decisions(&team_id)?,
        "events": tdb.item_events(&team_id, None, 30)?,
    }))
}

/// The user answers a decision from the UI, never through an MCP tool.
pub(crate) async fn team_decision_answer(state: &AppState, team_id: String, decision_id: String, answer: String) -> CmdResult<Decision> {
    let d = {
        let tdb = open(state)?;
        let d = tdb.answer_user(&team_id, &decision_id, &answer)?;
        export(&tdb, &state.db, &team_id, &format!("{}: user answered", d.id));
        d
    };
    if let Some(opener) = &d.opened_by {
        tell(state, None, opener, &format!("Decision {}: the user answered: {}", d.id, answer.trim())).await;
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (PathBuf, TeamsDb, Team) {
        let d = std::env::temp_dir().join(format!("pb-items-{}", new_id()));
        std::fs::create_dir_all(d.join("proj")).unwrap();
        let t = TeamsDb::open(&d).unwrap();
        let team = t
            .create_team(NewTeam {
                template_id: "builtin-triage-desk".into(),
                slug: "demo".into(),
                cwd: d.join("proj").to_string_lossy().into_owned(),
                id_prefix: Some("DM".into()),
                pr_base: None,
                worktree_root: None,
                project_rules: None,
                export_dir: Some(d.join("out").to_string_lossy().into_owned()),
            })
            .unwrap();
        (d, t, team)
    }

    #[test]
    fn every_tool_has_a_definition() {
        let defs: Vec<String> = tool_defs().iter().map(|d| d["name"].as_str().unwrap().to_owned()).collect();
        assert_eq!(defs, TOOLS);
    }

    #[test]
    fn ids_roll_over_per_day_and_are_never_reused() {
        let (_d, t, team) = setup();
        let a = t.add_item(&team, None, "20261109", "one", "bug", "high", "").unwrap();
        let b = t.add_item(&team, None, "20261109", "two", "bug", "high", "").unwrap();
        let c = t.add_item(&team, None, "20261110", "three", "bug", "high", "").unwrap();
        assert_eq!((a.id.as_str(), b.id.as_str(), c.id.as_str()), ("DM-20261109-01", "DM-20261109-02", "DM-20261110-01"));
        // Dropping an item doesn't free its number.
        t.set_item(&team.id, &b.id, None, &json!({ "status": "dropped" })).unwrap();
        assert_eq!(t.add_item(&team, None, "20261109", "four", "bug", "high", "").unwrap().id, "DM-20261109-03");
        assert!(t.add_item(&team, None, "20261109", "  ", "bug", "high", "").is_err());
    }

    #[test]
    fn set_validates_and_leaves_events_and_events_are_append_only() {
        let (_d, t, team) = setup();
        t.attach(&team.id, "w1", "workers").unwrap();
        let i = t.add_item(&team, Some("desk1"), "20261109", "x", "bug", "low", "").unwrap();
        assert!(t.set_item(&team.id, &i.id, None, &json!({ "status": "nope" })).is_err());
        assert!(t.set_item(&team.id, &i.id, None, &json!({ "colour": "red" })).is_err());
        assert!(t.set_item(&team.id, &i.id, None, &json!({ "assignee": "stranger" })).is_err());
        let s = t.set_item(&team.id, &i.id, Some("desk1"), &json!({ "status": "triaged", "assignee": "w1", "prUrl": "u" })).unwrap();
        assert_eq!((s.status.as_str(), s.member_session.as_deref(), s.pr_url.as_deref()), ("triaged", Some("w1"), Some("u")));
        assert!(t.set_item(&team.id, &i.id, None, &json!({ "status": "triaged" })).unwrap_err().to_string().contains("nothing changed"));
        let ev = t.item_events(&team.id, Some(&i.id), 50).unwrap();
        assert_eq!(ev.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), ["created", "set", "set", "set"]);
        assert!(t.0.execute("UPDATE item_events SET text = 'x'", []).unwrap_err().to_string().contains("append-only"));
        assert!(t.0.execute("DELETE FROM item_events", []).unwrap_err().to_string().contains("append-only"));
    }

    #[test]
    fn decisions_route_by_class_and_flag_disagreement() {
        let (_d, t, team) = setup();
        let human = t.open_decision(&team, Some("desk1"), None, "Ship it?", &[], None, "human").unwrap();
        assert_eq!(human.status, "awaiting_user");
        assert_eq!(t.open_decision(&team, None, None, "mechanics", &[], None, "coordinator").unwrap().status, "resolved");
        assert!(t.open_decision(&team, None, None, "q", &[], None, "bogus").is_err());

        let opts = vec!["A".to_owned(), "B".to_owned()];
        let d = t.open_decision(&team, Some("desk1"), None, "Which?", &opts, Some("A"), "advisor_first").unwrap();
        assert_eq!((d.id.as_str(), d.status.as_str()), ("DM-D3", "awaiting_advisor"));
        assert!(t.advise(&team.id, &d.id, None, "CLEAR", None, None).is_err(), "CLEAR names a choice");
        assert!(t.advise(&team.id, &d.id, None, "CLEAR", Some("C"), None).is_err(), "choice is one of the options");
        assert!(t.advise(&team.id, &d.id, None, "MAYBE", None, None).is_err());
        let cleared = t.advise(&team.id, &d.id, None, "clear", Some("b"), Some("cheaper")).unwrap();
        assert_eq!((cleared.status.as_str(), cleared.disagrees), ("cleared", true));
        assert!(t.advise(&team.id, &d.id, None, "HUMAN", None, None).is_err(), "answered once");

        let d2 = t.open_decision(&team, None, None, "Again?", &opts, Some("A"), "advisor_first").unwrap();
        let h = t.advise(&team.id, &d2.id, None, "HUMAN", None, Some("your call")).unwrap();
        assert_eq!((h.status.as_str(), h.disagrees), ("awaiting_user", false));
        let done = t.answer_user(&team.id, &d2.id, "A").unwrap();
        assert_eq!((done.status.as_str(), done.user_answer.as_deref()), ("resolved", Some("A")));
        assert!(t.answer_user(&team.id, &d2.id, "B").is_err());
    }

    fn setup_with_git_worktree() -> (PathBuf, TeamsDb, Team) {
        let d = std::env::temp_dir().join(format!("pb-items-git-{}", new_id()));
        let proj = d.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let t = TeamsDb::open(&d).unwrap();
        let team = t
            .create_team(NewTeam {
                template_id: "builtin-triage-desk".into(),
                slug: "demo".into(),
                cwd: proj.to_string_lossy().into_owned(),
                id_prefix: Some("DM".into()),
                pr_base: None,
                worktree_root: Some(proj.join("trees").to_string_lossy().into_owned()),
                project_rules: None,
                export_dir: Some(d.join("out").to_string_lossy().into_owned()),
            })
            .unwrap();
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git").arg("-C").arg(&proj).args(args).output().unwrap().status.success(),
                "{args:?}"
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "base"]);
        (d, t, team)
    }

    fn add_item_worktree(team: &Team, item_id: &str) -> PathBuf {
        let branch = item_id.to_lowercase();
        let path = Path::new(&team.worktree_root).join(&branch);
        std::fs::create_dir_all(&team.worktree_root).unwrap();
        let out = std::process::Command::new("git")
            .args(["-C", &team.cwd, "worktree", "add", "-b", &branch, "--", path.to_str().unwrap(), &team.pr_base])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        path
    }

    fn seed_dispatch(t: &TeamsDb, team: &Team, item_id: &str) {
        t.0.execute(
            "INSERT INTO dispatches(team_id, item_id, seat, desk_session, at) VALUES (?1, ?2, 'workers', 'desk1', 0)",
            params![team.id, item_id],
        )
        .unwrap();
    }

    #[test]
    fn done_requires_clean_commits_ahead_or_pr_url() {
        let (_d, t, team) = setup_with_git_worktree();
        let i = t.add_item(&team, Some("desk1"), "20261109", "Ship it", "task", "normal", "").unwrap();
        seed_dispatch(&t, &team, &i.id);
        let wt = add_item_worktree(&team, &i.id);

        // Clean worktree at base: no commit ahead.
        let err = t.set_item(&team.id, &i.id, Some("desk1"), &json!({ "status": "done" })).unwrap_err().to_string();
        assert_eq!(err, DONE_REQUIRES_COMMITS_AHEAD);

        // Dirty worktree (untracked file).
        std::fs::write(wt.join("LICENSE"), "MIT").unwrap();
        let err = t.set_item(&team.id, &i.id, Some("desk1"), &json!({ "status": "done" })).unwrap_err().to_string();
        assert_eq!(err, DONE_REQUIRES_CLEAN_WORKTREE);

        // PR URL bypasses local branch evidence.
        assert_eq!(
            t.set_item(&team.id, &i.id, Some("desk1"), &json!({ "status": "done", "prUrl": "https://example.test/pr/1" }))
                .unwrap()
                .status,
            "done"
        );

        // Reset to triaged for the clean+ahead path on a second item.
        let i2 = t.add_item(&team, Some("desk1"), "20261109", "Ship more", "task", "normal", "").unwrap();
        seed_dispatch(&t, &team, &i2.id);
        let wt2 = add_item_worktree(&team, &i2.id);
        std::fs::write(wt2.join("README.md"), "done").unwrap();
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git").arg("-C").arg(&wt2).args(args).output().unwrap().status.success(),
                "{args:?}"
            );
        };
        run(&["add", "README.md"]);
        run(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "work"]);
        assert_eq!(t.set_item(&team.id, &i2.id, Some("desk1"), &json!({ "status": "done" })).unwrap().status, "done");
    }

    #[test]
    fn awaiting_review_and_done_require_a_dispatch_row() {
        let (_d, t, team) = setup();
        let i = t.add_item(&team, Some("desk1"), "20261109", "Implement it", "task", "normal", "").unwrap();
        assert_eq!(i.status, "noticed");

        let err = t.set_item(&team.id, &i.id, Some("desk1"), &json!({ "status": "awaiting-review" })).unwrap_err().to_string();
        assert_eq!(err, DISPATCH_REQUIRED_FOR_FINISH);
        let err = t.set_item(&team.id, &i.id, Some("desk1"), &json!({ "status": "done" })).unwrap_err().to_string();
        assert_eq!(err, DISPATCH_REQUIRED_FOR_FINISH);
        assert_eq!(t.item(&team.id, &i.id).unwrap().status, "noticed");

        // Earlier statuses still work without a dispatch.
        t.set_item(&team.id, &i.id, Some("desk1"), &json!({ "status": "triaged" })).unwrap();

        t.0.execute(
            "INSERT INTO dispatches(team_id, item_id, seat, desk_session, at) VALUES (?1, ?2, 'workers', 'desk1', 0)",
            params![team.id, i.id],
        )
        .unwrap();
        let reviewed = t.set_item(&team.id, &i.id, Some("desk1"), &json!({ "status": "awaiting-review" })).unwrap();
        assert_eq!(reviewed.status, "awaiting-review");
        assert_eq!(t.set_item(&team.id, &i.id, Some("desk1"), &json!({ "status": "done" })).unwrap().status, "done");
    }

    #[test]
    fn create_agent_session_refuses_active_team_members_only() {
        let (d, t, team) = setup();
        // No membership yet: non-members (and later, released members) may create sessions.
        refuse_create_if_team_member(&d, "outsider").unwrap();
        refuse_create_if_team_member(&d, "desk1").unwrap();

        t.attach(&team.id, "desk1", "coordinator").unwrap();
        let err = refuse_create_if_team_member(&d, "desk1").unwrap_err().to_string();
        assert_eq!(err, TEAM_MEMBER_CREATE_REFUSAL);
        assert!(err.contains("dispatch_item") && err.contains("raise the blocker"), "{err}");
        // Unrelated sessions stay allowed while a teammate is blocked.
        refuse_create_if_team_member(&d, "outsider").unwrap();

        t.detach(&team.id, "desk1").unwrap();
        refuse_create_if_team_member(&d, "desk1").unwrap();
    }

    #[test]
    fn advisor_first_without_eligible_decider_waits_for_user() {
        let (_d, t, team) = setup();
        let no_deciders = deciders(&t, &team.id).unwrap();
        assert!(no_deciders.is_empty());
        let d = t.open_decision_routed(&team, Some("desk1"), None, "Choose?", &["A".into()], Some("A"), "advisor_first", !no_deciders.is_empty()).unwrap();
        assert_eq!(d.status, "awaiting_user");
        assert_eq!(t.decision(&team.id, &d.id).unwrap().status, "awaiting_user");

        t.attach(&team.id, "advisor1", "advisor").unwrap();
        let eligible_deciders = deciders(&t, &team.id).unwrap();
        assert_eq!(eligible_deciders, ["advisor1"]);
        let routed = t.open_decision_routed(&team, Some("desk1"), None, "Advisor?", &[], None, "advisor_first", !eligible_deciders.is_empty()).unwrap();
        assert_eq!(routed.status, "awaiting_advisor");
    }

    #[test]
    fn empty_seat_resolution_shows_dispatch_hint_only_for_dispatchers() {
        let (_d, t, team) = setup();
        t.attach(&team.id, "desk1", "coordinator").unwrap();
        t.attach(&team.id, "w1", "workers").unwrap();
        let coord = Caller::of(&t, "desk1").unwrap();
        let worker = Caller::of(&t, "w1").unwrap();

        // lead seat is empty
        let err_coord = resolve_target(&t, &coord, "lead").unwrap_err().to_string();
        assert_eq!(err_coord, "no member in seat lead; start one with dispatch_item");

        let err_worker = resolve_target(&t, &worker, "lead").unwrap_err().to_string();
        assert_eq!(err_worker, "no teammate with role or session id lead; see team_roster");

        // Reserved seat counts as occupied, so resolve_target falls through instead of saying no member
        t.0.execute(
            "INSERT INTO members(team_id, session_id, seat, role_id, status, joined_at) VALUES (?1, 'res1', 'lead', 'builtin-lead', 'reserved', 0)",
            params![team.id],
        )
        .unwrap();
        let err_reserved = resolve_target(&t, &coord, "lead").unwrap_err().to_string();
        assert_eq!(err_reserved, "no teammate with role or session id lead; see team_roster");

        // Missing template falls through to standard error
        let mut broken_team = team.clone();
        broken_team.template_id = "nonexistent".into();
        let broken_coord = Caller { team: broken_team, me: coord.me.clone(), role: coord.role.clone() };
        let err_broken = resolve_target(&t, &broken_coord, "advisor").unwrap_err().to_string();
        assert_eq!(err_broken, "no teammate with role or session id advisor; see team_roster");
    }
}
