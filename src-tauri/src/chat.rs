//! Chat tab: plain streaming chat over `patchbay-llm`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use patchbay_llm::{Client, Event, Message as LlmMessage, Part, Request, Role, SystemBlock, ToolChoice, ToolResult};

use crate::attachments;
use crate::db::{Db, Message};
use crate::providers;
use crate::usage::Tokens;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ChatEvent {
    #[serde(rename_all = "camelCase")]
    Started {
        user_message_id: String,
        assistant_message_id: String,
    },
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Status {
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    Done {
        stop_reason: Option<String>,
        usage: Option<Tokens>,
    },
    Error {
        message: String,
    },
    Title {
        title: String,
    },
}

/// Where a reply's progress goes: the UI that asked, plus every other mesh node.
pub type ChatSink = Arc<dyn Fn(ChatEvent) + Send + Sync>;

/// Lets `send` call `channel.send(ev)` on the sink like on a Tauri channel.
pub struct Emit(pub ChatSink);

impl Emit {
    pub fn send(&self, ev: ChatEvent) -> Result<()> {
        (self.0)(ev);
        Ok(())
    }
}

impl Clone for Emit {
    fn clone(&self) -> Self {
        Emit(self.0.clone())
    }
}

#[derive(Default)]
pub struct ChatManager {
    running: Mutex<HashMap<String, CancellationToken>>,
}

impl ChatManager {
    pub fn cancel(&self, conversation_id: &str) {
        if let Some(t) = self.running.lock().unwrap().remove(conversation_id) {
            t.cancel();
        }
    }
}

/// `native_pdf`: send PDFs as documents (Anthropic and Gemini read them natively) instead of
/// our text extraction.
fn user_item(data_dir: &std::path::Path, db: &Db, m: &Message, native_pdf: bool) -> Result<LlmMessage> {
    let mut parts = Vec::new();
    for a in &m.attachments {
        let att = db
            .get_attachment(&a.id)?
            .ok_or_else(|| anyhow!("attachment {} missing", a.name))?;
        if native_pdf && att.kind == "pdf" {
            parts.push(attachments::pdf_document_part(&att)?);
            continue;
        }
        parts.extend(attachments::to_content_parts(attachments::prepare(
            data_dir, &att,
        )?));
    }
    if !m.content.is_empty() || parts.is_empty() {
        parts.push(Part::Text(m.content.clone()));
    }
    Ok(LlmMessage::with_parts(Role::User, parts))
}

/// The model's view of the branch ending at `leaf_id` (a user message).
fn build_history(
    data_dir: &std::path::Path,
    db: &Db,
    conversation_id: &str,
    leaf_id: &str,
    system_prompt: Option<&str>,
    native_pdf: bool,
) -> Result<Request> {
    let mut req = Request::default();
    if let Some(sp) = system_prompt.filter(|s| !s.trim().is_empty()) {
        req.system.push(SystemBlock::new(sp));
    }
    for m in db.path_to(conversation_id, leaf_id)? {
        match m.role.as_str() {
            "user" => req.messages.push(user_item(data_dir, db, &m, native_pdf)?),
            "assistant" if !m.content.is_empty() => {
                req.messages.push(LlmMessage::assistant(m.content.clone()))
            }
            _ => {}
        }
    }
    Ok(req)
}

fn derive_title(text: &str, attachment_names: &[String]) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let base = if line.is_empty() {
        attachment_names.first().cloned().unwrap_or_else(|| "New chat".into())
    } else {
        line.to_owned()
    };
    let mut t: String = base.chars().take(60).collect();
    if base.chars().count() > 60 {
        t.push('…');
    }
    t
}

/// Searches one reply may run through Patchbay's own search tool (providers with a search of their
/// own set their limit themselves).
const MAX_SEARCH_ROUNDS: usize = 5;

const SEARCH_HINT: &str = "You can search the web for current information. After answering from search \
results, end with a \"Sources:\" list of Markdown links to the pages you used.";

