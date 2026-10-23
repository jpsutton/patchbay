//! The user's Claude Code setup: where it lives and what it holds.
//!
//! Claude Code reads `~/.claude` (the user's), `.claude/` in the project, and installed plugins.
//! Patchbay's agent loads the same instructions, slash commands, skills, subagent definitions,
//! MCP servers and hooks from there, each behind a switch in [`ClaudeConfig`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::host::ClaudeConfig;

/// `~/.claude`, or `PATCHBAY_CLAUDE_HOME` (tests point it at a temporary directory).
pub fn home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("PATCHBAY_CLAUDE_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(h));
    }
    let base = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(base).join(".claude"))
}

/// `~/.claude.json`, beside the `.claude` directory.
fn user_json(home: &Path) -> PathBuf {
    home.parent().map_or_else(|| PathBuf::from(".claude.json"), |p| p.join(".claude.json"))
}

/// The working directory and its parents up to the repository root, nearest first.
pub fn project_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in cwd.ancestors() {
        out.push(dir.to_path_buf());
        if dir.join(".git").exists() {
            return out;
        }
    }
    // Not in a repository: only the working directory itself counts as the project.
    out.truncate(1);
    out
}

/// Where one part of the setup comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Project,
    User,
    /// An installed plugin, by name.
    Plugin(String),
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::Project => "project".into(),
            Source::User => "user".into(),
            Source::Plugin(p) => format!("plugin {p}"),
        }
    }
}

/// A directory laid out like `.claude/`: `commands/`, `skills/`, `agents/`.
#[derive(Debug, Clone)]
pub struct Root {
    pub dir: PathBuf,
    pub source: Source,
}

/// An installed Claude Code plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct Plugin {
    /// `name@marketplace`, as `installed_plugins.json` and `enabledPlugins` key it.
    pub id: String,
    /// The name its commands and skills are prefixed with.
    pub name: String,
    pub path: PathBuf,
    /// Claude Code's own `enabledPlugins` setting.
    pub enabled: bool,
}

/// Installed plugins, from `~/.claude/plugins/installed_plugins.json` (version 1 or 2).
pub fn plugins(home: &Path) -> Vec<Plugin> {
    let installed = read_json(&home.join("plugins/installed_plugins.json"));
    let enabled = &read_json(&home.join("settings.json"))["enabledPlugins"];
    let mut out = Vec::new();
    for (id, v) in installed["plugins"].as_object().into_iter().flatten() {
        // Version 2 lists installs per scope; version 1 has one object.
        let entry = v.as_array().and_then(|a| a.iter().find(|e| e["scope"] == "user").or(a.first())).unwrap_or(v);
        let Some(path) = entry["installPath"].as_str().map(PathBuf::from).filter(|p| p.is_dir()) else { continue };
        let name = read_json(&path.join(".claude-plugin/plugin.json"))["name"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| id.split('@').next().unwrap_or(id).to_owned());
        // Not listed in `enabledPlugins`: Claude Code treats an installed plugin as on.
        let on = enabled.get(id).and_then(Value::as_bool).unwrap_or(true);
        out.push(Plugin { id: id.clone(), name, path, enabled: on });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// The directories to search, highest precedence first: the project's (nearest first), the
/// user's, then enabled plugins.
pub fn roots(home: Option<&Path>, cwd: &Path, cfg: &ClaudeConfig) -> Vec<Root> {
    let mut out: Vec<Root> = project_dirs(cwd)
        .into_iter()
        .map(|d| d.join(".claude"))
        .filter(|d| d.is_dir() && Some(d.as_path()) != home)
        .map(|dir| Root { dir, source: Source::Project })
        .collect();
    if let Some(h) = home.filter(|h| h.is_dir()) {
        out.push(Root { dir: h.to_path_buf(), source: Source::User });
        if cfg.plugins {
            for p in plugins(h).into_iter().filter(|p| p.enabled && !cfg.disabled_plugins.contains(&p.id)) {
                out.push(Root { dir: p.path, source: Source::Plugin(p.name) });
            }
        }
    }
    out
}

pub(crate) fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
}

// --- frontmatter ---

/// Split `---` frontmatter from a Markdown file: the fields (a small YAML subset: scalars,
/// quoted strings, `>`/`|` blocks, `[a, b]` and `- a` lists) and the body.
pub fn frontmatter(text: &str) -> (BTreeMap<String, Value>, &str) {
    let mut fields = BTreeMap::new();
    let Some(rest) = text.strip_prefix("---").and_then(|r| r.strip_prefix('\n').or_else(|| r.strip_prefix("\r\n"))) else {
        return (fields, text);
    };
    let Some(end) = rest.find("\n---") else { return (fields, text) };
    let head = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['-']).trim_start_matches(['\r', '\n']);
    let lines: Vec<&str> = head.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.starts_with([' ', '\t', '#']) || line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let (key, value) = (key.trim().to_owned(), value.trim());
        // Indented lines that follow belong to this key.
        let mut block = Vec::new();
        while i < lines.len() && (lines[i].starts_with([' ', '\t']) || lines[i].trim().is_empty()) {
            block.push(lines[i].trim());
            i += 1;
        }
        let v = if value.starts_with('>') || value.starts_with('|') {
            let sep = if value.starts_with('>') { " " } else { "\n" };
            json!(block.iter().filter(|l| !l.is_empty()).copied().collect::<Vec<_>>().join(sep))
        } else if value.is_empty() && block.iter().any(|l| l.starts_with("- ")) {
            json!(block.iter().filter_map(|l| l.strip_prefix("- ")).map(unquote).collect::<Vec<_>>())
        } else if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            json!(inner.split(',').map(unquote).filter(|s| !s.is_empty()).collect::<Vec<_>>())
        } else {
            json!(unquote(value))
        };
        fields.insert(key, v);
    }
    (fields, body)
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return inner.to_owned();
        }
    }
    s.to_owned()
}

