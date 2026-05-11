use crate::provider::ToolDefinition;
use serde_json::json;
use std::fs;
use std::path::Path;

// ---- Tool Definitions ----

pub fn read_file_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "read_file".into(),
            description: "Reads a file from the local filesystem. Returns content with line numbers. Use offset/limit for large files.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "filePath": {
                        "type": "string",
                        "description": "Absolute path to the file"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start from (1-indexed, default 1)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max lines to read (default 500)"
                    }
                },
                "required": ["filePath"]
            }),
        },
    }
}

pub fn write_file_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "write_file".into(),
            description: "Writes a file to the local filesystem. Creates parent directories if needed. Overwrites existing files.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "filePath": {
                        "type": "string",
                        "description": "Absolute path where the file should be written"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["filePath", "content"]
            }),
        },
    }
}

pub fn edit_file_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "edit_file".into(),
            description: "Performs exact string replacements in a file. Provide the old string to find and the new string to replace it with. The old string must be unique in the file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "filePath": {
                        "type": "string",
                        "description": "Absolute path to the file to edit"
                    },
                    "oldString": {
                        "type": "string",
                        "description": "The exact text to find and replace"
                    },
                    "newString": {
                        "type": "string",
                        "description": "The text to replace with"
                    },
                    "replaceAll": {
                        "type": "boolean",
                        "description": "Replace all occurrences (default false)"
                    }
                },
                "required": ["filePath", "oldString", "newString"]
            }),
        },
    }
}

// ---- Tool Handlers ----

pub async fn handle_read_file(args: serde_json::Value) -> Result<String, String> {
    let path = args["filePath"].as_str().ok_or("filePath required")?;
    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = args["limit"].as_u64().unwrap_or(500).max(1) as usize;

    let p = Path::new(path);
    if !p.exists() {
        return Err(format!("File not found: {}", path));
    }
    if p.is_dir() {
        let entries: Vec<String> = fs::read_dir(p)
            .map_err(|e| format!("Read dir error: {}", e))?
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir { format!("{}/", name) } else { name }
            })
            .collect();
        return Ok(entries.join("\n"));
    }

    let content = fs::read_to_string(p)
        .map_err(|e| format!("Read error: {}", e))?;

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = (offset - 1).min(total);
    let end = (start + limit).min(total);

    let mut output = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        let line_num = start + i + 1;
        output.push_str(&format!("{:>6}: {}\n", line_num, line));
    }

    if end < total {
        output.push_str(&format!(
            "\n[{} lines total, showing {}-{}]\n",
            total, start + 1, end
        ));
    }

    Ok(output)
}

pub async fn handle_write_file(args: serde_json::Value) -> Result<String, String> {
    let path = args["filePath"].as_str().ok_or("filePath required")?;
    let content = args["content"].as_str().ok_or("content required")?;

    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Create dir error: {}", e))?;
    }

    fs::write(p, content).map_err(|e| format!("Write error: {}", e))?;

    let size = content.len();
    let lines = content.lines().count();
    Ok(format!("Wrote {} bytes, {} lines to {}", size, lines, path))
}

pub async fn handle_edit_file(args: serde_json::Value) -> Result<String, String> {
    let path = args["filePath"].as_str().ok_or("filePath required")?;
    let old_str = args["oldString"].as_str().ok_or("oldString required")?;
    let new_str = args["newString"].as_str().ok_or("newString required")?;
    let replace_all = args["replaceAll"].as_bool().unwrap_or(false);

    let content = fs::read_to_string(path)
        .map_err(|e| format!("Read error: {}", e))?;

    if replace_all {
        if !content.contains(old_str) {
            return Err(format!("oldString not found in {}", path));
        }
        let count = content.matches(old_str).count();
        let new_content = content.replace(old_str, new_str);
        fs::write(path, &new_content).map_err(|e| format!("Write error: {}", e))?;
        Ok(format!("Replaced {} occurrence(s) in {}", count, path))
    } else {
        let count = content.matches(old_str).count();
        if count == 0 {
            return Err(format!("oldString not found in {}", path));
        }
        if count > 1 {
            return Err(format!(
                "Found {} matches for oldString in {}. Provide more surrounding context or use replaceAll=true.",
                count, path
            ));
        }
        let new_content = content.replacen(old_str, new_str, 1);
        fs::write(path, &new_content).map_err(|e| format!("Write error: {}", e))?;
        Ok(format!("Edited {}", path))
    }
}
