//! Read, Write and Edit.

use std::path::Path;

use async_trait::async_trait;
use base64::Engine;
use patchbay_llm::{Part, ToolSpec};
use serde_json::{Value, json};

use super::*;
use crate::session::{Session, stamp_of};

const MAX_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
const MAX_UNPAGED_BYTES: u64 = 1_000_000;
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

fn image_mime(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn diff_item(path: &Path, old: Option<&str>, new: &str) -> Value {
    json!({"type": "diff", "path": path.to_string_lossy(), "oldText": old, "newText": new})
}

/// An edit needs the file's current state to be the one the model saw.
fn check_fresh(session: &Session, path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let seen = session.files.lock().unwrap().get(path).copied();
    match (seen, stamp_of(path)) {
        (None, _) => Err("File has not been read yet. Read it first before writing to it.".into()),
        (Some(a), Some(b)) if a == b => Ok(()),
        _ => Err("File has been modified since it was read, either by the user or by a linter. Read it again before trying to change it.".into()),
    }
}

fn remember(session: &Session, path: &Path) {
    if let Some(s) = stamp_of(path) {
        session.files.lock().unwrap().insert(path.to_path_buf(), s);
    }
}

// --- Read ---

pub struct Read;

#[async_trait]
impl Tool for Read {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Read".into(),
            description: "Read a file from the local filesystem. Text files come back with line numbers; \
images are shown to you; PDFs are read as text (pass `pages` such as \"1-5\" for long ones); notebooks \
come back as cells. Give an absolute path. For a large file read only the part you need with `offset` \
(first line, 1-based) and `limit` (line count). Reading a directory is an error: use Glob or Bash `ls`."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "Absolute path of the file"},
                    "offset": {"type": "integer", "description": "Line to start at (1-based)"},
                    "limit": {"type": "integer", "description": "Number of lines to read"},
                    "pages": {"type": "string", "description": "Page range for PDFs, e.g. \"1-5\""}
                },
                "required": ["file_path"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "read"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn title(&self, input: &Value) -> String {
        format!("Read {}", str_arg(input, "file_path").unwrap_or("file"))
    }

    fn locations(&self, input: &Value, session: &Session) -> Vec<String> {
        str_arg(input, "file_path").map(|p| vec![session.resolve(p).to_string_lossy().into_owned()]).unwrap_or_default()
    }

    fn needs_permission(&self, input: &Value, session: &Session) -> bool {
        str_arg(input, "file_path").is_none_or(|p| !inside(&session.cwd, &session.resolve(p)))
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(file) = str_arg(&input, "file_path") else {
            return ToolOutput::error("file_path is required");
        };
        let session = &ctx.session;
        let path = session.resolve(file);
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolOutput::error(format!("File does not exist: {}", path.display()));
            }
            Err(e) => return ToolOutput::error(format!("Cannot read {}: {e}", path.display())),
        };
        if meta.is_dir() {
            return ToolOutput::error(format!("{} is a directory. Use Glob or Bash `ls` to list it.", path.display()));
        }
        if let Some(mime) = image_mime(&path) {
            if meta.len() > MAX_IMAGE_BYTES {
                return ToolOutput::error("Image is larger than 8 MB.");
            }
            return match std::fs::read(&path) {
                Ok(bytes) => {
                    remember(session, &path);
                    ToolOutput {
                        content: vec![
                            Part::Text(format!("Image {} ({} bytes):", path.display(), bytes.len())),
                            Part::Image { mime: mime.into(), data: base64::engine::general_purpose::STANDARD.encode(bytes) },
                        ],
                        ui: Vec::new(),
                        raw: Value::Null,
                        is_error: false,
                        meta: None,
                    }
                }
                Err(e) => ToolOutput::error(format!("Cannot read {}: {e}", path.display())),
            };
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if ext == "pdf" {
            return match session.host.read_pdf(&path, str_arg(&input, "pages")) {
                Ok(t) => {
                    remember(session, &path);
                    ToolOutput::text(t)
                }
                Err(e) => ToolOutput::error(e),
            };
        }
        let offset = u64_arg(&input, "offset");
        let limit = u64_arg(&input, "limit");
        if meta.len() > MAX_UNPAGED_BYTES && offset.is_none() && limit.is_none() {
            return ToolOutput::error(format!(
                "File is {} bytes, too large to read at once. Pass offset and limit to read part of it, or search it with Grep.",
                meta.len()
            ));
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return ToolOutput::error(format!("Cannot read {}: {e}", path.display())),
        };
        if bytes.iter().take(8000).any(|b| *b == 0) {
            return ToolOutput::error("This looks like a binary file, which cannot be shown as text.");
        }
        remember(session, &path);
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if ext == "ipynb" {
            text = notebook_text(&text);
        }
        ToolOutput::text(number_lines(&text, offset, limit))
    }
}

