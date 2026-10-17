//! SQLite storage: providers, models, chat conversations, attachments, code sessions.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub struct Db(Mutex<Connection>);

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE providers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    base_url TEXT NOT NULL,
    inline_key TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE models (
    id TEXT PRIMARY KEY,
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    vision INTEGER NOT NULL DEFAULT 1,
    context_window INTEGER,
    UNIQUE(provider_id, model_id)
);
CREATE TABLE conversations (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    model_ref TEXT,
    system_prompt TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE messages (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    reasoning TEXT,
    model_ref TEXT,
    error TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX messages_conv ON messages(conversation_id, created_at);
CREATE TABLE attachments (
    id TEXT PRIMARY KEY,
    sha256 TEXT NOT NULL,
    name TEXT NOT NULL,
    mime TEXT NOT NULL,
    kind TEXT NOT NULL,
    size INTEGER NOT NULL,
    path TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE message_attachments (
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    attachment_id TEXT NOT NULL REFERENCES attachments(id),
    ord INTEGER NOT NULL,
    PRIMARY KEY (message_id, attachment_id)
);
CREATE TABLE code_sessions (
    id TEXT PRIMARY KEY,
    cwd TEXT NOT NULL,
    acp_session_id TEXT,
    title TEXT NOT NULL,
    model_ref TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#, r#"
ALTER TABLE messages ADD COLUMN usage_json TEXT;
ALTER TABLE code_sessions ADD COLUMN usage_json TEXT;
"#, r#"
ALTER TABLE models ADD COLUMN params_json TEXT;
"#, r#"
ALTER TABLE conversations ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE code_sessions ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
"#, r#"
CREATE TABLE mcp_servers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    config_json TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL
);
CREATE TABLE permission_rules (
    id TEXT PRIMARY KEY,
    cwd TEXT NOT NULL,
    tool_kind TEXT NOT NULL,
    pattern TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);
CREATE TABLE prompts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    content TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
ALTER TABLE code_sessions ADD COLUMN transcript_json TEXT;

-- Messages form a tree: edits and regenerations add siblings instead of deleting.
ALTER TABLE messages ADD COLUMN parent_id TEXT;
ALTER TABLE conversations ADD COLUMN active_leaf_id TEXT;
UPDATE messages SET parent_id = (
    SELECT m2.id FROM messages m2
    WHERE m2.conversation_id = messages.conversation_id
      AND (m2.created_at < messages.created_at
           OR (m2.created_at = messages.created_at AND m2.rowid < messages.rowid))
    ORDER BY m2.created_at DESC, m2.rowid DESC LIMIT 1
);
UPDATE conversations SET active_leaf_id = (
    SELECT m.id FROM messages m WHERE m.conversation_id = conversations.id
    ORDER BY m.created_at DESC, m.rowid DESC LIMIT 1
);
CREATE INDEX messages_parent ON messages(parent_id);

CREATE VIRTUAL TABLE messages_fts USING fts5(content, message_id UNINDEXED, conversation_id UNINDEXED);
INSERT INTO messages_fts(rowid, content, message_id, conversation_id)
    SELECT rowid, content, id, conversation_id FROM messages;
CREATE TRIGGER messages_fts_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, content, message_id, conversation_id)
    VALUES (new.rowid, new.content, new.id, new.conversation_id);
END;
CREATE TRIGGER messages_fts_ad AFTER DELETE ON messages BEGIN
    DELETE FROM messages_fts WHERE rowid = old.rowid;
END;
CREATE TRIGGER messages_fts_au AFTER UPDATE OF content ON messages BEGIN
    UPDATE messages_fts SET content = new.content WHERE rowid = old.rowid;
END;
"#, r#"
-- What a Code session has done, for its sidebar icon: chat, edit, commit, branch or pr.
-- NULL until the UI has looked at the session's transcript.
ALTER TABLE code_sessions ADD COLUMN activity TEXT;
"#, r#"
-- Which coding agent runs the session (an AgentKind id), and its own model id for agents
-- that don't use Patchbay's models.
ALTER TABLE code_sessions ADD COLUMN agent TEXT NOT NULL DEFAULT 'grok';
ALTER TABLE code_sessions ADD COLUMN agent_model TEXT;
"#, r#"
-- Mesh sync (see src/mesh/sync.rs). `node_id` names this install; `seq` and `hlc` advance on every
-- local change; `applying` is 1 while remote changes are written, so triggers don't re-log them.
CREATE TABLE sync_meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    node_id TEXT NOT NULL,
    seq INTEGER NOT NULL DEFAULT 0,
    hlc INTEGER NOT NULL DEFAULT 0,
    applying INTEGER NOT NULL DEFAULT 0
);
INSERT INTO sync_meta (id, node_id) VALUES (1, lower(hex(randomblob(16))));
-- One clock per synced cell: who wrote it last, and when. `col` '-' is a row tombstone.
CREATE TABLE sync_clock (
    tbl TEXT NOT NULL,
    pk TEXT NOT NULL,
    col TEXT NOT NULL,
    hlc INTEGER NOT NULL,
    origin TEXT NOT NULL,
    seq INTEGER NOT NULL,
    PRIMARY KEY (tbl, pk, col)
) WITHOUT ROWID;
CREATE INDEX sync_clock_origin ON sync_clock(origin, seq);
-- Highest change seq received from each origin.
CREATE TABLE sync_vv (origin TEXT PRIMARY KEY, seq INTEGER NOT NULL);
-- Remote changes that could not be applied yet (e.g. their parent row hasn't arrived).
CREATE TABLE sync_parked (id INTEGER PRIMARY KEY, ops_json TEXT NOT NULL, created_at INTEGER NOT NULL);

-- Which node runs a Code session's agent, and a fencing token bumped on every handover.
ALTER TABLE code_sessions ADD COLUMN host_node TEXT;
ALTER TABLE code_sessions ADD COLUMN epoch INTEGER NOT NULL DEFAULT 0;
UPDATE code_sessions SET host_node = (SELECT node_id FROM sync_meta);

-- Mesh membership: signed admission (and removal) records.
CREATE TABLE mesh_members (
    node_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    sign_pub TEXT NOT NULL,
    noise_pub TEXT NOT NULL,
    mesh_id TEXT NOT NULL,
    approver TEXT NOT NULL,
    admitted_at INTEGER NOT NULL,
    sig TEXT NOT NULL,
    removed_by TEXT,
    removed_at INTEGER,
    removed_sig TEXT
);
-- Secrets (API keys, MCP secrets) sealed under the mesh data key; `account` is the keyring account.
CREATE TABLE mesh_secrets (
    account TEXT PRIMARY KEY,
    gen INTEGER NOT NULL,
    nonce TEXT NOT NULL,
    ct TEXT NOT NULL,
    digest TEXT NOT NULL
);
"#];

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiChat,
    OpenaiResponses,
    Anthropic,
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai_chat",
            Self::OpenaiResponses => "openai_responses",
            Self::Anthropic => "anthropic",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "openai_responses" => Self::OpenaiResponses,
            "anthropic" => Self::Anthropic,
            _ => Self::OpenaiChat,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    /// True when an API key is stored (keyring or inline fallback). The key itself never leaves Rust.
    #[serde(default)]
    pub has_key: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub provider_id: String,
    pub model_id: String,
    pub display_name: String,
    pub vision: bool,
    pub context_window: Option<i64>,
    #[serde(default)]
    pub params: ModelParams,
}

/// Optional per-model request parameters and user-entered pricing. `None` = provider default.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelParams {
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    /// none | minimal | low | medium | high | xhigh | max
    pub reasoning_effort: Option<String>,
    /// Responses API only: none | auto | concise | detailed
    pub reasoning_summary: Option<String>,
    /// Code agent: compact the conversation when context reaches this percentage.
    pub auto_compact_percent: Option<u8>,
    pub pricing: Pricing,
}