/// A frontmatter field as text.
fn field<'a>(fields: &'a BTreeMap<String, Value>, key: &str) -> Option<&'a str> {
    fields.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// A frontmatter field that may be a list or a comma-separated string.
fn list_field(fields: &BTreeMap<String, Value>, key: &str) -> Option<Vec<String>> {
    match fields.get(key)? {
        Value::Array(a) => Some(a.iter().filter_map(Value::as_str).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect()),
        Value::String(s) => Some(s.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect()),
        _ => None,
    }
}

/// The first line of text that isn't a heading marker, for a description.
fn first_line(body: &str) -> String {
    body.lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

// --- instructions ---

/// Longest instruction file kept, in bytes.
const MAX_INSTRUCTIONS: usize = 40_000;

fn read_capped(path: &Path) -> Option<String> {
    let mut text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    if text.len() > MAX_INSTRUCTIONS {
        let mut cut = MAX_INSTRUCTIONS;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n…[truncated]");
    }
    Some(text)
}

/// Instruction files, broadest first: `AGENTS.md` always; with `instructions` on, also Claude
/// Code's `~/.claude/CLAUDE.md`, its rules, and per directory `CLAUDE.md`, `.claude/CLAUDE.md`,
/// `CLAUDE.local.md` and `.claude/rules/*.md`. A file reached twice counts once.
pub fn instructions(home: Option<&Path>, cwd: &Path, cfg: &ClaudeConfig) -> Vec<(PathBuf, String)> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if cfg.instructions
        && let Some(h) = home
    {
        paths.push(h.join("CLAUDE.md"));
        paths.extend(rules(&h.join("rules")));
    }
    // Outermost directory first, so the nearest file has the last word.
    for dir in project_dirs(cwd).into_iter().rev() {
        paths.push(dir.join("AGENTS.md"));
        if cfg.instructions {
            paths.push(dir.join("CLAUDE.md"));
            paths.push(dir.join(".claude/CLAUDE.md"));
            paths.push(dir.join("CLAUDE.local.md"));
            if Some(dir.join(".claude").as_path()) != home {
                paths.extend(rules(&dir.join(".claude/rules")));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter(|p| p.is_file() && seen.insert(p.canonicalize().unwrap_or_else(|_| p.clone())))
        .filter_map(|p| read_capped(&p).map(|t| (p, t)))
        .collect()
}

/// `*.md` files under a rules directory, sorted.
fn rules(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_md(dir, &mut out);
    out.sort();
    out
}

fn walk_md(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk_md(&p, out);
        } else if p.extension().is_some_and(|x| x == "md") {
            out.push(p);
        }
    }
}

// --- slash commands, skills and subagents ---

/// A slash command from a `commands/` directory.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// Without the slash: `review`, `frontend:test`, `plugin:cmd`.
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub body: String,
    pub source: Source,
}

impl Command {
    /// The body with `$ARGUMENTS` and `$1`…`$9` filled in. Arguments the body doesn't use are
    /// appended, so they're never lost.
    pub fn expand(&self, args: &str) -> String {
        let args = args.trim();
        let words: Vec<&str> = args.split_whitespace().collect();
        let mut out = self.body.replace("$ARGUMENTS", args);
        let mut used = out != self.body;
        for i in (1..=9).rev() {
            let key = format!("${i}");
            if out.contains(&key) {
                used = true;
                out = out.replace(&key, words.get(i - 1).copied().unwrap_or(""));
            }
        }
        if !used && !args.is_empty() {
            out.push_str("\n\nArguments: ");
            out.push_str(args);
        }
        out
    }
}

