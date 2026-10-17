//! Chat tab: plain streaming chat over Grok Build's sampler (`xai-grok-sampler`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use serde::Serialize;
use tauri::ipc::Channel;
use tokio_util::sync::CancellationToken;
use xai_grok_sampler::{RequestId, RetryPolicy, SamplerActor, SamplingChannel, SamplingEvent};
use xai_grok_sampling_types::{ContentPart, ConversationItem, ConversationRequest};

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

/// `native_pdf`: send PDFs as documents (Anthropic Messages reads them natively) instead of
/// our text extraction.
fn user_item(data_dir: &std::path::Path, db: &Db, m: &Message, native_pdf: bool) -> Result<ConversationItem> {
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
        parts.push(ContentPart::Text {
            text: Arc::from(m.content.as_str()),
        });
    }
    Ok(ConversationItem::user_with_parts(parts))
}

/// The model's view of the branch ending at `leaf_id` (a user message).
fn build_history(
    data_dir: &std::path::Path,
    db: &Db,
    conversation_id: &str,
    leaf_id: &str,
    system_prompt: Option<&str>,
    native_pdf: bool,
) -> Result<Vec<ConversationItem>> {
    let mut items = Vec::new();
    if let Some(sp) = system_prompt.filter(|s| !s.trim().is_empty()) {
        items.push(ConversationItem::system(sp));
    }
    for m in db.path_to(conversation_id, leaf_id)? {
        match m.role.as_str() {
            "user" => items.push(user_item(data_dir, db, &m, native_pdf)?),
            "assistant" if !m.content.is_empty() => {
                items.push(ConversationItem::assistant(m.content.clone()))
            }
            _ => {}
        }
    }
    Ok(items)
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
    channel: Channel<ChatEvent>,
) -> Result<()> {
    let conv = db
        .get_conversation(&conversation_id)?
        .ok_or_else(|| anyhow!("conversation not found"))?;
    let mut resolved = providers::resolve(&db, &model_ref)?;
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

    let history = {
        let db = db.clone();
        let data_dir = data_dir.clone();
        let cid = conversation_id.clone();
        let sp = conv.system_prompt.clone();
        let leaf = user_id.clone();
        let native_pdf = resolved.provider.kind == crate::db::ProviderKind::Anthropic;
        tokio::task::spawn_blocking(move || build_history(&data_dir, &db, &cid, &leaf, sp.as_deref(), native_pdf))
            .await??
    };

    let token = CancellationToken::new();
    manager
        .running
        .lock()
        .unwrap()
        .insert(conversation_id.clone(), token.clone());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        providers::sampler_config(&resolved),
        RetryPolicy::default(),
        tx,
    );
    let request_id = RequestId::random();
    handle.submit(
        request_id.clone(),
        ConversationRequest {
            items: history,
            ..Default::default()
        },
    );

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut error: Option<String> = None;
    let mut usage: Option<Tokens> = None;
    let mut cancelled = false;
    loop {
        tokio::select! {
            _ = token.cancelled() => {
                cancelled = true;
                handle.cancel(request_id.clone());
                let _ = channel.send(ChatEvent::Done { stop_reason: Some("cancelled".into()), usage: None });
                break;
            }
            ev = rx.recv() => {
                let Some(ev) = ev else {
                    error = Some("sampler stopped unexpectedly".into());
                    break;
                };
                match ev {
                    SamplingEvent::ChannelToken { channel: ch, text, .. } => match ch {
                        SamplingChannel::Reasoning => {
                            reasoning.push_str(&text);
                            let _ = channel.send(ChatEvent::Reasoning { text });
                        }
                        _ => {
                            content.push_str(&text);
                            let _ = channel.send(ChatEvent::Text { text });
                        }
                    },
                    SamplingEvent::Retrying { attempt, max_retries, reason, .. } => {
                        let _ = channel.send(ChatEvent::Status {
                            message: format!("Retrying ({attempt}/{max_retries}): {reason}"),
                        });
                    }
                    SamplingEvent::ImagesStripped { stripped_urls, .. } => {
                        let _ = channel.send(ChatEvent::Status {
                            message: format!("{} image(s) were rejected by the endpoint and removed", stripped_urls.len()),
                        });
                    }
                    SamplingEvent::Completed { response, .. } => {
                        if content.is_empty()
                            && let Some(a) = response.assistant()
                        {
                            content = a.content.to_string();
                            if !content.is_empty() {
                                let _ = channel.send(ChatEvent::Text { text: content.clone() });
                            }
                        }
                        usage = response
                            .usage
                            .as_ref()
                            .map(|u| Tokens::from_sampler(u, response.cost_usd_ticks));
                        let _ = channel.send(ChatEvent::Done {
                            stop_reason: response.stop_reason.as_ref().map(|r| format!("{r:?}")),
                            usage,
                        });
                        break;
                    }
                    SamplingEvent::Failed { error: e, .. } => {
                        let msg = match e.status_code {
                            Some(code) => format!("{} (HTTP {code})", e.message),
                            None => e.message,
                        };
                        error = Some(msg);
                        break;
                    }
                    _ => {}
                }
            }
        }
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

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// One non-streaming-style completion: collects the visible text of a single reply.
pub(crate) async fn complete_text(
    resolved: &providers::Resolved,
    items: Vec<ConversationItem>,
) -> Result<String> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(providers::sampler_config(resolved), RetryPolicy::default(), tx);
    handle.submit(
        RequestId::random(),
        ConversationRequest {
            items,
            ..Default::default()
        },
    );
    let mut out = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            SamplingEvent::ChannelToken { channel: SamplingChannel::Reasoning, .. } => {}
            SamplingEvent::ChannelToken { text, .. } => out.push_str(&text),
            SamplingEvent::Completed { response, .. } => {
                if out.is_empty()
                    && let Some(a) = response.assistant()
                {
                    out = a.content.to_string();
                }
                return Ok(out);
            }
            SamplingEvent::Failed { error, .. } => return Err(anyhow!(error.message)),
            _ => {}
        }
    }
    Err(anyhow!("sampler stopped unexpectedly"))
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
    let raw = complete_text(
        resolved,
        vec![ConversationItem::system(TITLE_PROMPT), ConversationItem::user(convo)],
    )
    .await?;
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

#[cfg(test)]
mod wire_tests {
    use xai_grok_sampling_types::types::ChatCompletionChunk;

    /// Some OpenAI-compatible hosts (e.g. Fireworks) send explicit `null` for numeric fields.
    #[test]
    fn chunk_with_null_numbers_parses() {
        let raw = r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m",
            "choices":[{"index":0,"delta":{"tool_calls":[{"index":null,"id":"c","type":"function",
            "function":{"name":"f","arguments":"{}"}}]},"finish_reason":null}],
            "usage":{"prompt_tokens":10,"completion_tokens":null,"total_tokens":10,
            "prompt_tokens_details":{"cached_tokens":null},
            "completion_tokens_details":{"reasoning_tokens":null}}}"#;
        let c: ChatCompletionChunk = serde_json::from_str(raw).expect("parse");
        let u = c.usage.unwrap();
        assert_eq!(u.completion_tokens, 0);
        assert_eq!(u.prompt_tokens_details.unwrap().cached_tokens, 0);
        assert_eq!(c.choices[0].delta.tool_calls[0].index, 0);
    }
}
