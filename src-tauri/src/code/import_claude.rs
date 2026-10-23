//! Import Claude Code sessions (`<config dir>/projects/<folder>/<id>.jsonl`) as Code sessions.
//!
//! The file is a tree of JSON lines (`parentUuid` links a line to the one before it; a rewind
//! leaves dead branches behind). The import follows the chain from the last message back to the
//! start, turns it into Patchbay's transcript entries and tool calls, and the new session carries
//! it as earlier history. The agent that continues the session gets the conversation as context
//! with its first prompt (see `handoff::import_prompt`), as after a switch of agents.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};

/// How much of a file's start and end `list` reads to name the session.
const SCAN_BYTES: u64 = 192 * 1024;
/// Longest tool output kept per tool call.
const RESULT_CAP: usize = 8_000;
/// Longest text kept per side of an edit.
const DIFF_CAP: usize = 20_000;
/// A saved transcript past this size loses tool payloads, as the UI does when it saves one.
const TRANSCRIPT_CAP: usize = 3_000_000;

/// One Claude Code session on disk, as the import dialog lists it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSessionInfo {
    /// Claude Code's session id (the file name).
    pub id: String,
    pub cwd: String,
    /// Whether `cwd` still exists on this computer.
    pub cwd_exists: bool,
    pub title: String,
    /// The first thing the user asked.
    pub preview: String,
    /// Last write, in Unix milliseconds.
    pub modified: i64,
    pub size: u64,
    /// The Code session an earlier import made, while it still exists.
    pub imported: Option<String>,
}

/// A session converted for Patchbay.
#[derive(Debug)]
pub struct Converted {
    pub cwd: String,
    pub title: String,
    pub created_ms: i64,
    pub updated_ms: i64,
    /// Transcript JSON as the UI saves it: everything under `earlier`.
    pub transcript: String,
    /// How many user prompts and replies came across.
    pub turns: usize,
}

pub fn projects_dir() -> Result<PathBuf> {
    Ok(crate::claude_auth::config_dir()?.join("projects"))
}

/// Every session file under `projects`, newest first, without the ones with nothing to show.
pub fn list(projects: &Path) -> Vec<ClaudeSessionInfo> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(projects) else {
        return out;
    };
    for dir in dirs.flatten().filter(|d| d.path().is_dir()) {
        let Ok(files) = std::fs::read_dir(dir.path()) else { continue };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(info) = summarize(&path) {
                out.push(info);
            }
        }
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