/// A skill: a `skills/<name>/SKILL.md` file and the directory it sits in.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub body: String,
    pub source: Source,
}

/// A subagent definition from an `agents/` directory.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// Tool names it may use; `None` = every tool a subagent gets.
    pub tools: Option<Vec<String>>,
    /// The model it asks for, as written (`sonnet`, a model id); used when the host has it.
    pub model: Option<String>,
    /// Its system prompt.
    pub prompt: String,
    pub source: Source,
}

/// Plugin parts carry the plugin's name: `plugin:name`.
fn qualified(source: &Source, name: &str) -> String {
    match source {
        Source::Plugin(p) => format!("{p}:{name}"),
        _ => name.to_owned(),
    }
}

/// Slash commands in `roots`, the first of each name winning.
pub fn commands(roots: &[Root]) -> Vec<Command> {
    let mut out: Vec<Command> = Vec::new();
    for root in roots {
        let dir = root.dir.join("commands");
        let mut files = Vec::new();
        walk_md(&dir, &mut files);
        files.sort();
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else { continue };
            let rel = f.strip_prefix(&dir).unwrap_or(&f).with_extension("");
            let local = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join(":");
            let name = qualified(&root.source, &local);
            if out.iter().any(|c| c.name == name) {
                continue;
            }
            let (fields, body) = frontmatter(&text);
            out.push(Command {
                description: field(&fields, "description").map_or_else(|| first_line(body), str::to_owned),
                argument_hint: field(&fields, "argument-hint").map(str::to_owned),
                body: body.trim().to_owned(),
                source: root.source.clone(),
                name,
            });
        }
    }
    out
}

/// Skills in `roots`, the first of each name winning.
pub fn skills(roots: &[Root]) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root.dir.join("skills")) else { continue };
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.join("SKILL.md").is_file()).collect();
        dirs.sort();
        for dir in dirs {
            let Ok(text) = std::fs::read_to_string(dir.join("SKILL.md")) else { continue };
            let (fields, body) = frontmatter(&text);
            let local = field(&fields, "name").map_or_else(|| dir.file_name().unwrap_or_default().to_string_lossy().into_owned(), str::to_owned);
            let name = qualified(&root.source, &local);
            if out.iter().any(|s| s.name == name) {
                continue;
            }
            out.push(Skill {
                description: field(&fields, "description").map_or_else(|| first_line(body), str::to_owned),
                body: body.trim().to_owned(),
                source: root.source.clone(),
                dir,
                name,
            });
        }
    }
    out
}

/// Subagent definitions in `roots`, the first of each name winning. A file without a name and
/// description isn't one.
pub fn agents(roots: &[Root]) -> Vec<AgentDef> {
    let mut out: Vec<AgentDef> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root.dir.join("agents")) else { continue };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "md")).collect();
        files.sort();
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else { continue };
            let (fields, body) = frontmatter(&text);
            let (Some(local), Some(description)) = (field(&fields, "name"), field(&fields, "description")) else { continue };
            let name = qualified(&root.source, local);
            if out.iter().any(|a| a.name == name) || body.trim().is_empty() {
                continue;
            }
            out.push(AgentDef {
                description: description.to_owned(),
                tools: list_field(&fields, "tools").filter(|t| !t.is_empty() && t.iter().all(|x| x != "*")),
                model: field(&fields, "model").filter(|m| *m != "inherit").map(str::to_owned),
                prompt: body.trim().to_owned(),
                source: root.source.clone(),
                name,
            });
        }
    }
    out
}

// --- MCP servers ---

/// An MCP server from Claude's configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct McpEntry {
    pub name: String,
    pub source: Source,
    /// stdio, http or sse.
    pub transport: String,
    /// The server in the shape ACP's `mcpServers` (and `McpSet::connect`) take.
    pub config: Value,
}

/// `${VAR}` and `${VAR:-default}` from the environment, as Claude Code expands them in MCP
/// configuration; `vars` are set first (a plugin's `CLAUDE_PLUGIN_ROOT`).
fn expand(s: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &rest[start + 2..start + end];
        let (name, default) = inner.split_once(":-").map_or((inner, None), |(n, d)| (n, Some(d)));
        let value = vars
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| (*v).to_owned())
            .or_else(|| std::env::var(name).ok())
            .or(default.map(str::to_owned))
            .unwrap_or_default();
        out.push_str(&value);
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

/// A `{name: value}` object as ACP's `[{name, value}]`, expanded.
fn name_values(v: &Value, vars: &[(&str, &str)]) -> Value {
    json!(
        v.as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some(json!({"name": k, "value": expand(v.as_str()?, vars)})))
            .collect::<Vec<_>>()
    )
}