fn number_lines(text: &str, offset: Option<u64>, limit: Option<u64>) -> String {
    let total = text.lines().count();
    if text.is_empty() {
        return "<system-reminder>The file exists but is empty.</system-reminder>".into();
    }
    let start = offset.map(|o| o.max(1) as usize).unwrap_or(1);
    if start > total {
        return format!("<system-reminder>The file is shorter than the offset: it has {total} lines, the offset is {start}.</system-reminder>");
    }
    let take = limit.map(|l| l as usize).unwrap_or(MAX_LINES);
    let mut out = String::new();
    for (i, line) in text.lines().enumerate().skip(start - 1).take(take) {
        let line: String = if line.chars().count() > MAX_LINE_CHARS {
            let mut l: String = line.chars().take(MAX_LINE_CHARS).collect();
            l.push('…');
            l
        } else {
            line.to_owned()
        };
        out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
    }
    let shown_to = (start - 1 + take).min(total);
    if shown_to < total {
        out.push_str(&format!("\n[{} more lines; read on with offset {}]", total - shown_to, shown_to + 1));
    }
    out
}

fn notebook_text(raw: &str) -> String {
    let Ok(nb) = serde_json::from_str::<Value>(raw) else { return raw.to_owned() };
    let join = |v: &Value| match v {
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect::<String>(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    let mut out = String::new();
    for (i, cell) in nb["cells"].as_array().into_iter().flatten().enumerate() {
        out.push_str(&format!("# Cell {i} [{}]\n{}\n", cell["cell_type"].as_str().unwrap_or("?"), join(&cell["source"])));
        for o in cell["outputs"].as_array().into_iter().flatten() {
            let t = join(&o["text"]);
            let t = if t.is_empty() { join(&o["data"]["text/plain"]) } else { t };
            if !t.is_empty() {
                out.push_str(&format!("## Output\n{t}\n"));
            }
        }
        out.push('\n');
    }
    out
}

// --- Write ---

pub struct Write;

#[async_trait]
impl Tool for Write {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Write".into(),
            description: "Create a file or replace one completely. Overwriting a file you have not Read \
fails. Prefer Edit for partial changes. Do not create documentation files unless asked."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "Absolute path of the file"},
                    "content": {"type": "string", "description": "The full new content"}
                },
                "required": ["file_path", "content"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "edit"
    }

    fn title(&self, input: &Value) -> String {
        format!("Write {}", str_arg(input, "file_path").unwrap_or("file"))
    }

    fn locations(&self, input: &Value, session: &Session) -> Vec<String> {
        str_arg(input, "file_path").map(|p| vec![session.resolve(p).to_string_lossy().into_owned()]).unwrap_or_default()
    }

    fn needs_permission(&self, _: &Value, _: &Session) -> bool {
        true
    }

    async fn preview(&self, input: &Value, session: &Session) -> Vec<Value> {
        let (Some(p), Some(c)) = (str_arg(input, "file_path"), str_arg(input, "content")) else { return Vec::new() };
        let path = session.resolve(p);
        let old = std::fs::read_to_string(&path).ok();
        vec![diff_item(&path, old.as_deref(), c)]
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let (Some(file), Some(content)) = (str_arg(&input, "file_path"), str_arg(&input, "content")) else {
            return ToolOutput::error("file_path and content are required");
        };
        let session = &ctx.session;
        let path = session.resolve(file);
        if let Err(e) = check_fresh(session, &path) {
            return ToolOutput::error(e);
        }
        let old = std::fs::read_to_string(&path).ok();
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            return ToolOutput::error(format!("Cannot create {}: {e}", dir.display()));
        }
        if let Err(e) = std::fs::write(&path, content) {
            return ToolOutput::error(format!("Cannot write {}: {e}", path.display()));
        }
        remember(session, &path);
        let verb = if old.is_some() { "updated" } else { "created" };
        ToolOutput::text(format!("The file {} has been {verb} successfully.", path.display()))
            .with_ui(vec![diff_item(&path, old.as_deref(), content)])
    }
}

// --- Edit ---

pub struct Edit;

struct Planned {
    path: std::path::PathBuf,
    old_file: Option<String>,
    new_file: String,
    replaced: usize,
}