/// USD per 1M tokens, used to estimate cost when the provider reports none.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Pricing {
    pub input: Option<f64>,
    pub cached_input: Option<f64>,
    pub cache_write: Option<f64>,
    pub output: Option<f64>,
}

impl ModelParams {
    pub fn reasoning_effort(&self) -> Option<xai_grok_sampling_types::ReasoningEffort> {
        let e = self.reasoning_effort.as_deref()?;
        serde_json::from_value(serde_json::Value::String(e.to_owned())).ok()
    }

    pub fn reasoning_summary(&self) -> Option<xai_grok_sampling_types::ReasoningSummary> {
        let e = self.reasoning_summary.as_deref()?;
        serde_json::from_value(serde_json::Value::String(e.to_owned())).ok()
    }

    /// Reject values providers would refuse, so mistakes surface in Settings, not mid-chat.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if let Some(t) = self.temperature
            && !(0.0..=2.0).contains(&t)
        {
            return Err("temperature must be between 0 and 2".into());
        }
        if let Some(p) = self.top_p
            && !(0.0..=1.0).contains(&p)
        {
            return Err("top P must be between 0 and 1".into());
        }
        if self.max_output_tokens == Some(0) {
            return Err("max output tokens must be positive".into());
        }
        if let Some(p) = self.auto_compact_percent
            && !(10..=99).contains(&p)
        {
            return Err("auto-compact threshold must be between 10 and 99%".into());
        }
        if self.reasoning_effort.is_some() && self.reasoning_effort().is_none() {
            return Err("unknown reasoning effort".into());
        }
        if self.reasoning_summary.is_some() && self.reasoning_summary().is_none() {
            return Err("unknown reasoning summary".into());
        }
        let p = &self.pricing;
        if [p.input, p.cached_input, p.cache_write, p.output]
            .iter()
            .flatten()
            .any(|v| !v.is_finite() || *v < 0.0)
        {
            return Err("prices must be zero or positive".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub model_ref: Option<String>,
    pub system_prompt: Option<String>,
    pub pinned: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub id: String,
    pub sha256: String,
    pub name: String,
    pub mime: String,
    /// "image" | "text" | "pdf"
    pub kind: String,
    pub size: i64,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub content: String,
    pub reasoning: Option<String>,
    pub model_ref: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub attachments: Vec<Attachment>,
    /// Token usage of this assistant reply (`usage::Tokens`).
    pub usage: Option<serde_json::Value>,
    pub parent_id: Option<String>,
    /// This message and its alternatives (same parent and role), oldest first.
    pub sibling_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// `McpConfig` with secret values blanked; secrets live in the keyring.
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    pub id: String,
    /// Folder the rule applies to; `*` means every folder.
    pub cwd: String,
    /// ACP tool kind (`edit`, `execute`, `read`, …).
    pub tool_kind: String,
    /// For `execute`: command prefix; empty matches any call of this kind.
    pub pattern: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prompt {
    pub id: String,
    pub name: String,
    pub content: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub conversation_id: String,
    pub conversation_title: String,
    pub message_id: String,
    pub role: String,
    /// Snippet with matches wrapped in \u{1}…\u{2}.
    pub snippet: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatUsageRow {
    /// Local calendar day, `YYYY-MM-DD`.
    pub day: String,
    pub model_ref: Option<String>,
    pub replies: i64,
    pub usage: crate::usage::Tokens,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeUsageRow {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub model_ref: Option<String>,
    pub updated_at: i64,
    pub usage: crate::usage::Tokens,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeSession {
    pub id: String,
    pub cwd: String,
    pub acp_session_id: Option<String>,
    pub title: String,
    pub model_ref: Option<String>,
    pub pinned: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub activity: Option<String>,
    /// `AgentKind` id of the agent that runs this session.
    pub agent: String,
    /// The agent's own model id, for agents that keep their own model list.
    pub agent_model: Option<String>,
    /// Node running this session's agent.
    pub host_node: Option<String>,
    /// Bumped on every handover between nodes.
    pub epoch: i64,
}

/// Values `code_sessions.activity` may hold.
pub const CODE_ACTIVITIES: &[&str] = &["chat", "edit", "commit", "branch", "pr"];

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            conn.execute_batch(sql)?;
            conn.pragma_update(None, "user_version", (i + 1) as i64)?;
        }
        crate::mesh::sync::install(&conn)?;
        Ok(Self(Mutex::new(conn)))
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Raw access for the mesh sync engine.
    pub(crate) fn with_conn<R>(&self, f: impl FnOnce(&mut Connection) -> R) -> R {
        f(&mut self.conn())
    }

    /// This install's mesh node id.
    pub fn node_id(&self) -> String {
        self.conn()
            .query_row("SELECT node_id FROM sync_meta", [], |r| r.get(0))
            .unwrap_or_default()
    }

    // ---- providers ----

    pub fn list_providers(&self) -> Result<Vec<(Provider, Option<String>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, base_url, inline_key FROM providers ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                Provider {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: ProviderKind::parse(&r.get::<_, String>(2)?),
                    base_url: r.get(3)?,
                    has_key: false,
                },
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_provider(&self, id: &str) -> Result<Option<(Provider, Option<String>)>> {
        Ok(self.list_providers()?.into_iter().find(|(p, _)| p.id == id))
    }

    pub fn upsert_provider(&self, p: &Provider, inline_key: Option<&str>) -> Result<()> {
        self.conn().execute(
            "INSERT INTO providers (id, name, kind, base_url, inline_key, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET name = ?2, kind = ?3, base_url = ?4,
               inline_key = COALESCE(?5, inline_key)",
            params![p.id, p.name, p.kind.as_str(), p.base_url, inline_key, now_ms()],
        )?;
        Ok(())
    }

    pub fn clear_inline_key(&self, id: &str) -> Result<()> {
        self.conn()
            .execute("UPDATE providers SET inline_key = NULL WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn delete_provider(&self, id: &str) -> Result<()> {
        self.conn().execute("DELETE FROM providers WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- models ----

    pub fn list_models(&self) -> Result<Vec<Model>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT m.id, m.provider_id, m.model_id, m.display_name, m.vision, m.context_window,
                    m.params_json
             FROM models m JOIN providers p ON p.id = m.provider_id
             ORDER BY p.created_at, m.display_name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Model {
                id: r.get(0)?,
                provider_id: r.get(1)?,
                model_id: r.get(2)?,
                display_name: r.get(3)?,
                vision: r.get::<_, i64>(4)? != 0,
                context_window: r.get(5)?,
                params: r
                    .get::<_, Option<String>>(6)?
                    .and_then(|j| serde_json::from_str(&j).ok())
                    .unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_model(&self, id: &str) -> Result<Option<Model>> {
        Ok(self.list_models()?.into_iter().find(|m| m.id == id))
    }

    pub fn upsert_model(&self, m: &Model) -> Result<()> {
        self.conn().execute(
            "INSERT INTO models (id, provider_id, model_id, display_name, vision, context_window,
                                 params_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET model_id = ?3, display_name = ?4, vision = ?5,
               context_window = ?6, params_json = ?7",
            params![
                m.id,
                m.provider_id,
                m.model_id,
                m.display_name,
                m.vision as i64,
                m.context_window,
                serde_json::to_string(&m.params)?
            ],
        )?;
        Ok(())
    }

    pub fn delete_model(&self, id: &str) -> Result<()> {
        self.conn().execute("DELETE FROM models WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- conversations ----

    pub fn list_conversations(&self) -> Result<Vec<Conversation>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, title, model_ref, system_prompt, created_at, updated_at, pinned
             FROM conversations ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], map_conversation)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_conversation(&self, id: &str) -> Result<Option<Conversation>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, title, model_ref, system_prompt, created_at, updated_at, pinned
                 FROM conversations WHERE id = ?1",
                [id],
                map_conversation,
            )
            .optional()?)
    }

    pub fn create_conversation(&self, model_ref: Option<&str>) -> Result<Conversation> {
        let now = now_ms();
        let c = Conversation {
            id: new_id(),
            title: "New chat".into(),
            model_ref: model_ref.map(str::to_owned),
            system_prompt: None,
            pinned: false,
            created_at: now,
            updated_at: now,
        };
        self.conn().execute(
            "INSERT INTO conversations (id, title, model_ref, system_prompt, created_at, updated_at)
             VALUES (?1, ?2, ?3, NULL, ?4, ?4)",
            params![c.id, c.title, c.model_ref, now],
        )?;
        Ok(c)
    }

    pub fn update_conversation(
        &self,
        id: &str,
        title: Option<&str>,
        model_ref: Option<&str>,
        system_prompt: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn();
        if let Some(t) = title {
            conn.execute("UPDATE conversations SET title = ?2 WHERE id = ?1", params![id, t])?;
        }
        if let Some(m) = model_ref {
            conn.execute(
                "UPDATE conversations SET model_ref = ?2 WHERE id = ?1",
                params![id, m],
            )?;
        }
        if let Some(s) = system_prompt {
            conn.execute(
                "UPDATE conversations SET system_prompt = NULLIF(?2, '') WHERE id = ?1",
                params![id, s],
            )?;
        }
        Ok(())
    }

    /// Pinning doesn't count as activity, so `updated_at` is left alone.
    pub fn set_conversation_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        self.conn().execute(
            "UPDATE conversations SET pinned = ?2 WHERE id = ?1",
            params![id, pinned],
        )?;
        Ok(())
    }

    pub fn touch_conversation(&self, id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE conversations SET updated_at = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    pub fn delete_conversation(&self, id: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM conversations WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- messages ----

    /// The active branch: root to the conversation's active leaf.
    pub fn list_messages(&self, conversation_id: &str) -> Result<Vec<Message>> {
        let leaf: Option<String> = self
            .conn()
            .query_row(
                "SELECT active_leaf_id FROM conversations WHERE id = ?1",
                [conversation_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        match leaf {
            Some(leaf) => self.path_to(conversation_id, &leaf),
            None => Ok(Vec::new()),
        }
    }

    /// Messages from the root down to `message_id`, inclusive.
    pub fn path_to(&self, conversation_id: &str, message_id: &str) -> Result<Vec<Message>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "WITH RECURSIVE path(id, depth) AS (
                 SELECT id, 0 FROM messages WHERE id = ?2 AND conversation_id = ?1
                 UNION ALL
                 SELECT m.parent_id, p.depth + 1 FROM messages m JOIN path p ON m.id = p.id
                 WHERE m.parent_id IS NOT NULL
             )
             SELECT m.id, m.conversation_id, m.role, m.content, m.reasoning, m.model_ref, m.error,
                    m.created_at, m.usage_json, m.parent_id
             FROM path p JOIN messages m ON m.id = p.id
             ORDER BY p.depth DESC",
        )?;
        let mut msgs: Vec<Message> = stmt
            .query_map(params![conversation_id, message_id], |r| {
                Ok(Message {
                    id: r.get(0)?,
                    conversation_id: r.get(1)?,
                    role: r.get(2)?,
                    content: r.get(3)?,
                    reasoning: r.get(4)?,
                    model_ref: r.get(5)?,
                    error: r.get(6)?,
                    created_at: r.get(7)?,
                    attachments: Vec::new(),
                    usage: r
                        .get::<_, Option<String>>(8)?
                        .and_then(|j| serde_json::from_str(&j).ok()),
                    parent_id: r.get(9)?,
                    sibling_ids: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut att = conn.prepare(
            "SELECT a.id, a.sha256, a.name, a.mime, a.kind, a.size, a.path
             FROM message_attachments ma JOIN attachments a ON a.id = ma.attachment_id
             WHERE ma.message_id = ?1 ORDER BY ma.ord",
        )?;
        let mut sib = conn.prepare(
            "SELECT id FROM messages
             WHERE conversation_id = ?1 AND role = ?2 AND parent_id IS ?3
             ORDER BY created_at, rowid",
        )?;
        for m in &mut msgs {
            m.attachments = att
                .query_map([&m.id], map_attachment)?
                .collect::<rusqlite::Result<_>>()?;
            m.sibling_ids = sib
                .query_map(params![m.conversation_id, m.role, m.parent_id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
        }
        Ok(msgs)
    }

    /// The newest leaf under `message_id` (following the latest child at each level).
    pub fn latest_leaf(&self, message_id: &str) -> Result<String> {
        let conn = self.conn();
        let mut cur = message_id.to_owned();
        loop {
            let child: Option<String> = conn
                .query_row(
                    "SELECT id FROM messages WHERE parent_id = ?1
                     ORDER BY created_at DESC, rowid DESC LIMIT 1",
                    [&cur],
                    |r| r.get(0),
                )
                .optional()?;
            match child {
                Some(c) => cur = c,
                None => return Ok(cur),
            }
        }
    }

    pub fn set_active_leaf(&self, conversation_id: &str, message_id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE conversations SET active_leaf_id = ?2 WHERE id = ?1",
            params![conversation_id, message_id],
        )?;
        Ok(())
    }

    pub fn active_leaf(&self, conversation_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT active_leaf_id FROM conversations WHERE id = ?1",
                [conversation_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Full-text search over message content, newest first.
    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let Some(fts) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT f.conversation_id, c.title, f.message_id, m.role,
                    snippet(messages_fts, 0, char(1), char(2), '…', 12), m.created_at
             FROM messages_fts f
             JOIN messages m ON m.rowid = f.rowid
             JOIN conversations c ON c.id = f.conversation_id
             WHERE messages_fts MATCH ?1
             ORDER BY m.created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![fts, limit as i64], |r| {
            Ok(SearchHit {
                conversation_id: r.get(0)?,
                conversation_title: r.get(1)?,
                message_id: r.get(2)?,
                role: r.get(3)?,
                snippet: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Insert a message under `parent_id` and make it the conversation's active leaf.
    pub fn insert_message(
        &self,
        conversation_id: &str,
        parent_id: Option<&str>,
        role: &str,
        content: &str,
        model_ref: Option<&str>,
        attachment_ids: &[String],
    ) -> Result<String> {
        let id = new_id();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, parent_id, role, content, model_ref, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, conversation_id, parent_id, role, content, model_ref, now_ms()],
        )?;
        conn.execute(
            "UPDATE conversations SET active_leaf_id = ?2 WHERE id = ?1",
            params![conversation_id, id],
        )?;
        for (ord, aid) in attachment_ids.iter().enumerate() {
            conn.execute(
                "INSERT OR IGNORE INTO message_attachments (message_id, attachment_id, ord)
                 VALUES (?1, ?2, ?3)",
                params![id, aid, ord as i64],
            )?;
        }
        Ok(id)
    }

    pub fn finish_message(
        &self,
        id: &str,
        content: &str,
        reasoning: Option<&str>,
        error: Option<&str>,
        usage: Option<&crate::usage::Tokens>,
    ) -> Result<()> {
        let usage = usage.map(serde_json::to_string).transpose()?;
        self.conn().execute(
            "UPDATE messages SET content = ?2, reasoning = ?3, error = ?4, usage_json = ?5
             WHERE id = ?1",
            params![id, content, reasoning, error, usage],
        )?;
        Ok(())
    }

    // ---- attachments ----

    pub fn insert_attachment(&self, a: &Attachment) -> Result<()> {
        self.conn().execute(
            "INSERT INTO attachments (id, sha256, name, mime, kind, size, path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![a.id, a.sha256, a.name, a.mime, a.kind, a.size, a.path, now_ms()],
        )?;
        Ok(())
    }

    pub fn get_attachment(&self, id: &str) -> Result<Option<Attachment>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, sha256, name, mime, kind, size, path FROM attachments WHERE id = ?1",
                [id],
                map_attachment,
            )
            .optional()?)
    }

    // ---- code sessions ----

    pub fn list_code_sessions(&self) -> Result<Vec<CodeSession>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, cwd, acp_session_id, title, model_ref, created_at, updated_at, pinned, activity,
                    agent, agent_model, host_node, epoch
             FROM code_sessions ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], map_code_session)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_code_session(&self, id: &str) -> Result<Option<CodeSession>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, cwd, acp_session_id, title, model_ref, created_at, updated_at, pinned, activity,
                        agent, agent_model, host_node, epoch
                 FROM code_sessions WHERE id = ?1",
                [id],
                map_code_session,
            )
            .optional()?)
    }

    pub fn create_code_session(
        &self,
        cwd: &str,
        model_ref: Option<&str>,
        agent: &str,
    ) -> Result<CodeSession> {
        let now = now_ms();
        // The sidebar groups sessions under their folder; the UI retitles from the first prompt.
        let title = "New session".to_owned();
        let s = CodeSession {
            id: new_id(),
            cwd: cwd.to_owned(),
            acp_session_id: None,
            title,
            model_ref: model_ref.map(str::to_owned),
            pinned: false,
            created_at: now,
            updated_at: now,
            activity: None,
            agent: agent.to_owned(),
            agent_model: None,
            host_node: Some(self.node_id()),
            epoch: 0,
        };
        self.conn().execute(
            "INSERT INTO code_sessions (id, cwd, acp_session_id, title, model_ref, created_at, updated_at,
                                        agent, host_node, epoch)
             VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?5, ?6, ?7, 0)",
            params![s.id, s.cwd, s.title, s.model_ref, now, s.agent, s.host_node],
        )?;
        Ok(s)
    }

    pub fn update_code_session(
        &self,
        id: &str,
        acp_session_id: Option<&str>,
        title: Option<&str>,
        model_ref: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn();
        if let Some(a) = acp_session_id {
            conn.execute(
                "UPDATE code_sessions SET acp_session_id = ?2 WHERE id = ?1",
                params![id, a],
            )?;
        }
        if let Some(t) = title {
            conn.execute("UPDATE code_sessions SET title = ?2 WHERE id = ?1", params![id, t])?;
        }
        if let Some(m) = model_ref {
            conn.execute(
                "UPDATE code_sessions SET model_ref = ?2 WHERE id = ?1",
                params![id, m],
            )?;
        }
        conn.execute(
            "UPDATE code_sessions SET updated_at = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    /// Doesn't touch `updated_at`: recording what a session did isn't new activity in it.
    pub fn set_code_session_activity(&self, id: &str, activity: &str) -> Result<()> {
        if !CODE_ACTIVITIES.contains(&activity) {
            anyhow::bail!("unknown session activity {activity:?}");
        }
        self.conn().execute(
            "UPDATE code_sessions SET activity = ?2 WHERE id = ?1",
            params![id, activity],
        )?;
        Ok(())
    }

    pub fn set_code_session_agent_model(&self, id: &str, model: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE code_sessions SET agent_model = ?2 WHERE id = ?1",
            params![id, model],
        )?;
        Ok(())
    }

    /// Hand a session to `host`: bump the fencing epoch and record where its folder is there.
    pub fn set_code_session_host(&self, id: &str, host: &str, cwd: &str, acp_session_id: Option<&str>) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "UPDATE code_sessions SET host_node = ?2, epoch = epoch + 1, cwd = ?3, acp_session_id = ?4
             WHERE id = ?1",
            params![id, host, cwd, acp_session_id],
        )?;
        Ok(conn.query_row("SELECT epoch FROM code_sessions WHERE id = ?1", [id], |r| r.get(0))?)
    }

    pub fn set_code_session_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        self.conn().execute(
            "UPDATE code_sessions SET pinned = ?2 WHERE id = ?1",
            params![id, pinned],
        )?;
        Ok(())
    }

    pub fn code_usage(&self, id: &str) -> Result<crate::usage::CodeUsageState> {
        let raw: Option<String> = self
            .conn()
            .query_row(
                "SELECT usage_json FROM code_sessions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(raw
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default())
    }

    pub fn set_code_usage(&self, id: &str, u: &crate::usage::CodeUsageState) -> Result<()> {
        self.conn().execute(
            "UPDATE code_sessions SET usage_json = ?2 WHERE id = ?1",
            params![id, serde_json::to_string(u)?],
        )?;
        Ok(())
    }

    pub fn delete_code_session(&self, id: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM code_sessions WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn code_transcript(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT transcript_json FROM code_sessions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    pub fn set_code_transcript(&self, id: &str, json: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE code_sessions SET transcript_json = ?2 WHERE id = ?1",
            params![id, json],
        )?;
        Ok(())
    }

    // ---- MCP servers ----

    pub fn list_mcp_servers(&self) -> Result<Vec<McpServer>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, enabled, config_json FROM mcp_servers ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(McpServer {
                id: r.get(0)?,
                name: r.get(1)?,
                enabled: r.get(2)?,
                config: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn upsert_mcp_server(&self, m: &McpServer) -> Result<()> {
        self.conn().execute(
            "INSERT INTO mcp_servers (id, name, enabled, config_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET name = ?2, enabled = ?3, config_json = ?4",
            params![m.id, m.name, m.enabled, serde_json::to_string(&m.config)?, now_ms()],
        )?;
        Ok(())
    }

    pub fn delete_mcp_server(&self, id: &str) -> Result<()> {
        self.conn().execute("DELETE FROM mcp_servers WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- permission rules ----

    pub fn list_permission_rules(&self) -> Result<Vec<PermissionRule>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, cwd, tool_kind, pattern, created_at FROM permission_rules
             ORDER BY cwd, tool_kind, pattern",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PermissionRule {
                id: r.get(0)?,
                cwd: r.get(1)?,
                tool_kind: r.get(2)?,
                pattern: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_permission_rule(&self, cwd: &str, tool_kind: &str, pattern: &str) -> Result<PermissionRule> {
        let rule = PermissionRule {
            id: new_id(),
            cwd: cwd.to_owned(),
            tool_kind: tool_kind.to_owned(),
            pattern: pattern.to_owned(),
            created_at: now_ms(),
        };
        self.conn().execute(
            "INSERT INTO permission_rules (id, cwd, tool_kind, pattern, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![rule.id, rule.cwd, rule.tool_kind, rule.pattern, rule.created_at],
        )?;
        Ok(rule)
    }

    pub fn delete_permission_rule(&self, id: &str) -> Result<()> {
        self.conn().execute("DELETE FROM permission_rules WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- prompts ----

    pub fn list_prompts(&self) -> Result<Vec<Prompt>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, content, created_at, updated_at FROM prompts ORDER BY name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Prompt {
                id: r.get(0)?,
                name: r.get(1)?,
                content: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn save_prompt(&self, id: Option<&str>, name: &str, content: &str) -> Result<Prompt> {
        let now = now_ms();
        let id = id.map(str::to_owned).unwrap_or_else(new_id);
        self.conn().execute(
            "INSERT INTO prompts (id, name, content, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(id) DO UPDATE SET name = ?2, content = ?3, updated_at = ?4",
            params![id, name, content, now],
        )?;
        Ok(self
            .conn()
            .query_row(
                "SELECT id, name, content, created_at, updated_at FROM prompts WHERE id = ?1",
                [&id],
                |r| {
                    Ok(Prompt {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        content: r.get(2)?,
                        created_at: r.get(3)?,
                        updated_at: r.get(4)?,
                    })
                },
            )?)
    }

    pub fn delete_prompt(&self, id: &str) -> Result<()> {
        self.conn().execute("DELETE FROM prompts WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- usage report ----

    /// Chat replies since `since_ms`, summed per local day and model.
    pub fn chat_usage_since(&self, since_ms: i64) -> Result<Vec<ChatUsageRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT created_at, model_ref, usage_json FROM messages
             WHERE role = 'assistant' AND usage_json IS NOT NULL AND created_at >= ?1",
        )?;
        let rows: Vec<(i64, Option<String>, String)> = stmt
            .query_map([since_ms], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut by: std::collections::BTreeMap<(String, Option<String>), ChatUsageRow> = Default::default();
        for (ts, model_ref, json) in rows {
            let Ok(t) = serde_json::from_str::<crate::usage::Tokens>(&json) else { continue };
            let day = chrono::DateTime::from_timestamp_millis(ts)
                .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
                .unwrap_or_default();
            let row = by.entry((day.clone(), model_ref.clone())).or_insert_with(|| ChatUsageRow {
                day,
                model_ref,
                replies: 0,
                usage: Default::default(),
            });
            row.replies += 1;
            row.usage = row.usage.add(&t);
        }
        Ok(by.into_values().collect())
    }

    pub fn code_usage_rows(&self) -> Result<Vec<CodeUsageRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, title, cwd, model_ref, updated_at, usage_json FROM code_sessions
             WHERE usage_json IS NOT NULL ORDER BY updated_at DESC",
        )?;
        let rows: Vec<(String, String, String, Option<String>, i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, title, cwd, model_ref, updated_at, json)| {
                let u: crate::usage::CodeUsageState = serde_json::from_str(&json).ok()?;
                Some(CodeUsageRow { id, title, cwd, model_ref, updated_at, usage: u.view().totals })
            })
            .collect())
    }

    // ---- settings ----

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }
}

/// Turn free text into an FTS5 query: each term quoted, the last one a prefix match.
fn fts_query(q: &str) -> Option<String> {
    let terms: Vec<String> = q
        .split_whitespace()
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();
    let (last, rest) = terms.split_last()?;
    let mut out = rest.join(" ");
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(last);
    out.push('*');
    Some(out)
}

fn map_conversation(r: &rusqlite::Row<'_>) -> rusqlite::Result<Conversation> {
    Ok(Conversation {
        id: r.get(0)?,
        title: r.get(1)?,
        model_ref: r.get(2)?,
        system_prompt: r.get(3)?,
        created_at: r.get(4)?,
        updated_at: r.get(5)?,
        pinned: r.get(6)?,
    })
}

fn map_attachment(r: &rusqlite::Row<'_>) -> rusqlite::Result<Attachment> {
    Ok(Attachment {
        id: r.get(0)?,
        sha256: r.get(1)?,
        name: r.get(2)?,
        mime: r.get(3)?,
        kind: r.get(4)?,
        size: r.get(5)?,
        path: r.get(6)?,
    })
}

fn map_code_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<CodeSession> {
    Ok(CodeSession {
        id: r.get(0)?,
        cwd: r.get(1)?,
        acp_session_id: r.get(2)?,
        title: r.get(3)?,
        model_ref: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
        pinned: r.get(7)?,
        activity: r.get(8)?,
        agent: r.get(9)?,
        agent_model: r.get(10)?,
        host_node: r.get(11)?,
        epoch: r.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("pb-db-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite");
        (Db::open(&path).unwrap(), path)
    }

    #[test]
    fn code_session_activity_is_stored_without_bumping_updated_at() {
        let (db, _) = temp_db();
        let s = db.create_code_session("/tmp", None, "grok").unwrap();
        assert_eq!(db.get_code_session(&s.id).unwrap().unwrap().activity, None);
        db.set_code_session_activity(&s.id, "branch").unwrap();
        let got = db.get_code_session(&s.id).unwrap().unwrap();
        assert_eq!(got.activity.as_deref(), Some("branch"));
        assert_eq!(got.updated_at, s.updated_at);
        assert!(db.set_code_session_activity(&s.id, "nonsense").is_err());
    }

    #[test]
    fn branches_and_switching() {
        let (db, _) = temp_db();
        let c = db.create_conversation(None).unwrap();
        let u1 = db.insert_message(&c.id, None, "user", "hello", None, &[]).unwrap();
        let a1 = db.insert_message(&c.id, Some(&u1), "assistant", "hi", None, &[]).unwrap();
        let u2 = db.insert_message(&c.id, Some(&a1), "user", "first question", None, &[]).unwrap();
        let _a2 = db.insert_message(&c.id, Some(&u2), "assistant", "first answer", None, &[]).unwrap();
        // Edit u2: a sibling under a1.
        let u2b = db.insert_message(&c.id, Some(&a1), "user", "edited question", None, &[]).unwrap();
        let a2b = db.insert_message(&c.id, Some(&u2b), "assistant", "second answer", None, &[]).unwrap();

        let path = db.list_messages(&c.id).unwrap();
        let texts: Vec<&str> = path.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(texts, ["hello", "hi", "edited question", "second answer"]);
        assert_eq!(path[2].sibling_ids, vec![u2.clone(), u2b.clone()]);
        assert_eq!(path[0].sibling_ids, vec![u1.clone()]);

        // Switch back to the first branch: its newest leaf becomes active.
        let leaf = db.latest_leaf(&u2).unwrap();
        db.set_active_leaf(&c.id, &leaf).unwrap();
        let texts: Vec<String> = db.list_messages(&c.id).unwrap().into_iter().map(|m| m.content).collect();
        assert_eq!(texts, ["hello", "hi", "first question", "first answer"]);
        assert_eq!(db.latest_leaf(&u2b).unwrap(), a2b);
    }

    #[test]
    fn full_text_search_tracks_inserts_updates_and_deletes() {
        let (db, _) = temp_db();
        let c = db.create_conversation(None).unwrap();
        db.update_conversation(&c.id, Some("Borrow checker"), None, None).unwrap();
        let u = db.insert_message(&c.id, None, "user", "explain lifetimes please", None, &[]).unwrap();
        let a = db.insert_message(&c.id, Some(&u), "assistant", "", None, &[]).unwrap();
        db.finish_message(&a, "Lifetimes scope borrows.", None, None, None).unwrap();

        let hits = db.search_messages("lifetim", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].conversation_title, "Borrow checker");
        assert!(hits.iter().any(|h| h.snippet.contains('\u{1}')));
        assert!(db.search_messages("scope borrow", 10).unwrap().len() == 1);
        assert!(db.search_messages("\"\"", 10).unwrap().is_empty());
        db.delete_conversation(&c.id).unwrap();
        assert!(db.search_messages("lifetimes", 10).unwrap().is_empty());
    }

    #[test]
    fn migration_links_existing_messages_into_a_chain() {
        let dir = std::env::temp_dir().join(format!("pb-db-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite");
        {
            // A database as it was before message trees.
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..4] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 4).unwrap();
            conn.execute_batch(
                "INSERT INTO conversations (id, title, created_at, updated_at) VALUES ('c', 't', 1, 1);
                 INSERT INTO messages (id, conversation_id, role, content, created_at) VALUES
                   ('m1', 'c', 'user', 'one', 10), ('m2', 'c', 'assistant', 'two', 11),
                   ('m3', 'c', 'user', 'three', 12);",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let ids: Vec<String> = db.list_messages("c").unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["m1", "m2", "m3"]);
        assert_eq!(db.search_messages("three", 5).unwrap().len(), 1);
    }
}