/// One server from Claude's format (`{command, args, env}` or `{type, url, headers}`) in ACP's.
fn convert_server(name: &str, s: &Value, cwd: &Path, vars: &[(&str, &str)]) -> Option<(String, Value)> {
    let ty = s["type"].as_str().unwrap_or(if s.get("url").is_some() { "http" } else { "stdio" });
    match ty {
        "http" | "streamable-http" | "sse" => {
            let url = expand(s["url"].as_str()?, vars);
            let transport = if ty == "sse" { "sse" } else { "http" };
            Some((transport.to_owned(), json!({"type": transport, "name": name, "url": url, "headers": name_values(&s["headers"], vars)})))
        }
        "stdio" => {
            let command = expand(s["command"].as_str()?, vars);
            let args: Vec<String> = s["args"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|a| expand(a, vars)).collect();
            Some((
                "stdio".to_owned(),
                json!({"name": name, "command": command, "args": args, "env": name_values(&s["env"], vars), "cwd": cwd}),
            ))
        }
        _ => None,
    }
}

/// MCP servers from Claude's configuration, the first of each name winning: the project's
/// local servers in `~/.claude.json`, its `.mcp.json`, the user's servers, then plugins'.
/// Lists every server found; the caller drops the ones turned off.
pub fn mcp_servers(home: Option<&Path>, cwd: &Path, roots: &[Root]) -> Vec<McpEntry> {
    let mut found: Vec<(Source, Value, Option<PathBuf>)> = Vec::new();
    let user = home.map(|h| read_json(&user_json(h))).unwrap_or(Value::Null);
    for dir in project_dirs(cwd) {
        found.push((Source::Project, user["projects"][dir.to_string_lossy().as_ref()]["mcpServers"].clone(), None));
    }
    for dir in project_dirs(cwd) {
        found.push((Source::Project, read_json(&dir.join(".mcp.json"))["mcpServers"].clone(), None));
    }
    found.push((Source::User, user["mcpServers"].clone(), None));
    for root in roots.iter().filter(|r| matches!(r.source, Source::Plugin(_))) {
        let file = read_json(&root.dir.join(".mcp.json"));
        // A plugin's `.mcp.json` may or may not wrap its servers in `mcpServers`.
        let servers = if file["mcpServers"].is_object() { file["mcpServers"].clone() } else { file };
        found.push((root.source.clone(), servers, Some(root.dir.clone())));
        let manifest = &read_json(&root.dir.join(".claude-plugin/plugin.json"))["mcpServers"];
        let inline = match manifest {
            Value::String(rel) => {
                let f = read_json(&root.dir.join(rel));
                if f["mcpServers"].is_object() { f["mcpServers"].clone() } else { f }
            }
            v => v.clone(),
        };
        found.push((root.source.clone(), inline, Some(root.dir.clone())));
    }
    let mut out: Vec<McpEntry> = Vec::new();
    for (source, servers, plugin_root) in found {
        let root_str = plugin_root.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let vars: Vec<(&str, &str)> = if plugin_root.is_some() { vec![("CLAUDE_PLUGIN_ROOT", root_str.as_str())] } else { Vec::new() };
        for (name, s) in servers.as_object().into_iter().flatten() {
            if out.iter().any(|e| &e.name == name) {
                continue;
            }
            if let Some((transport, config)) = convert_server(name, s, cwd, &vars) {
                out.push(McpEntry { name: name.clone(), source: source.clone(), transport, config });
            }
        }
    }
    out
}

// --- hooks ---

/// One command hook from Claude's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Hook {
    /// PreToolUse, PostToolUse, UserPromptSubmit, Stop, SessionStart, …
    pub event: String,
    /// Tool-name pattern for tool events; empty or `*` matches every tool.
    pub matcher: String,
    pub command: String,
    pub timeout_secs: u64,
    /// The plugin's directory, set as `CLAUDE_PLUGIN_ROOT` for its hooks.
    pub plugin_root: Option<PathBuf>,
    pub source: Source,
}

impl Hook {
    /// Whether the hook applies to `tool` (always, for events without a tool).
    pub fn matches(&self, tool: Option<&str>) -> bool {
        let m = self.matcher.trim();
        let Some(tool) = tool else { return true };
        if m.is_empty() || m == "*" {
            return true;
        }
        regex::Regex::new(&format!("^(?:{m})$")).map_or(m == tool, |re| re.is_match(tool))
    }
}