fn plan_edit(input: &Value, session: &Session) -> Result<Planned, String> {
    let (Some(file), Some(old), Some(new)) =
        (str_arg(input, "file_path"), str_arg(input, "old_string"), str_arg(input, "new_string"))
    else {
        return Err("file_path, old_string and new_string are required".into());
    };
    let path = session.resolve(file);
    if old == new {
        return Err("No changes to make: old_string and new_string are exactly the same.".into());
    }
    let existing = match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("Cannot read {}: {e}", path.display())),
    };
    let Some(text) = existing else {
        if old.is_empty() {
            return Ok(Planned { path, old_file: None, new_file: new.to_owned(), replaced: 1 });
        }
        return Err(format!("File does not exist: {}", path.display()));
    };
    check_fresh(session, &path)?;
    if old.is_empty() {
        if text.trim().is_empty() {
            return Ok(Planned { path, old_file: Some(text), new_file: new.to_owned(), replaced: 1 });
        }
        return Err("Cannot create a new file: the file already exists.".into());
    }
    let n = text.matches(old).count();
    if n == 0 {
        return Err(format!("String to replace not found in file.\nString: {old}"));
    }
    let all = bool_arg(input, "replace_all");
    if n > 1 && !all {
        return Err(format!(
            "Found {n} matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one, provide more surrounding context so it is unique.\nString: {old}"
        ));
    }
    let new_file = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
    Ok(Planned { path, old_file: Some(text), new_file, replaced: if all { n } else { 1 } })
}

#[async_trait]
impl Tool for Edit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Edit".into(),
            description: "Replace text in a file. `old_string` must match the file exactly (indentation \
included, without the line-number prefix Read adds) and be unique unless `replace_all` is true. Read the \
file first. To create a file, use an empty `old_string` with a path that does not exist yet."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "Absolute path of the file"},
                    "old_string": {"type": "string", "description": "The text to replace"},
                    "new_string": {"type": "string", "description": "The replacement"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence"}
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "edit"
    }

    fn title(&self, input: &Value) -> String {
        format!("Edit {}", str_arg(input, "file_path").unwrap_or("file"))
    }

    fn locations(&self, input: &Value, session: &Session) -> Vec<String> {
        str_arg(input, "file_path").map(|p| vec![session.resolve(p).to_string_lossy().into_owned()]).unwrap_or_default()
    }

    fn needs_permission(&self, _: &Value, _: &Session) -> bool {
        true
    }

    async fn preview(&self, input: &Value, session: &Session) -> Vec<Value> {
        match plan_edit(input, session) {
            Ok(p) => vec![diff_item(&p.path, Some(str_arg(input, "old_string").unwrap_or("")).filter(|_| p.old_file.is_some()), str_arg(input, "new_string").unwrap_or(""))],
            Err(_) => Vec::new(),
        }
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let session = &ctx.session;
        let plan = match plan_edit(&input, session) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        if let Some(dir) = plan.path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            return ToolOutput::error(format!("Cannot create {}: {e}", dir.display()));
        }
        if let Err(e) = std::fs::write(&plan.path, &plan.new_file) {
            return ToolOutput::error(format!("Cannot write {}: {e}", plan.path.display()));
        }
        remember(session, &plan.path);
        let old = str_arg(&input, "old_string").unwrap_or("");
        let new = str_arg(&input, "new_string").unwrap_or("");
        let msg = if plan.old_file.is_none() {
            format!("The file {} has been created successfully.", plan.path.display())
        } else if plan.replaced > 1 {
            format!("The file {} has been updated: {} occurrences replaced.", plan.path.display(), plan.replaced)
        } else {
            format!("The file {} has been updated successfully.", plan.path.display())
        };
        ToolOutput::text(msg).with_ui(vec![diff_item(&plan.path, plan.old_file.as_ref().map(|_| old), new)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_lines_and_pages() {
        let text = (1..=5).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
        let all = number_lines(&text, None, None);
        assert!(all.starts_with("     1\tl1\n"), "{all}");
        let part = number_lines(&text, Some(2), Some(2));
        assert!(part.contains("     2\tl2") && part.contains("     3\tl3") && !part.contains("l4\n"), "{part}");
        assert!(part.contains("2 more lines; read on with offset 4"), "{part}");
        assert!(number_lines("", None, None).contains("empty"));
        assert!(number_lines("a\nb", Some(9), None).contains("shorter than the offset"));
    }

    #[test]
    fn renders_notebook_cells() {
        let nb = r#"{"cells":[{"cell_type":"code","source":["x = 1\n","print(x)"],"outputs":[{"text":["1\n"]}]}]}"#;
        let t = notebook_text(nb);
        assert!(t.contains("# Cell 0 [code]") && t.contains("print(x)") && t.contains("## Output\n1"), "{t}");
    }
}