/// What a chat turn adds to the message tree.
pub enum Turn {
    /// A new user message. `parent`: `None` = after the active leaf, `Some(None)` = a new root
    /// (editing the first message), `Some(Some(id))` = under that message (editing a later one).
    Message {
        text: String,
        attachment_ids: Vec<String>,
        parent: Option<Option<String>>,
    },
    /// Another reply to an existing user message.
    Regenerate { user_message_id: String },
}

#[allow(clippy::too_many_arguments)]
pub async fn send(
    db: Arc<Db>,
    data_dir: PathBuf,
    manager: Arc<ChatManager>,
    conversation_id: String,
    model_ref: String,
    turn: Turn,
    reasoning_effort: Option<String>,
    channel: Emit,
    mesh: crate::mesh::Mesh,
) -> Result<()> {
    let conv = db
        .get_conversation(&conversation_id)?
        .ok_or_else(|| anyhow!("conversation not found"))?;
    let mut resolved = providers::resolve(&db, &model_ref)?;
    providers::ensure_token(&mut resolved).await?;
    if let Some(e) = reasoning_effort.filter(|e| !e.is_empty() && e != "default") {
        resolved.model.params.reasoning_effort = Some(e);
        if resolved.model.params.reasoning_effort().is_none() {
            return Err(anyhow!("unknown reasoning effort"));
        }
    }
    let first_message = db.list_messages(&conversation_id)?.is_empty();

    db.update_conversation(&conversation_id, None, Some(&model_ref), None)?;
    let (user_id, text, attachment_ids) = match turn {
        Turn::Message { text, attachment_ids, parent } => {
            if attachment_ids.len() > attachments::MAX_PER_MESSAGE {
                return Err(anyhow!(
                    "at most {} attachments per message",
                    attachments::MAX_PER_MESSAGE
                ));
            }
            let parent = match parent {
                None => db.active_leaf(&conversation_id)?,
                Some(p) => p,
            };
            let id = db.insert_message(&conversation_id, parent.as_deref(), "user", &text, None, &attachment_ids)?;
            (id, text, attachment_ids)
        }
        Turn::Regenerate { user_message_id } => {
            let user = db
                .path_to(&conversation_id, &user_message_id)?
                .pop()
                .filter(|m| m.role == "user")
                .ok_or_else(|| anyhow!("message to regenerate not found"))?;
            (user.id, user.content, Vec::new())
        }
    };
    let assistant_id =
        db.insert_message(&conversation_id, Some(&user_id), "assistant", "", Some(&model_ref), &[])?;
    db.touch_conversation(&conversation_id)?;
    let _ = channel.send(ChatEvent::Started {
        user_message_id: user_id.clone(),
        assistant_message_id: assistant_id.clone(),
    });

    // Placeholder title from the prompt; replaced by a generated one after the reply.
    let mut derived_title = None;
    if first_message && conv.title == "New chat" {
        let names: Vec<String> = attachment_ids
            .iter()
            .filter_map(|id| db.get_attachment(id).ok().flatten().map(|a| a.name))
            .collect();
        let title = derive_title(&text, &names);
        db.update_conversation(&conversation_id, Some(&title), None, None)?;
        let _ = channel.send(ChatEvent::Title { title: title.clone() });
        derived_title = Some(title);
    }

    // Attachments added on another node are fetched from a peer before they're read.
    for m in db.path_to(&conversation_id, &user_id)? {
        for a in &m.attachments {
            if !std::path::Path::new(&a.path).exists() {
                mesh.ensure_file(&a.sha256, &a.name).await?;
            }
        }
    }

    let mut request = {
        let db = db.clone();
        let data_dir = data_dir.clone();
        let cid = conversation_id.clone();
        let sp = conv.system_prompt.clone();
        let leaf = user_id.clone();
        let native_pdf = matches!(
            resolved.provider.kind,
            crate::db::ProviderKind::Anthropic | crate::db::ProviderKind::Gemini
        );
        tokio::task::spawn_blocking(move || build_history(&data_dir, &db, &cid, &leaf, sp.as_deref(), native_pdf))
            .await??
    };
    // The provider's own search where it has one, else Patchbay's Brave search tool.
    let search = crate::prefs::load(&db).chat_web_search;
    let brave_key = (search && !providers::native_web_search(&resolved))
        .then(|| providers::web_search_key(&db).map(|(k, _)| k))
        .flatten();
    if search && (brave_key.is_some() || providers::native_web_search(&resolved)) {
        request.system.push(SystemBlock::new(SEARCH_HINT));
        request.web_search = brave_key.is_none();
        if brave_key.is_some() {
            request.tools = vec![patchbay_agent::web_search_spec()];
        }
    }
    providers::prepare_request(&resolved, &mut request);

    let token = CancellationToken::new();
    manager
        .running
        .lock()
        .unwrap()
        .insert(conversation_id.clone(), token.clone());

    let client = Client::new(providers::llm_config(&resolved));
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut error: Option<String> = None;
    let mut usage: Option<Tokens> = None;
    let mut cancelled = false;
    for round in 0..=MAX_SEARCH_ROUNDS {
        if round == MAX_SEARCH_ROUNDS {
            // Out of searches: the model has to answer with what it has.
            request.tool_choice = ToolChoice::None;
        }
        // Text before and after a search belongs in separate paragraphs.
        let mut separate = !content.is_empty();
        let result = client
            .complete(&request, &token, |ev| match ev {
                Event::Text(text) => {
                    if std::mem::take(&mut separate) {
                        content.push_str("\n\n");
                        let _ = channel.send(ChatEvent::Text { text: "\n\n".into() });
                    }
                    content.push_str(&text);
                    let _ = channel.send(ChatEvent::Text { text });
                }
                Event::Reasoning(text) => {
                    reasoning.push_str(&text);
                    let _ = channel.send(ChatEvent::Reasoning { text });
                }
                Event::Search(query) => {
                    let _ = channel.send(ChatEvent::Status { message: format!("Searching the web: {query}") });
                }
                Event::Retrying { attempt, max, reason } => {
                    let _ = channel.send(ChatEvent::Status {
                        message: format!("Retrying ({attempt}/{max}): {reason}"),
                    });
                }
                Event::ToolCall(_) => {}
            })
            .await;
        match result {
            Ok(response) => {
                let used = response.usage.as_ref().map(Tokens::from_llm);
                usage = match (usage, used) {
                    (Some(a), Some(b)) => Some(a.add(&b)),
                    (a, b) => a.or(b),
                };
                let calls: Vec<_> = response.message.tool_calls().cloned().collect();
                if let (Some(key), false, true) = (&brave_key, calls.is_empty(), round < MAX_SEARCH_ROUNDS) {
                    let mut results = Vec::new();
                    for call in &calls {
                        let query = call.arguments["query"].as_str().unwrap_or_default();
                        let _ = channel.send(ChatEvent::Status { message: format!("Searching the web: {query}") });
                        let out = if call.name == "WebSearch" {
                            patchbay_agent::brave_search(key, &call.arguments, &token, false).await
                        } else {
                            Err(format!("unknown tool {}", call.name))
                        };
                        let (text, is_error) = match out {
                            Ok(t) => (t, false),
                            Err(e) => (e, true),
                        };
                        results.push(Part::ToolResult(ToolResult::text(call, text, is_error)));
                    }
                    if token.is_cancelled() {
                        cancelled = true;
                        let _ = channel.send(ChatEvent::Done { stop_reason: Some("cancelled".into()), usage: None });
                        break;
                    }
                    request.messages.push(response.message);
                    request.messages.push(LlmMessage::with_parts(Role::User, results));
                    continue;
                }
                if content.is_empty() {
                    content = response.message.text();
                    if !content.is_empty() {
                        let _ = channel.send(ChatEvent::Text { text: content.clone() });
                    }
                }
                let _ = channel.send(ChatEvent::Done {
                    stop_reason: Some(response.stop.label().to_owned()),
                    usage,
                });
            }
            Err(e) if e.cancelled => {
                cancelled = true;
                let _ = channel.send(ChatEvent::Done { stop_reason: Some("cancelled".into()), usage: None });
            }
            Err(e) => error = Some(e.to_string()),
        }
        break;
    }
    manager.running.lock().unwrap().remove(&conversation_id);

    if let Some(e) = &error {
        let _ = channel.send(ChatEvent::Error { message: e.clone() });
    }
    db.finish_message(
        &assistant_id,
        &content,
        (!reasoning.is_empty()).then_some(reasoning.as_str()),
        error.as_deref(),
        usage.as_ref(),
    )?;
    db.touch_conversation(&conversation_id)?;

    if let Some(derived) = derived_title
        && error.is_none()
        && !cancelled
        && !content.is_empty()
        && crate::prefs::load(&db).generate_titles
    {
        tokio::spawn(async move {
            match generate_title(&resolved, &text, &content).await {
                Ok(Some(title)) => {
                    // Keep a title the user typed while the reply was streaming.
                    let still_derived = db
                        .get_conversation(&conversation_id)
                        .ok()
                        .flatten()
                        .is_some_and(|c| c.title == derived);
                    if still_derived
                        && db.update_conversation(&conversation_id, Some(&title), None, None).is_ok()
                    {
                        let _ = channel.send(ChatEvent::Title { title });
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("title generation failed: {e:#}"),
            }
        });
    }
    Ok(())
}

pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// One non-streaming-style completion: collects the visible text of a single reply.
pub(crate) async fn complete_text(resolved: &providers::Resolved, mut request: Request) -> Result<String> {
    providers::prepare_request(resolved, &mut request);
    let client = Client::new(providers::llm_config(resolved));
    client
        .complete_text(&request, &CancellationToken::new())
        .await
        .map_err(|e| anyhow!(e.to_string()))
}

const TITLE_PROMPT: &str = "You name chat conversations. Reply with a short title for the conversation \
below: at most 6 words, no quotes, no trailing punctuation. Reply with the title only.";

pub(crate) async fn generate_title(
    resolved: &providers::Resolved,
    user_text: &str,
    reply: &str,
) -> Result<Option<String>> {
    let convo = format!(
        "User: {}\n\nAssistant: {}",
        clip(user_text, 2000),
        clip(reply, 2000)
    );
    name_with(resolved, TITLE_PROMPT, convo).await
}

/// One model call that names `convo` as `system_prompt` asks; `None` if the reply has no usable title.
pub(crate) async fn name_with(
    resolved: &providers::Resolved,
    system_prompt: &str,
    convo: String,
) -> Result<Option<String>> {
    let raw = complete_text(resolved, Request::simple(system_prompt, convo)).await?;
    Ok(clean_title(&raw))
}

/// First non-empty line, without markdown, quotes or a trailing period; `None` if nothing usable.
fn clean_title(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line.replace(['*', '`'], "");
    let line = line.trim();
    let line = line.strip_prefix("Title:").unwrap_or(line).trim();
    let t = line
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '#' | '“' | '”'))
        .trim()
        .trim_end_matches(['.', ':'])
        .trim();
    if t.is_empty() {
        return None;
    }
    let mut out: String = t.chars().take(60).collect();
    if t.chars().count() > 60 {
        out.push('…');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{clean_title, derive_title};

    #[test]
    fn cleans_generated_titles() {
        assert_eq!(clean_title("\n\"Rust lifetimes explained.\"\n").as_deref(), Some("Rust lifetimes explained"));
        assert_eq!(clean_title("Title: **Fixing CORS**").as_deref(), Some("Fixing CORS"));
        assert_eq!(clean_title("  \n \"\" "), None);
        assert!(clean_title(&"word ".repeat(30)).unwrap().ends_with('…'));
    }

    #[test]
    fn title_from_first_line() {
        assert_eq!(derive_title("\n  hello world\nmore", &[]), "hello world");
        assert_eq!(derive_title("", &["report.pdf".into()]), "report.pdf");
        assert!(derive_title(&"x".repeat(80), &[]).ends_with('…'));
    }
}