/// Hooks from `{event: [{matcher, hooks: [{type, command, timeout}]}]}`.
fn parse_hooks(v: &Value, source: &Source, plugin_root: Option<&Path>, out: &mut Vec<Hook>) {
    for (event, groups) in v.as_object().into_iter().flatten() {
        for g in groups.as_array().into_iter().flatten() {
            let matcher = g["matcher"].as_str().unwrap_or("").to_owned();
            for h in g["hooks"].as_array().into_iter().flatten() {
                // Prompt-based hooks ask a model; only command hooks run here.
                if h["type"].as_str().unwrap_or("command") != "command" {
                    continue;
                }
                let Some(command) = h["command"].as_str().filter(|c| !c.trim().is_empty()) else { continue };
                out.push(Hook {
                    event: event.clone(),
                    matcher: matcher.clone(),
                    command: command.to_owned(),
                    timeout_secs: h["timeout"].as_u64().filter(|t| *t > 0).unwrap_or(60),
                    plugin_root: plugin_root.map(Path::to_path_buf),
                    source: source.clone(),
                });
            }
        }
    }
}

/// Command hooks from the user's and the project's settings and from plugins. None when a
/// settings file sets `disableAllHooks`.
pub fn hooks(home: Option<&Path>, cwd: &Path, roots: &[Root]) -> Vec<Hook> {
    let mut files: Vec<(Source, PathBuf)> = Vec::new();
    if let Some(h) = home {
        files.push((Source::User, h.join("settings.json")));
    }
    for dir in project_dirs(cwd) {
        let d = dir.join(".claude");
        if Some(d.as_path()) == home {
            continue;
        }
        files.push((Source::Project, d.join("settings.json")));
        files.push((Source::Project, d.join("settings.local.json")));
    }
    let mut out = Vec::new();
    for (source, f) in &files {
        let v = read_json(f);
        if v["disableAllHooks"] == true {
            return Vec::new();
        }
        parse_hooks(&v["hooks"], source, None, &mut out);
    }
    for root in roots.iter().filter(|r| matches!(r.source, Source::Plugin(_))) {
        let file = read_json(&root.dir.join("hooks/hooks.json"));
        let v = if file["hooks"].is_object() { &file["hooks"] } else { &file };
        parse_hooks(v, &root.source, Some(&root.dir), &mut out);
        match &read_json(&root.dir.join(".claude-plugin/plugin.json"))["hooks"] {
            Value::String(rel) if rel.trim_start_matches("./") != "hooks/hooks.json" => {
                let f = read_json(&root.dir.join(rel));
                parse_hooks(if f["hooks"].is_object() { &f["hooks"] } else { &f }, &root.source, Some(&root.dir), &mut out);
            }
            v @ Value::Object(_) => parse_hooks(v, &root.source, Some(&root.dir), &mut out),
            _ => {}
        }
    }
    out
}

// --- everything at once ---

/// The parts of the setup a session uses, as its switches allow.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    pub commands: Vec<Command>,
    pub skills: Vec<Skill>,
    pub agents: Vec<AgentDef>,
    pub hooks: Vec<Hook>,
    /// Servers to start, in ACP's shape.
    pub mcp: Vec<McpEntry>,
}

impl Setup {
    pub fn load(cfg: &ClaudeConfig, cwd: &Path) -> Setup {
        Setup::load_from(home().as_deref(), cfg, cwd)
    }

    pub fn load_from(home: Option<&Path>, cfg: &ClaudeConfig, cwd: &Path) -> Setup {
        let roots = roots(home, cwd, cfg);
        Setup {
            commands: if cfg.skills { commands(&roots) } else { Vec::new() },
            skills: if cfg.skills { skills(&roots) } else { Vec::new() },
            agents: if cfg.agents { agents(&roots) } else { Vec::new() },
            hooks: if cfg.hooks { hooks(home, cwd, &roots) } else { Vec::new() },
            mcp: if cfg.mcp_servers {
                mcp_servers(home, cwd, &roots).into_iter().filter(|e| !cfg.disabled_mcp_servers.contains(&e.name)).collect()
            } else {
                Vec::new()
            },
        }
    }

    pub fn has_hooks(&self, event: &str) -> bool {
        self.hooks.iter().any(|h| h.event == event)
    }

    /// The user's `/name args`, when `name` is a command or a skill: the text to send instead.
    pub fn expand_slash(&self, text: &str) -> Option<String> {
        let rest = text.trim_start().strip_prefix('/')?;
        let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        if let Some(c) = self.commands.iter().find(|c| c.name == name) {
            return Some(c.expand(args));
        }
        let s = self.skills.iter().find(|s| s.name == name)?;
        let mut out = skill_text(s);
        if !args.trim().is_empty() {
            out.push_str("\n\nARGUMENTS: ");
            out.push_str(args.trim());
        }
        Some(out)
    }
}

/// What the model gets when a skill is loaded.
pub fn skill_text(s: &Skill) -> String {
    format!("Skill \"{}\" (base directory: {}):\n\n{}", s.name, s.dir.display(), s.body)
}

// --- running hooks ---

