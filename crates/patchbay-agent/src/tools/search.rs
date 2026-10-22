//! Glob and Grep, over a gitignore-aware walk.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use globset::{Glob as GlobPattern, GlobSetBuilder};
use ignore::WalkBuilder;
use patchbay_llm::ToolSpec;
use regex::RegexBuilder;
use serde_json::{Value, json};

use super::*;
use crate::session::Session;

const GLOB_LIMIT: usize = 100;
const GREP_DEFAULT_LIMIT: usize = 250;
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

fn walker(root: &Path) -> WalkBuilder {
    let mut b = WalkBuilder::new(root);
    // Dotfiles are code too; the VCS directory is not.
    b.hidden(false).filter_entry(|e| e.file_name() != ".git");
    b
}

fn search_root(input: &Value, session: &Session) -> PathBuf {
    str_arg(input, "path").filter(|p| !p.is_empty()).map(|p| session.resolve(p)).unwrap_or_else(|| session.cwd.clone())
}

fn path_locations(input: &Value, session: &Session) -> Vec<String> {
    vec![search_root(input, session).to_string_lossy().into_owned()]
}

// --- Glob ---

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Glob".into(),
            description: "Find files by name pattern (for example `**/*.rs` or `src/**/test_*.py`). Respects \
.gitignore. Results are sorted by modification time, newest first, and cut at 100. For open-ended \
searches that need several rounds, search step by step yourself."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "The glob pattern"},
                    "path": {"type": "string", "description": "Directory to search; defaults to the working directory"}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "search"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn title(&self, input: &Value) -> String {
        format!("Glob {}", str_arg(input, "pattern").unwrap_or(""))
    }

    fn locations(&self, input: &Value, session: &Session) -> Vec<String> {
        path_locations(input, session)
    }

    fn needs_permission(&self, input: &Value, session: &Session) -> bool {
        !inside(&session.cwd, &search_root(input, session))
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(pattern) = str_arg(&input, "pattern") else { return ToolOutput::error("pattern is required") };
        let root = search_root(&input, &ctx.session);
        if !root.is_dir() {
            return ToolOutput::error(format!("Directory does not exist: {}", root.display()));
        }
        let cwd = ctx.session.cwd.clone();
        let pattern = pattern.to_owned();
        let found = tokio::task::spawn_blocking(move || glob(&root, &cwd, &pattern)).await;
        match found {
            Ok(Ok((files, truncated))) if files.is_empty() => {
                let _ = truncated;
                ToolOutput::text("No files found")
            }
            Ok(Ok((files, truncated))) => {
                let mut out = files.join("\n");
                if truncated {
                    out.push_str(&format!("\n(Results are truncated to the first {GLOB_LIMIT}. Use a more specific pattern or path.)"));
                }
                ToolOutput::text(out)
            }
            Ok(Err(e)) => ToolOutput::error(e),
            Err(e) => ToolOutput::error(format!("search failed: {e}")),
        }
    }
}

fn glob(root: &Path, cwd: &Path, pattern: &str) -> Result<(Vec<String>, bool), String> {
    // A pattern with a leading slash names a path under the search root.
    let pattern = pattern.trim_start_matches("./");
    let set = GlobSetBuilder::new()
        .add(GlobPattern::new(pattern).map_err(|e| format!("bad pattern: {e}"))?)
        .build()
        .map_err(|e| format!("bad pattern: {e}"))?;
    let mut hits: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in walker(root).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if set.is_match(rel) || (!pattern.contains('/') && entry.path().file_name().is_some_and(|n| set.is_match(n))) {
            let mtime = entry.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
            hits.push((mtime, entry.path().to_path_buf()));
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let truncated = hits.len() > GLOB_LIMIT;
    Ok((hits.into_iter().take(GLOB_LIMIT).map(|(_, p)| display_path(cwd, &p)).collect(), truncated))
}

// --- Grep ---

pub struct Grep;

fn type_extensions(t: &str) -> Option<&'static [&'static str]> {
    Some(match t {
        "rust" | "rs" => &["rs"],
        "py" | "python" => &["py", "pyi"],
        "js" => &["js", "mjs", "cjs", "jsx"],
        "ts" => &["ts", "tsx", "mts", "cts"],
        "go" => &["go"],
        "java" => &["java"],
        "c" => &["c", "h"],
        "cpp" => &["cc", "cpp", "cxx", "hpp", "hh", "h"],
        "cs" => &["cs"],
        "rb" | "ruby" => &["rb"],
        "php" => &["php"],
        "swift" => &["swift"],
        "kotlin" => &["kt", "kts"],
        "md" | "markdown" => &["md", "markdown"],
        "json" => &["json"],
        "yaml" | "yml" => &["yaml", "yml"],
        "toml" => &["toml"],
        "html" => &["html", "htm"],
        "css" => &["css", "scss", "sass"],
        "sh" | "shell" => &["sh", "bash", "zsh"],
        "sql" => &["sql"],
        _ => return None,
    })
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Mode {
    Files,
    Content,
    Count,
}

struct GrepArgs {
    re: regex::Regex,
    root: PathBuf,
    cwd: PathBuf,
    glob: Option<globset::GlobSet>,
    exts: Option<&'static [&'static str]>,
    mode: Mode,
    before: usize,
    after: usize,
    numbers: bool,
    limit: usize,
    offset: usize,
}

