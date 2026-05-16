//! Jupyter-notebook editing tool — `notebook_edit`.
//!
//! Operates on the `cells` array of an `.ipynb` file with three commands:
//! `edit_cell`, `insert_cell`, `delete_cell`. Cell indices are 1-based.
//! New source text is stored in the notebook's array-of-lines format
//! via [`source_to_lines`].

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

pub struct NotebookEdit;

#[derive(Debug, Deserialize)]
struct NotebookEditArgs {
    /// Path to the .ipynb file.
    path: String,
    /// Operation: "edit_cell", "insert_cell", "delete_cell".
    command: String,
    /// 1-based cell index to operate on.
    cell_index: usize,
    /// New source content (required for edit_cell and insert_cell).
    #[serde(default)]
    new_source: Option<String>,
    /// Cell type for insert_cell: "code" or "markdown" (default: "code").
    #[serde(default)]
    cell_type: Option<String>,
}

#[async_trait]
impl Tool for NotebookEdit {
    fn name(&self) -> &str {
        "notebook_edit"
    }
    fn description(&self) -> &str {
        "Edit Jupyter notebook (.ipynb) cells. Supports three commands: \
         'edit_cell' replaces the source of cell at cell_index, \
         'insert_cell' inserts a new cell before cell_index, \
         'delete_cell' removes the cell at cell_index. \
         Cell indices are 1-based."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the .ipynb file"
                },
                "command": {
                    "type": "string",
                    "enum": ["edit_cell", "insert_cell", "delete_cell"],
                    "description": "Operation to perform"
                },
                "cell_index": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based cell index"
                },
                "new_source": {
                    "type": "string",
                    "description": "New cell content (required for edit_cell and insert_cell)"
                },
                "cell_type": {
                    "type": "string",
                    "enum": ["code", "markdown"],
                    "description": "Cell type for insert_cell (default: code)"
                }
            },
            "required": ["path", "command", "cell_index"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: NotebookEditArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let path = ctx.resolve_path(&args.path)?;
        if path.extension().and_then(|e| e.to_str()) != Some("ipynb") {
            return Err(ToolError::InvalidArgs(
                "notebook_edit only works on .ipynb files".to_string(),
            ));
        }

        let contents = std::fs::read_to_string(&path).map_err(|source| ToolError::Io {
            path: args.path.clone(),
            source,
        })?;
        let mut nb: serde_json::Value = serde_json::from_str(&contents)
            .map_err(|e| ToolError::InvalidArgs(format!("invalid notebook JSON: {e}")))?;

        let cells = nb
            .get_mut("cells")
            .and_then(|c| c.as_array_mut())
            .ok_or_else(|| ToolError::InvalidArgs("notebook has no cells array".to_string()))?;

        let idx = args.cell_index; // 1-based
        let result = match args.command.as_str() {
            "edit_cell" => {
                let new_source = args.new_source.ok_or_else(|| {
                    ToolError::InvalidArgs("edit_cell requires new_source".to_string())
                })?;
                if idx < 1 || idx > cells.len() {
                    return Err(ToolError::InvalidArgs(format!(
                        "cell_index {idx} out of range (1..{})",
                        cells.len()
                    )));
                }
                let cell = &mut cells[idx - 1];
                // Convert source to array-of-lines format (notebook convention)
                let source_lines = source_to_lines(&new_source);
                cell["source"] = source_lines;
                format!("Edited cell {idx} in {}", args.path)
            }
            "insert_cell" => {
                let new_source = args.new_source.ok_or_else(|| {
                    ToolError::InvalidArgs("insert_cell requires new_source".to_string())
                })?;
                if idx < 1 || idx > cells.len() + 1 {
                    return Err(ToolError::InvalidArgs(format!(
                        "cell_index {idx} out of range for insert (1..={})",
                        cells.len() + 1
                    )));
                }
                let ct = args.cell_type.as_deref().unwrap_or("code");
                let source_lines = source_to_lines(&new_source);
                let new_cell = if ct == "markdown" {
                    json!({
                        "cell_type": "markdown",
                        "metadata": {},
                        "source": source_lines
                    })
                } else {
                    json!({
                        "cell_type": "code",
                        "metadata": {},
                        "source": source_lines,
                        "outputs": [],
                        "execution_count": null
                    })
                };
                cells.insert(idx - 1, new_cell);
                format!("Inserted {} cell at position {idx} in {}", ct, args.path)
            }
            "delete_cell" => {
                if idx < 1 || idx > cells.len() {
                    return Err(ToolError::InvalidArgs(format!(
                        "cell_index {idx} out of range (1..{})",
                        cells.len()
                    )));
                }
                let removed = &cells[idx - 1];
                let ct = removed
                    .get("cell_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let msg = format!("Deleted {ct} cell {idx} from {}", args.path);
                cells.remove(idx - 1);
                msg
            }
            other => {
                return Err(ToolError::InvalidArgs(format!("unknown command: {other}")));
            }
        };

        // Write back
        let output = serde_json::to_string_pretty(&nb)
            .map_err(|e| ToolError::InvalidArgs(format!("failed to serialize notebook: {e}")))?;
        std::fs::write(&path, output.as_bytes()).map_err(|source| ToolError::Io {
            path: args.path,
            source,
        })?;

        Ok(result)
    }
}

/// Convert a string to the notebook source array format (array of lines,
/// each line includes its trailing newline except possibly the last).
pub(super) fn source_to_lines(s: &str) -> serde_json::Value {
    let lines: Vec<String> = s
        .split('\n')
        .enumerate()
        .map(|(i, line)| {
            // All lines except the last get a trailing newline
            if i < s.matches('\n').count() {
                format!("{line}\n")
            } else {
                line.to_string()
            }
        })
        .collect();
    json!(lines)
}