/// What the hooks for one event decided.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookResult {
    /// Exit code 2, or `"decision": "block"`: the reason.
    pub block: Option<String>,
    /// PreToolUse's `permissionDecision`: allow, deny or ask (the strictest wins).
    pub permission: Option<String>,
    pub permission_reason: Option<String>,
    /// Text for the model (`additionalContext`, or a prompt hook's plain output).
    pub context: Vec<String>,
    /// `"continue": false`: stop the turn, with this reason.
    pub stop: Option<String>,
    /// Hooks that failed (shown, then ignored) and `systemMessage`s for the user.
    pub notes: Vec<String>,
}

/// Run the hooks for `event` (and `tool`, for tool events) side by side, each with `input` as
/// JSON on stdin, and merge what they decide.
pub async fn run_hooks(hooks: &[Hook], event: &str, tool: Option<&str>, input: Value, cwd: &Path) -> HookResult {
    let mut input = input;
    input["hook_event_name"] = json!(event);
    input["cwd"] = json!(cwd);
    let stdin = input.to_string();
    let runs = hooks.iter().filter(|h| h.event == event && h.matches(tool)).map(|h| run_one(h, &stdin, cwd));
    let mut out = HookResult::default();
    let rank = |p: &str| match p {
        "deny" => 3,
        "ask" => 2,
        "allow" => 1,
        _ => 0,
    };
    for (hook, r) in futures::future::join_all(runs).await {
        match r {
            Err(e) => out.notes.push(format!("Hook `{}` failed: {e}", short(&hook.command))),
            Ok((code, stdout, stderr)) => match code {
                Some(0) => {
                    let parsed: Option<Value> = serde_json::from_str(stdout.trim()).ok().filter(Value::is_object);
                    let Some(v) = parsed else {
                        if matches!(event, "UserPromptSubmit" | "SessionStart") && !stdout.trim().is_empty() {
                            out.context.push(stdout.trim().to_owned());
                        }
                        continue;
                    };
                    if v["continue"] == false {
                        out.stop = Some(v["stopReason"].as_str().unwrap_or("A hook stopped the turn.").to_owned());
                    }
                    if v["decision"] == "block" {
                        out.block = Some(v["reason"].as_str().unwrap_or("Blocked by a hook.").to_owned());
                    }
                    let specific = &v["hookSpecificOutput"];
                    // Older hooks put approve/block in `decision` for PreToolUse.
                    let decision = specific["permissionDecision"].as_str().or(match v["decision"].as_str() {
                        Some("approve") => Some("allow"),
                        _ => None,
                    });
                    if let Some(p) = decision
                        && rank(p) > out.permission.as_deref().map_or(0, rank)
                    {
                        out.permission = Some(p.to_owned());
                        out.permission_reason = specific["permissionDecisionReason"].as_str().or(v["reason"].as_str()).map(str::to_owned);
                    }
                    if let Some(c) = specific["additionalContext"].as_str().filter(|c| !c.trim().is_empty()) {
                        out.context.push(c.trim().to_owned());
                    }
                    if let Some(m) = v["systemMessage"].as_str().filter(|m| !m.trim().is_empty()) {
                        out.notes.push(m.trim().to_owned());
                    }
                }
                Some(2) => {
                    let reason = if stderr.trim().is_empty() { "Blocked by a hook." } else { stderr.trim() };
                    out.block = Some(reason.to_owned());
                }
                code => {
                    let why = code.map_or("killed".to_owned(), |c| format!("exit {c}"));
                    let detail = stderr.trim();
                    out.notes.push(format!("Hook `{}` failed ({why}){}{detail}", short(&hook.command), if detail.is_empty() { "" } else { ": " }));
                }
            },
        }
    }
    out
}

fn short(command: &str) -> String {
    let c: String = command.chars().take(80).collect();
    if c.len() < command.len() { format!("{c}…") } else { c }
}

