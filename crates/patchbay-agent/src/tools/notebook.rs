//! NotebookEdit: change one cell of a Jupyter notebook.

use async_trait::async_trait;
use patchbay_llm::ToolSpec;
use serde_json::{Value, json};

use super::fs::{check_fresh, remember};
use super::*;

pub struct NotebookEdit;

/// A cell's source as nbformat stores it: a list of lines that keep their newlines.
fn source_lines(text: &str) -> Value {
    json!(text.split_inclusive('\n').collect::<Vec<_>>())
}

fn find_cell(cells: &[Value], id: &str) -> Option<usize> {
    cells
        .iter()
        .position(|c| c["id"].as_str() == Some(id))
        .or_else(|| id.parse::<usize>().ok().filter(|i| *i < cells.len()))
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_owned()
}

/// Apply one edit; returns what happened, for the model.
fn edit_cells(nb: &mut Value, input: &Value) -> Result<String, String> {
    let mode = str_arg(input, "edit_mode").unwrap_or("replace");
    let cells = nb["cells"].as_array_mut().ok_or("the notebook has no cells")?;
    let at = str_arg(input, "cell_id").map(|id| find_cell(cells, id).ok_or_else(|| format!("no cell {id}"))).transpose()?;
    let source = str_arg(input, "new_source");
    let cell_type = str_arg(input, "cell_type");
    match mode {
        "replace" => {
            let i = at.ok_or("cell_id is required to replace a cell")?;
            let source = source.ok_or("new_source is required")?;
            let cell = &mut cells[i];
            cell["source"] = source_lines(source);
            if let Some(t) = cell_type {
                cell["cell_type"] = json!(t);
            }
            if cell["cell_type"] == "code" {
                cell["outputs"] = json!([]);
                cell["execution_count"] = Value::Null;
            }
            Ok(format!("Replaced the source of cell {i}."))
        }
        "insert" => {
            let source = source.ok_or("new_source is required")?;
            let t = cell_type.ok_or("cell_type is required to insert a cell")?;
            let mut cell = json!({"cell_type": t, "id": new_id(), "metadata": {}, "source": source_lines(source)});
            if t == "code" {
                cell["outputs"] = json!([]);
                cell["execution_count"] = Value::Null;
            }
            let pos = at.map(|i| i + 1).unwrap_or(0);
            cells.insert(pos, cell);
            Ok(format!("Inserted a new {t} cell at position {pos}."))
        }
        "delete" => {
            let i = at.ok_or("cell_id is required to delete a cell")?;
            cells.remove(i);
            Ok(format!("Deleted cell {i}."))
        }
        other => Err(format!("unknown edit_mode {other}")),
    }
}

#[async_trait]
impl Tool for NotebookEdit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "NotebookEdit".into(),
            description: "Replace, insert or delete one cell of a Jupyter notebook (.ipynb). Read the notebook \
first. `cell_id` is a cell's id or its 0-based index. `replace` needs `new_source`; `insert` adds a cell \
after `cell_id` (at the start when it is empty) and needs `new_source` and `cell_type`; `delete` removes \
the cell. A replaced code cell loses its outputs."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "notebook_path": {"type": "string", "description": "Absolute path of the notebook"},
                    "cell_id": {"type": "string", "description": "Cell id or 0-based index"},
                    "new_source": {"type": "string", "description": "The cell's new source"},
                    "cell_type": {"type": "string", "enum": ["code", "markdown"]},
                    "edit_mode": {"type": "string", "enum": ["replace", "insert", "delete"]}
                },
                "required": ["notebook_path"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "edit"
    }

    fn title(&self, input: &Value) -> String {
        format!("Edit notebook {}", str_arg(input, "notebook_path").unwrap_or(""))
    }

    fn locations(&self, input: &Value, session: &crate::session::Session) -> Vec<String> {
        str_arg(input, "notebook_path").map(|p| vec![session.resolve(p).to_string_lossy().into_owned()]).unwrap_or_default()
    }

    fn needs_permission(&self, _: &Value, _: &crate::session::Session) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(file) = str_arg(&input, "notebook_path") else {
            return ToolOutput::error("notebook_path is required");
        };
        let session = &ctx.session;
        let path = session.resolve(file);
        if path.extension().and_then(|e| e.to_str()) != Some("ipynb") {
            return ToolOutput::error("NotebookEdit only edits .ipynb files.");
        }
        if let Err(e) = check_fresh(session, &path) {
            return ToolOutput::error(e);
        }
        let mut nb: Value = match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string())) {
            Ok(v) => v,
            Err(e) => return ToolOutput::error(format!("Cannot read the notebook: {e}")),
        };
        let what = match edit_cells(&mut nb, &input) {
            Ok(w) => w,
            Err(e) => return ToolOutput::error(e),
        };
        let mut out = serde_json::to_string_pretty(&nb).unwrap_or_default();
        out.push('\n');
        session.snapshot_file(&path);
        if let Err(e) = std::fs::write(&path, out) {
            return ToolOutput::error(format!("Cannot write {}: {e}", path.display()));
        }
        remember(session, &path);
        ToolOutput::text(what)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nb() -> Value {
        json!({"cells": [
            {"cell_type": "code", "id": "aa", "metadata": {}, "source": ["x = 1\n"], "outputs": [{"text": ["1"]}], "execution_count": 3},
            {"cell_type": "markdown", "id": "bb", "metadata": {}, "source": ["# hi"]},
        ]})
    }

    #[test]
    fn replaces_inserts_and_deletes() {
        let mut n = nb();
        edit_cells(&mut n, &json!({"cell_id": "aa", "new_source": "y = 2\nz = 3"})).unwrap();
        assert_eq!(n["cells"][0]["source"], json!(["y = 2\n", "z = 3"]));
        assert_eq!(n["cells"][0]["outputs"], json!([]));
        assert!(n["cells"][0]["execution_count"].is_null());
        edit_cells(&mut n, &json!({"edit_mode": "insert", "cell_id": "0", "cell_type": "markdown", "new_source": "note"})).unwrap();
        assert_eq!(n["cells"][1]["source"], json!(["note"]));
        assert_eq!(n["cells"].as_array().unwrap().len(), 3);
        edit_cells(&mut n, &json!({"edit_mode": "delete", "cell_id": "bb"})).unwrap();
        assert_eq!(n["cells"].as_array().unwrap().len(), 2);
        assert!(edit_cells(&mut n, &json!({"cell_id": "nope", "new_source": "x"})).is_err());
    }
}