fn grep_args(input: &Value, session: &Session) -> Result<GrepArgs, String> {
    let pattern = str_arg(input, "pattern").ok_or("pattern is required")?;
    let multiline = bool_arg(input, "multiline");
    let re = RegexBuilder::new(pattern)
        .case_insensitive(bool_arg(input, "-i"))
        .multi_line(true)
        .dot_matches_new_line(multiline)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))?;
    let glob = match str_arg(input, "glob").filter(|g| !g.is_empty()) {
        Some(g) => {
            let mut b = GlobSetBuilder::new();
            for part in g.split_whitespace() {
                b.add(GlobPattern::new(part).map_err(|e| format!("bad glob: {e}"))?);
            }
            Some(b.build().map_err(|e| format!("bad glob: {e}"))?)
        }
        None => None,
    };
    let exts = match str_arg(input, "type") {
        Some(t) => Some(type_extensions(t).ok_or_else(|| format!("unknown file type: {t}"))?),
        None => None,
    };
    let mode = match str_arg(input, "output_mode").unwrap_or("files_with_matches") {
        "content" => Mode::Content,
        "count" => Mode::Count,
        _ => Mode::Files,
    };
    let context = u64_arg(input, "-C").or_else(|| u64_arg(input, "context")).map(|n| n as usize);
    Ok(GrepArgs {
        re,
        root: search_root(input, session),
        cwd: session.cwd.clone(),
        glob,
        exts,
        mode,
        before: u64_arg(input, "-B").map(|n| n as usize).or(context).unwrap_or(0),
        after: u64_arg(input, "-A").map(|n| n as usize).or(context).unwrap_or(0),
        numbers: input.get("-n").and_then(Value::as_bool).unwrap_or(true),
        limit: u64_arg(input, "head_limit").map(|n| n as usize).unwrap_or(GREP_DEFAULT_LIMIT),
        offset: u64_arg(input, "offset").map(|n| n as usize).unwrap_or(0),
    })
}

fn grep(a: &GrepArgs) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut files: Vec<(SystemTime, String)> = Vec::new();
    let files_walk: Vec<PathBuf> = if a.root.is_file() {
        vec![a.root.clone()]
    } else {
        walker(&a.root)
            .build()
            .flatten()
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(|e| e.into_path())
            .collect()
    };
    for path in files_walk {
        let rel = path.strip_prefix(&a.root).unwrap_or(&path);
        if let Some(g) = &a.glob
            && !(g.is_match(rel) || path.file_name().is_some_and(|n| g.is_match(n)))
        {
            continue;
        }
        if let Some(exts) = a.exts
            && !path.extension().and_then(|e| e.to_str()).is_some_and(|e| exts.contains(&e.to_ascii_lowercase().as_str()))
        {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        if bytes.iter().take(8000).any(|b| *b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let shown = display_path(&a.cwd, &path);
        match a.mode {
            Mode::Files => {
                if a.re.is_match(&text) {
                    files.push((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), shown));
                }
            }
            Mode::Count => {
                let n = a.re.find_iter(&text).count();
                if n > 0 {
                    lines.push(format!("{shown}:{n}"));
                }
            }
            Mode::Content => content_matches(a, &text, &shown, &mut lines),
        }
    }
    if a.mode == Mode::Files {
        files.sort_by(|x, y| y.0.cmp(&x.0).then_with(|| x.1.cmp(&y.1)));
        lines = files.into_iter().map(|(_, p)| p).collect();
    }
    if lines.is_empty() {
        return "No matches found".into();
    }
    let total = lines.len();
    let page: Vec<String> = if a.limit == 0 {
        lines.into_iter().skip(a.offset).collect()
    } else {
        lines.into_iter().skip(a.offset).take(a.limit).collect()
    };
    let mut out = page.join("\n");
    if a.limit != 0 && total > a.offset + a.limit {
        out.push_str(&format!("\n\n[Showing results {}-{} of {total}. Use offset to see more.]", a.offset + 1, a.offset + a.limit));
    }
    out
}