/// The named parts of a file's start and end; `None` when it holds no prompt.
fn summarize(path: &Path) -> Option<ClaudeSessionInfo> {
    let id = path.file_stem()?.to_str()?.to_owned();
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    let mut file = std::fs::File::open(path).ok()?;
    let head = read_chunk(&mut file, 0, SCAN_BYTES.min(meta.len()), false)?;
    let tail = if meta.len() > SCAN_BYTES {
        read_chunk(&mut file, meta.len() - SCAN_BYTES, SCAN_BYTES, true).unwrap_or_default()
    } else {
        String::new()
    };

    let (mut cwd, mut preview) = (String::new(), String::new());
    let (mut custom, mut ai) = (None::<String>, None::<String>);
    for (n, chunk) in [&head, &tail].into_iter().enumerate() {
        for line in chunk.lines() {
            let wanted = line.contains("-title\"") || (n == 0 && line.contains("\"type\":\"user\""));
            if !wanted {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            match v["type"].as_str() {
                Some("custom-title") => custom = v["customTitle"].as_str().map(str::to_owned),
                Some("ai-title") => ai = v["aiTitle"].as_str().map(str::to_owned),
                Some("user") if n == 0 && !v["isSidechain"].as_bool().unwrap_or(false) => {
                    if cwd.is_empty() {
                        cwd = v["cwd"].as_str().unwrap_or_default().to_owned();
                    }
                    if preview.is_empty() && !v["isMeta"].as_bool().unwrap_or(false) {
                        let text = user_text(&v["message"]["content"]);
                        if classify(&text) == Role::User {
                            preview = one_line(&text, 200);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if preview.is_empty() {
        return None;
    }
    let title = custom.or(ai).filter(|t| !t.trim().is_empty()).unwrap_or_else(|| one_line(&preview, 80));
    Some(ClaudeSessionInfo {
        id,
        cwd_exists: !cwd.is_empty() && Path::new(&cwd).is_dir(),
        cwd,
        title,
        preview,
        modified,
        size: meta.len(),
        imported: None,
    })
}

/// `len` bytes of the file from `start`, whole lines only (a cut one at either end is dropped).
fn read_chunk(file: &mut std::fs::File, start: u64, len: u64, cut_front: bool) -> Option<String> {
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.take(len).read_to_end(&mut buf).ok()?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if cut_front {
        text = text.split_once('\n').map(|(_, rest)| rest.to_owned()).unwrap_or_default();
    }
    if !text.ends_with('\n') {
        match text.rfind('\n') {
            // The last line is cut off (unless the file ends there, which `lines` still reads).
            Some(i) if start + len < file.metadata().ok()?.len() => text.truncate(i + 1),
            _ => {}
        }
    }
    Some(text)
}

#[derive(Debug, PartialEq, Eq)]
enum Role {
    User,
    /// A reminder, command output or notification the harness wrote as the user.
    Harness,
    /// The user stopped the turn.
    Cancelled,
    Empty,
}

const HARNESS_TAGS: [&str; 7] = [
    "<command-name>",
    "<command-message>",
    "<local-command",
    "<system-reminder>",
    "<task-notification>",
    "<artifact-view-context",
    "<ide_",
];

fn classify(text: &str) -> Role {
    let t = text.trim_start();
    if t.is_empty() {
        Role::Empty
    } else if t.starts_with("[Request interrupted") {
        Role::Cancelled
    } else if HARNESS_TAGS.iter().any(|tag| t.starts_with(tag)) {
        Role::Harness
    } else {
        Role::User
    }
}

/// The text of a user message's content: a string, or the text blocks of an array.
fn user_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn one_line(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    clip(&s, max)
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

fn parse_time(v: &Value) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(v["timestamp"].as_str()?).ok().map(|t| t.timestamp_millis())
}

/// Read one session file and convert the conversation on its main branch.
pub fn convert(path: &Path) -> Result<Converted> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let lines: Vec<Value> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let chain = main_branch(&lines);

    let mut entries: Vec<Value> = Vec::new();
    let mut calls = serde_json::Map::new();
    let (mut cwd, mut custom, mut ai) = (String::new(), None::<String>, None::<String>);
    let (mut created, mut updated) = (0i64, 0i64);
    let mut turns = 0usize;

    for l in &lines {
        match l["type"].as_str() {
            Some("custom-title") => custom = l["customTitle"].as_str().map(str::to_owned),
            Some("ai-title") => ai = l["aiTitle"].as_str().map(str::to_owned),
            _ => {}
        }
    }
    for &i in &chain {
        let l = &lines[i];
        if let Some(t) = parse_time(l) {
            if created == 0 {
                created = t;
            }
            updated = t;
        }
        if cwd.is_empty() {
            cwd = l["cwd"].as_str().unwrap_or_default().to_owned();
        }
        match l["type"].as_str() {
            Some("user") => {
                let content = &l["message"]["content"];
                if let Value::Array(blocks) = content {
                    for b in blocks {
                        match b["type"].as_str() {
                            Some("tool_result") => apply_result(&mut calls, b),
                            Some("image") => entries.push(json!({ "kind": "info", "text": "[An image was attached; it did not come across.]" })),
                            _ => {}
                        }
                    }
                }
                let text = user_text(content);
                let role = if l["isCompactSummary"].as_bool().unwrap_or(false) || l["isMeta"].as_bool().unwrap_or(false) {
                    if text.trim().is_empty() { Role::Empty } else { Role::Harness }
                } else {
                    classify(&text)
                };
                match role {
                    Role::User => {
                        turns += 1;
                        entries.push(json!({ "kind": "user", "text": text.trim_end() }));
                    }
                    Role::Harness => entries.push(json!({ "kind": "harness", "text": text.trim() })),
                    Role::Cancelled => entries.push(json!({ "kind": "cancelled" })),
                    Role::Empty => {}
                }
            }
            Some("assistant") => {
                let Some(blocks) = l["message"]["content"].as_array() else { continue };
                for b in blocks {
                    match b["type"].as_str() {
                        Some("text") => {
                            let t = b["text"].as_str().unwrap_or_default().trim();
                            if !t.is_empty() {
                                turns += 1;
                                push_text(&mut entries, "agent", t);
                            }
                        }
                        Some("thinking") => {
                            let t = b["thinking"].as_str().unwrap_or_default().trim();
                            if !t.is_empty() {
                                push_text(&mut entries, "thought", t);
                            }
                        }
                        Some("tool_use") => {
                            let id = b["id"].as_str().unwrap_or_default();
                            if id.is_empty() {
                                continue;
                            }
                            entries.push(json!({ "kind": "tool", "id": id }));
                            calls.insert(id.to_owned(), tool_call(id, b["name"].as_str().unwrap_or("tool"), &b["input"]));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let first_prompt = entries.iter().find(|e| e["kind"] == "user").and_then(|e| e["text"].as_str()).unwrap_or_default();
    let title = custom
        .or(ai)
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| one_line(first_prompt, 80));
    entries.push(json!({
        "kind": "info",
        "text": "Imported from Claude Code. The agent gets this conversation as context with your first message."
    }));

    let mut transcript = transcript_json(&entries, &calls, false);
    if transcript.len() > TRANSCRIPT_CAP {
        transcript = transcript_json(&entries, &calls, true);
    }
    Ok(Converted { cwd, title, created_ms: created, updated_ms: updated, transcript, turns })
}

fn transcript_json(entries: &[Value], calls: &serde_json::Map<String, Value>, slim: bool) -> String {
    let calls = if slim {
        calls
            .iter()
            .map(|(k, v)| {
                let mut v = v.clone();
                v["content"] = Value::Null;
                v["rawInput"] = Value::Null;
                v["terminalOutput"] = Value::Null;
                (k.clone(), v)
            })
            .collect()
    } else {
        calls.clone()
    };
    json!({ "v": 1, "earlier": { "entries": entries, "toolCalls": calls }, "entries": [], "toolCalls": {} }).to_string()
}

/// Consecutive text of one kind reads as one block, as the agent streamed it.
fn push_text(entries: &mut Vec<Value>, kind: &str, text: &str) {
    if let Some(last) = entries.last_mut()
        && last["kind"] == kind
    {
        let joined = format!("{}\n\n{text}", last["text"].as_str().unwrap_or_default());
        last["text"] = Value::String(joined);
        return;
    }
    entries.push(json!({ "kind": kind, "text": text }));
}

/// Indexes of the lines on the branch that ends at the last message, oldest first. A rewind or a
/// retry leaves other branches in the file; a compaction restarts the chain but names the line it
/// follows (`logicalParentUuid`).
fn main_branch(lines: &[Value]) -> Vec<usize> {
    let by_uuid: HashMap<&str, usize> =
        lines.iter().enumerate().filter_map(|(i, l)| Some((l["uuid"].as_str()?, i))).collect();
    let is_message = |l: &Value| matches!(l["type"].as_str(), Some("user" | "assistant")) && !l["isSidechain"].as_bool().unwrap_or(false);
    let Some(leaf) = lines.iter().rposition(is_message) else { return Vec::new() };
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut cur = Some(leaf);
    while let Some(i) = cur {
        if !seen.insert(i) {
            break;
        }
        chain.push(i);
        let l = &lines[i];
        let parent = l["parentUuid"].as_str().or_else(|| l["logicalParentUuid"].as_str());
        cur = parent.and_then(|p| by_uuid.get(p).copied());
    }
    chain.reverse();
    // Files without a usable chain (older versions): take the messages in file order.
    if chain.len() == 1 && lines.iter().filter(|l| is_message(l)).count() > 1 {
        return lines.iter().enumerate().filter(|(_, l)| is_message(l)).map(|(i, _)| i).collect();
    }
    chain.into_iter().filter(|&i| matches!(lines[i]["type"].as_str(), Some("user" | "assistant")) && !lines[i]["isSidechain"].as_bool().unwrap_or(false)).collect()
}

/// Fill in a tool call with the result Claude Code recorded for it.
fn apply_result(calls: &mut serde_json::Map<String, Value>, block: &Value) {
    let Some(call) = block["tool_use_id"].as_str().and_then(|id| calls.get_mut(id)) else { return };
    let text = match &block["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    let text = clip_tail(&text, RESULT_CAP);
    call["status"] = json!(if block["is_error"].as_bool().unwrap_or(false) { "failed" } else { "completed" });
    if text.trim().is_empty() {
        return;
    }
    if call["kind"] == "execute" {
        call["terminalOutput"] = json!(text);
    } else {
        let mut content = call["content"].as_array().cloned().unwrap_or_default();
        content.push(json!({ "type": "content", "content": { "type": "text", "text": text } }));
        call["content"] = Value::Array(content);
    }
}

/// `s` cut to about `max` bytes, with a note that the rest is gone.
fn clip_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [{} more bytes not imported]", &s[..cut], s.len() - cut)
}

/// A Claude Code tool use as an ACP tool call, with Patchbay's kinds, titles and diffs.
fn tool_call(id: &str, name: &str, input: &Value) -> Value {
    let s = |k: &str| input[k].as_str().unwrap_or_default();
    let path = s("file_path");
    let diff = |old: Option<&str>, new: &str| {
        json!({ "type": "diff", "path": path, "oldText": old.map(|o| clip_tail(o, DIFF_CAP)), "newText": clip_tail(new, DIFF_CAP) })
    };
    let (kind, title, content): (&str, String, Vec<Value>) = match name {
        "Read" => ("read", format!("Read {path}"), vec![]),
        "Edit" => ("edit", format!("Edit {path}"), vec![diff(Some(s("old_string")), s("new_string"))]),
        "MultiEdit" => (
            "edit",
            format!("Edit {path}"),
            input["edits"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| diff(e["old_string"].as_str(), e["new_string"].as_str().unwrap_or_default()))
                .collect(),
        ),
        "Write" => ("edit", format!("Write {path}"), vec![diff(None, s("content"))]),
        "NotebookEdit" => ("edit", format!("Edit {}", s("notebook_path")), vec![]),
        "Bash" => ("execute", one_line(s("command"), 200), vec![]),
        "Grep" => ("search", format!("Grep {}", s("pattern")), vec![]),
        "Glob" => ("search", format!("Glob {}", s("pattern")), vec![]),
        "WebFetch" => ("fetch", format!("Fetch {}", s("url")), vec![]),
        "WebSearch" => ("fetch", format!("Search {}", s("query")), vec![]),
        "Task" | "Agent" => ("other", one_line(&format!("Agent: {}", s("description")), 200), vec![]),
        "TodoWrite" => ("other", "Update todos".to_owned(), vec![]),
        other => match other.strip_prefix("mcp__").and_then(|r| r.split_once("__")) {
            Some((server, tool)) => ("other", format!("{server}: {tool}"), vec![]),
            None => ("other", other.to_owned(), vec![]),
        },
    };
    let mut call = json!({
        "toolCallId": id,
        "title": title,
        "kind": kind,
        "status": "completed",
        "content": if content.is_empty() { Value::Null } else { Value::Array(content) },
        // An edit's input is its diff; keeping both would double the size.
        "rawInput": if kind == "edit" { json!({ "file_path": path }) } else { input.clone() },
    });
    if !path.is_empty() {
        call["locations"] = json!([{ "path": path }]);
    }
    call
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jsonl(lines: &[Value]) -> String {
        lines.iter().map(|l| l.to_string() + "\n").collect()
    }

    fn fixture() -> String {
        jsonl(&[
            json!({ "type": "custom-title", "customTitle": "Fix it", "sessionId": "s1" }),
            json!({ "type": "user", "uuid": "u1", "parentUuid": null, "cwd": "/work/proj", "timestamp": "2026-01-01T00:00:00Z",
                    "message": { "role": "user", "content": "Please fix the bug" } }),
            json!({ "type": "assistant", "uuid": "a1", "parentUuid": "u1", "timestamp": "2026-01-01T00:00:01Z",
                    "message": { "role": "assistant", "content": [
                        { "type": "thinking", "thinking": "hmm", "signature": "x" },
                        { "type": "text", "text": "Looking." },
                        { "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "ls -la" } } ] } }),
            json!({ "type": "user", "uuid": "u2", "parentUuid": "a1", "timestamp": "2026-01-01T00:00:02Z",
                    "message": { "role": "user", "content": [ { "type": "tool_result", "tool_use_id": "t1", "content": "a.txt" } ] } }),
            // A dead branch from a rewind: the same parent, never continued.
            json!({ "type": "assistant", "uuid": "dead", "parentUuid": "u1", "message": { "role": "assistant", "content": [ { "type": "text", "text": "dead end" } ] } }),
            json!({ "type": "assistant", "uuid": "a2", "parentUuid": "u2", "timestamp": "2026-01-01T00:00:03Z",
                    "message": { "role": "assistant", "content": [
                        { "type": "tool_use", "id": "t2", "name": "Edit", "input": { "file_path": "/work/proj/a.txt", "old_string": "x", "new_string": "y" } },
                        { "type": "text", "text": "Done." } ] } }),
            json!({ "type": "user", "uuid": "u3", "parentUuid": "a2", "message": { "role": "user", "content": [
                        { "type": "tool_result", "tool_use_id": "t2", "content": [ { "type": "text", "text": "ok" } ], "is_error": true } ] } }),
            json!({ "type": "user", "uuid": "u4", "parentUuid": "u3", "isSidechain": false,
                    "message": { "role": "user", "content": "<system-reminder>be nice</system-reminder>" } }),
            json!({ "type": "user", "uuid": "u5", "parentUuid": "u4", "message": { "role": "user", "content": [ { "type": "text", "text": "[Request interrupted by user]" } ] } }),
        ])
    }

    #[test]
    fn converts_the_main_branch() {
        let dir = tempdir();
        let path = dir.join("s1.jsonl");
        std::fs::write(&path, fixture()).unwrap();
        let c = convert(&path).unwrap();
        assert_eq!(c.title, "Fix it");
        assert_eq!(c.cwd, "/work/proj");
        let v: Value = serde_json::from_str(&c.transcript).unwrap();
        let kinds: Vec<&str> = v["earlier"]["entries"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["user", "thought", "agent", "tool", "tool", "agent", "harness", "cancelled", "info"]);
        let calls = &v["earlier"]["toolCalls"];
        assert_eq!(calls["t1"]["kind"], "execute");
        assert_eq!(calls["t1"]["title"], "ls -la");
        assert_eq!(calls["t1"]["terminalOutput"], "a.txt");
        assert_eq!(calls["t2"]["status"], "failed");
        assert_eq!(calls["t2"]["content"][0]["type"], "diff");
        assert!(!c.transcript.contains("dead end"));
        assert_eq!(c.turns, 3);
        assert!(c.created_ms > 0 && c.updated_ms > c.created_ms);
    }

    #[test]
    fn lists_with_a_title_and_preview() {
        let root = tempdir();
        let proj = root.join("-work-proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("s1.jsonl"), fixture()).unwrap();
        // A session with no prompt is left out.
        std::fs::write(proj.join("s2.jsonl"), jsonl(&[json!({ "type": "mode", "mode": "normal" })])).unwrap();
        let found = list(&root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "s1");
        assert_eq!(found[0].title, "Fix it");
        assert_eq!(found[0].preview, "Please fix the bug");
        assert_eq!(found[0].cwd, "/work/proj");
        assert!(!found[0].cwd_exists);
    }

    #[test]
    fn follows_a_compaction() {
        let lines = [
            json!({ "type": "user", "uuid": "u1", "parentUuid": null, "message": { "content": "one" } }),
            json!({ "type": "system", "uuid": "b", "parentUuid": null, "logicalParentUuid": "u1", "subtype": "compact_boundary" }),
            json!({ "type": "user", "uuid": "u2", "parentUuid": "b", "message": { "content": "two" } }),
        ];
        assert_eq!(main_branch(&lines), [0, 2]);
    }

    /// Against the real `~/.claude/projects`: `cargo test --lib real_sessions -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_sessions() {
        let t = std::time::Instant::now();
        let all = list(&projects_dir().unwrap());
        eprintln!("{} sessions listed in {:?}", all.len(), t.elapsed());
        let biggest = all.iter().max_by_key(|s| s.size).unwrap();
        let path = projects_dir().unwrap().join(std::fs::read_dir(projects_dir().unwrap()).unwrap().flatten()
            .map(|d| d.path()).find(|d| d.join(format!("{}.jsonl", biggest.id)).is_file()).unwrap()).join(format!("{}.jsonl", biggest.id));
        let t = std::time::Instant::now();
        let c = convert(&path).unwrap();
        eprintln!("{} ({} MB) -> {} KB, {} turns, {:?}, title {:?}", biggest.id, biggest.size / 1_000_000, c.transcript.len() / 1000, c.turns, t.elapsed(), c.title);
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("pb-import-{}", crate::db::new_id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