/// Run one hook: exit code (`None` if killed), stdout, stderr.
async fn run_one<'a>(hook: &'a Hook, stdin: &str, cwd: &Path) -> (&'a Hook, Result<(Option<i32>, String, String), String>) {
    use tokio::io::AsyncWriteExt;
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(&hook.command);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(&hook.command);
        c
    };
    cmd.current_dir(cwd)
        .env("CLAUDE_PROJECT_DIR", project_dirs(cwd).last().unwrap_or(&cwd.to_path_buf()))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(root) = &hook.plugin_root {
        cmd.env("CLAUDE_PLUGIN_ROOT", root);
    }
    let r = async {
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        if let Some(mut si) = child.stdin.take() {
            // A hook that doesn't read its input may close stdin early; that's fine.
            let _ = si.write_all(stdin.as_bytes()).await;
        }
        let out = tokio::time::timeout(std::time::Duration::from_secs(hook.timeout_secs), child.wait_with_output())
            .await
            .map_err(|_| format!("timed out after {}s", hook.timeout_secs))?
            .map_err(|e| e.to_string())?;
        Ok((out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned()))
    }
    .await;
    (hook, r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!("pb-claude-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A home with a user setup and a plugin, and a repository with a project setup.
    fn fixture() -> (PathBuf, PathBuf, PathBuf) {
        let base = tmp();
        let home = base.join("home/.claude");
        let repo = base.join("repo");
        let cwd = repo.join("sub");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        put(&home.join("CLAUDE.md"), "user rules");
        put(&home.join("commands/hello.md"), "---\ndescription: Say hi\nargument-hint: <name>\n---\nGreet $1 and $ARGUMENTS");
        put(&home.join("skills/pdf/SKILL.md"), "---\nname: pdf\ndescription: >\n  Work with\n  PDF files\n---\n# PDF\nUse pdftotext.");
        put(&home.join("agents/reviewer.md"), "---\nname: reviewer\ndescription: Reviews code\ntools: Read, Grep\nmodel: inherit\n---\nYou review.");
        put(&home.join("agents/notes.md"), "no frontmatter, not an agent");
        let plugin = base.join("plugins/cave");
        put(&plugin.join(".claude-plugin/plugin.json"), r#"{"name": "cave", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "echo stop"}]}]}}"#);
        put(&plugin.join("commands/talk.md"), "Talk like caveman.");
        put(&plugin.join(".mcp.json"), r#"{"cavemcp": {"command": "${CLAUDE_PLUGIN_ROOT}/bin/srv"}}"#);
        put(
            &home.join("plugins/installed_plugins.json"),
            &json!({"version": 2, "plugins": {"cave@market": [{"scope": "user", "installPath": plugin}]}}).to_string(),
        );
        put(
            &home.join("settings.json"),
            r#"{"enabledPlugins": {"cave@market": true}, "hooks": {"PreToolUse": [{"matcher": "Edit|Write", "hooks": [{"type": "command", "command": "exit 0", "timeout": 5}]}]}}"#,
        );
        put(
            &base.join("home/.claude.json"),
            &json!({"mcpServers": {"web": {"type": "http", "url": "https://x/${PB_TEST_UNSET:-mcp}", "headers": {"A": "b"}}},
                    "projects": {cwd.to_string_lossy(): {"mcpServers": {"local": {"command": "srv", "args": ["-v"], "env": {"K": "v"}}}}}})
            .to_string(),
        );
        put(&repo.join("AGENTS.md"), "agents file");
        put(&repo.join("CLAUDE.md"), "project claude");
        put(&repo.join(".claude/rules/style.md"), "style rule");
        put(&repo.join(".claude/commands/frontend/test.md"), "Run the frontend tests");
        put(&repo.join(".claude/commands/hello.md"), "---\ndescription: Project hello\n---\nProject greeting");
        put(&repo.join(".mcp.json"), r#"{"mcpServers": {"web": {"command": "shadowed-by-nothing"}, "proj": {"type": "sse", "url": "https://s"}}}"#);
        put(&repo.join(".claude/settings.json"), r#"{"hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": "echo ctx"}]}]}}"#);
        (base, home, cwd)
    }

    #[test]
    fn frontmatter_handles_blocks_lists_and_quotes() {
        let (f, body) = frontmatter("---\nname: 'x'\ndescription: >\n  one\n  two\ntools:\n  - Read\n  - Grep\nother: [a, \"b\"]\n---\nBody\n");
        assert_eq!(f["name"], "x");
        assert_eq!(f["description"], "one two");
        assert_eq!(f["tools"], json!(["Read", "Grep"]));
        assert_eq!(f["other"], json!(["a", "b"]));
        assert_eq!(body, "Body\n");
        let (f, body) = frontmatter("no frontmatter");
        assert!(f.is_empty() && body == "no frontmatter");
    }

    #[test]
    fn instructions_follow_the_switch() {
        let (base, home, cwd) = fixture();
        let on = instructions(Some(&home), &cwd, &ClaudeConfig::default());
        let texts: Vec<&str> = on.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(texts, ["user rules", "agents file", "project claude", "style rule"]);
        let off = instructions(Some(&home), &cwd, &ClaudeConfig { instructions: false, ..Default::default() });
        assert_eq!(off.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(), ["agents file"]);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn commands_skills_and_agents_load_with_precedence() {
        let (base, home, cwd) = fixture();
        let setup = Setup::load_from(Some(&home), &ClaudeConfig::default(), &cwd);
        let names: Vec<&str> = setup.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["frontend:test", "hello", "cave:talk"]);
        // The project's command wins over the user's of the same name.
        assert_eq!(setup.commands[1].description, "Project hello");
        assert_eq!(setup.skills[0].description, "Work with PDF files");
        assert_eq!(setup.agents.len(), 1);
        assert_eq!(setup.agents[0].tools, Some(vec!["Read".to_owned(), "Grep".to_owned()]));
        assert_eq!(setup.agents[0].model, None);
        assert!(setup.expand_slash("/pdf now").unwrap().contains("Use pdftotext.") );
        assert_eq!(setup.expand_slash("/frontend:test"), Some("Run the frontend tests".into()));
        assert_eq!(setup.expand_slash("/nope"), None);

        let off = Setup::load_from(Some(&home), &ClaudeConfig { skills: false, agents: false, ..Default::default() }, &cwd);
        assert!(off.commands.is_empty() && off.skills.is_empty() && off.agents.is_empty());
        let no_plugin = Setup::load_from(Some(&home), &ClaudeConfig { disabled_plugins: vec!["cave@market".into()], ..Default::default() }, &cwd);
        assert!(!no_plugin.commands.iter().any(|c| c.name == "cave:talk"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn command_arguments_fill_in() {
        let c = Command { name: "x".into(), description: String::new(), argument_hint: None, body: "Greet $1 and $ARGUMENTS".into(), source: Source::User };
        assert_eq!(c.expand("Ann Bob"), "Greet Ann and Ann Bob");
        let plain = Command { body: "Do it".into(), ..c };
        assert_eq!(plain.expand("now"), "Do it\n\nArguments: now");
    }

    #[test]
    fn mcp_servers_convert_and_merge() {
        let (base, home, cwd) = fixture();
        let setup = Setup::load_from(Some(&home), &ClaudeConfig::default(), &cwd);
        let names: Vec<&str> = setup.mcp.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["local", "proj", "web", "cavemcp"]);
        let local = &setup.mcp[0].config;
        assert_eq!((local["command"].as_str(), local["env"][0]["name"].as_str()), (Some("srv"), Some("K")));
        assert_eq!(setup.mcp[1].transport, "sse");
        // The project's `.mcp.json` comes before the user's servers.
        assert_eq!(setup.mcp[2].transport, "stdio");
        assert!(setup.mcp[3].config["command"].as_str().unwrap().ends_with("plugins/cave/bin/srv"));
        let some = Setup::load_from(Some(&home), &ClaudeConfig { disabled_mcp_servers: vec!["web".into()], ..Default::default() }, &cwd);
        assert!(!some.mcp.iter().any(|e| e.name == "web"));
        assert_eq!(expand("a${PB_TEST_UNSET:-d}b", &[]), "adb");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn hooks_load_and_match() {
        let (base, home, cwd) = fixture();
        let setup = Setup::load_from(Some(&home), &ClaudeConfig::default(), &cwd);
        let events: Vec<&str> = setup.hooks.iter().map(|h| h.event.as_str()).collect();
        assert_eq!(events, ["PreToolUse", "UserPromptSubmit", "Stop"]);
        assert!(setup.hooks[0].matches(Some("Edit")) && !setup.hooks[0].matches(Some("Read")));
        assert!(setup.hooks[2].plugin_root.is_some());
        let off = Setup::load_from(Some(&home), &ClaudeConfig { hooks: false, ..Default::default() }, &cwd);
        assert!(off.hooks.is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hook_results_merge() {
        let dir = tmp();
        let hook = |event: &str, command: &str| Hook {
            event: event.into(),
            matcher: String::new(),
            command: command.into(),
            timeout_secs: 5,
            plugin_root: None,
            source: Source::User,
        };
        let hooks = vec![
            hook("PreToolUse", r#"echo '{"hookSpecificOutput": {"permissionDecision": "allow"}}'"#),
            hook("PreToolUse", r#"echo '{"hookSpecificOutput": {"permissionDecision": "deny", "permissionDecisionReason": "no"}}'"#),
            hook("UserPromptSubmit", "cat >/dev/null; echo extra context"),
            hook("Stop", "echo keep going >&2; exit 2"),
            hook("Stop", "exit 1"),
        ];
        let pre = run_hooks(&hooks, "PreToolUse", Some("Bash"), json!({"tool_name": "Bash"}), &dir).await;
        assert_eq!((pre.permission.as_deref(), pre.permission_reason.as_deref()), (Some("deny"), Some("no")));
        let prompt = run_hooks(&hooks, "UserPromptSubmit", None, json!({"prompt": "hi"}), &dir).await;
        assert_eq!(prompt.context, ["extra context"]);
        let stop = run_hooks(&hooks, "Stop", None, json!({}), &dir).await;
        assert_eq!(stop.block.as_deref(), Some("keep going"));
        assert_eq!(stop.notes.len(), 1, "{:?}", stop.notes);
        let slow = vec![Hook { timeout_secs: 1, ..hook("Stop", "sleep 5") }];
        assert!(run_hooks(&slow, "Stop", None, json!({}), &dir).await.notes[0].contains("timed out"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