fn content_matches(a: &GrepArgs, text: &str, shown: &str, out: &mut Vec<String>) {
    let all: Vec<&str> = text.lines().collect();
    // Line numbers of lines that hold a match (a multiline match marks each line it spans).
    let mut hit = vec![false; all.len()];
    let starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    for m in a.re.find_iter(text) {
        let first = starts.partition_point(|s| *s <= m.start()) - 1;
        let last = starts.partition_point(|s| *s < m.end().max(m.start() + 1)).saturating_sub(1).max(first);
        for h in hit.iter_mut().take(last + 1).skip(first) {
            *h = true;
        }
    }
    let mut last_printed: Option<usize> = None;
    for i in 0..all.len() {
        if !hit[i] {
            continue;
        }
        let from = i.saturating_sub(a.before);
        let to = (i + a.after).min(all.len() - 1);
        for j in from..=to {
            if last_printed.is_some_and(|l| j <= l) {
                continue;
            }
            if (a.before > 0 || a.after > 0) && last_printed.is_some_and(|l| j > l + 1) {
                out.push("--".into());
            }
            let sep = if hit[j] { ':' } else { '-' };
            out.push(if a.numbers {
                format!("{shown}{sep}{}{sep}{}", j + 1, all[j])
            } else {
                format!("{shown}{sep}{}", all[j])
            });
            last_printed = Some(j);
        }
    }
}

#[async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Grep".into(),
            description: "Search file contents with a regular expression (Rust regex syntax: escape literal \
braces and parentheses). Respects .gitignore. `output_mode` is `files_with_matches` (default), `content` \
(matching lines; use -n, -A, -B, -C) or `count`. Narrow with `glob` (for example \"*.ts\") or `type` \
(for example \"rust\"). `multiline` lets a pattern span lines. Use this instead of grep or rg in Bash."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression to search for"},
                    "path": {"type": "string", "description": "File or directory to search; defaults to the working directory"},
                    "glob": {"type": "string", "description": "Only search files matching this glob"},
                    "type": {"type": "string", "description": "Only search this file type (rust, py, js, ts, go, …)"},
                    "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"]},
                    "-i": {"type": "boolean", "description": "Case-insensitive"},
                    "-n": {"type": "boolean", "description": "Show line numbers (content mode; default true)"},
                    "-A": {"type": "integer", "description": "Lines of context after each match"},
                    "-B": {"type": "integer", "description": "Lines of context before each match"},
                    "-C": {"type": "integer", "description": "Lines of context around each match"},
                    "multiline": {"type": "boolean", "description": "Let patterns span lines and `.` match newlines"},
                    "head_limit": {"type": "integer", "description": "Show at most this many result lines (default 250, 0 = all)"},
                    "offset": {"type": "integer", "description": "Skip this many result lines first"}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "search"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn title(&self, input: &Value) -> String {
        format!("Grep {}", str_arg(input, "pattern").unwrap_or(""))
    }

    fn locations(&self, input: &Value, session: &Session) -> Vec<String> {
        path_locations(input, session)
    }

    fn needs_permission(&self, input: &Value, session: &Session) -> bool {
        !inside(&session.cwd, &search_root(input, session))
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let args = match grep_args(&input, &ctx.session) {
            Ok(a) => a,
            Err(e) => return ToolOutput::error(e),
        };
        if !args.root.exists() {
            return ToolOutput::error(format!("Path does not exist: {}", args.root.display()));
        }
        match tokio::task::spawn_blocking(move || grep(&args)).await {
            Ok(out) => ToolOutput::text(out),
            Err(e) => ToolOutput::error(format!("search failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pb-search-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n// TODO one\n").unwrap();
        std::fs::write(dir.join("src/b.py"), "def gamma():\n    pass\n# TODO two\n").unwrap();
        std::fs::write(dir.join("notes.md"), "todo lower\n").unwrap();
        dir
    }

    fn args(root: &Path, mode: Mode) -> GrepArgs {
        GrepArgs {
            re: regex::Regex::new("TODO").unwrap(),
            root: root.to_path_buf(),
            cwd: root.to_path_buf(),
            glob: None,
            exts: None,
            mode,
            before: 0,
            after: 0,
            numbers: true,
            limit: 250,
            offset: 0,
        }
    }

    #[test]
    fn globs_by_extension_anywhere() {
        let dir = tree();
        let (files, _) = glob(&dir, &dir, "*.rs").unwrap();
        assert_eq!(files, vec!["src/a.rs"]);
        let (files, _) = glob(&dir, &dir, "src/**/*.py").unwrap();
        assert_eq!(files, vec!["src/b.py"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn greps_in_three_modes() {
        let dir = tree();
        let files = grep(&args(&dir, Mode::Files));
        assert!(files.contains("src/a.rs") && files.contains("src/b.py") && !files.contains("notes.md"), "{files}");
        let content = grep(&args(&dir, Mode::Content));
        assert!(content.contains("src/a.rs:3:// TODO one"), "{content}");
        let count = grep(&args(&dir, Mode::Count));
        assert!(count.contains("src/b.py:1"), "{count}");
        let mut a = args(&dir, Mode::Content);
        a.after = 1;
        a.before = 1;
        a.re = regex::Regex::new("beta").unwrap();
        let ctx = grep(&a);
        assert_eq!(ctx, "src/a.rs-1-fn alpha() {}\nsrc/a.rs:2:fn beta() {}\nsrc/a.rs-3-// TODO one");
        a.re = regex::Regex::new("nothing-here").unwrap();
        assert_eq!(grep(&a), "No matches found");
        let _ = std::fs::remove_dir_all(dir);
    }
}
