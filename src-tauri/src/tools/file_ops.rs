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

pub fn multi_edit_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "multi_edit".into(),
            description: "Performs multiple atomic string replacements across multiple files. All edits are validated before any file is written. Returns error if any edit fails validation, leaving all files unchanged. Each edit is { filePath, oldString, newString }.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "edits": {
                        "type": "array",
                        "items": {
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
                                    "description": "Replace all occurrences in this file (default false)"
                                }
                            },
                            "required": ["filePath", "oldString", "newString"]
                        },
                        "description": "Array of edit operations to apply atomically"
                    }
                },
                "required": ["edits"]
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

pub async fn handle_multi_edit(args: serde_json::Value) -> Result<String, String> {
    let edits = args["edits"].as_array().ok_or("edits array required")?;
    if edits.is_empty() {
        return Err("edits array is empty".to_string());
    }
    if edits.len() > 20 {
        return Err("Maximum 20 edits per multi_edit call".to_string());
    }

    #[derive(Debug)]
    struct EditOp {
        path: String,
        old_str: String,
        new_str: String,
        replace_all: bool,
    }

    let mut ops = Vec::new();
    for edit in edits {
        let path = edit["filePath"].as_str().ok_or("filePath required for each edit")?.to_string();
        let old_str = edit["oldString"].as_str().ok_or("oldString required for each edit")?.to_string();
        let new_str = edit["newString"].as_str().ok_or("newString required for each edit")?.to_string();
        let replace_all = edit["replaceAll"].as_bool().unwrap_or(false);
        ops.push(EditOp { path, old_str, new_str, replace_all });
    }

    // Phase 1: Validate all edits (read + check)
    let mut snapshots: Vec<(String, String)> = Vec::new();
    let mut new_contents: Vec<String> = Vec::new();

    for op in &ops {
        let content = fs::read_to_string(&op.path)
            .map_err(|e| format!("Read error for {}: {}", op.path, e))?;

        if op.replace_all {
            if !content.contains(&op.old_str) {
                return Err(format!("oldString not found in {}", op.path));
            }
            new_contents.push(content.replace(&op.old_str, &op.new_str));
        } else {
            let count = content.matches(&op.old_str).count();
            if count == 0 {
                return Err(format!("oldString not found in {}", op.path));
            }
            if count > 1 {
                return Err(format!(
                    "Found {} matches for oldString in {}. Provide more context or use replaceAll=true.",
                    count, op.path
                ));
            }
            new_contents.push(content.replacen(&op.old_str, &op.new_str, 1));
        }

        snapshots.push((op.path.clone(), content));
    }

    // Phase 2: Apply all edits
    let mut results = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        fs::write(&op.path, &new_contents[i])
            .map_err(|e| {
                // Rollback on failure
                for (path, content) in &snapshots[..i] {
                    let _ = fs::write(path, content);
                }
                format!("Write error for {}: {}", op.path, e)
            })?;
        results.push(op.path.clone());
    }

    Ok(format!(
        "Applied {} edit(s) atomically across {} file(s):\n{}",
        ops.len(),
        results.iter().collect::<std::collections::HashSet<_>>().len(),
        results.iter().map(|p| format!("  - {}", p)).collect::<Vec<_>>().join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path(name: &str) -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let mut p = std::env::temp_dir();
        p.push(format!("goblin_file_{}_{}", pid, id));
        let _ = fs::create_dir_all(&p);
        p.push(name);
        p
    }

    fn cleanup(name: &str) {
        let p = temp_path(name);
        let _ = fs::remove_file(&p);
    }

    #[tokio::test]
    async fn read_file_not_found() {
        let result = handle_read_file(json!({"filePath": "/nonexistent/path.xyz"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn read_file_directory() {
        let result = handle_read_file(json!({"filePath": "/"})).await;
        assert!(result.is_ok());
        // root dizini boş değildir
        assert!(!result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn write_and_read_roundtrip() {
        let path = temp_path("roundtrip.txt");
        let content = "hello\nworld\ntest\n";

        handle_write_file(json!({"filePath": path.to_str().unwrap(), "content": content})).await.unwrap();
        let read = handle_read_file(json!({"filePath": path.to_str().unwrap()})).await.unwrap();

        assert!(read.contains("hello"));
        assert!(read.contains("world"));
        assert!(read.contains("test"));

        cleanup("roundtrip.txt");
    }

    #[tokio::test]
    async fn read_file_with_offset() {
        let path = temp_path("offset.txt");
        fs::write(&path, "line1\nline2\nline3\nline4\n").unwrap();

        let result = handle_read_file(json!({"filePath": path.to_str().unwrap(), "offset": 2, "limit": 2})).await.unwrap();
        assert!(result.contains("line2"));
        assert!(result.contains("line3"));
        assert!(!result.contains("line4"));

        cleanup("offset.txt");
    }

    #[tokio::test]
    async fn write_file_creates_parent_dirs() {
        let mut p = std::env::temp_dir();
        p.push("goblin_test/nested/deep/test.txt");
        let content = "deep file";

        handle_write_file(json!({"filePath": p.to_str().unwrap(), "content": content})).await.unwrap();
        assert!(p.exists());
        assert_eq!(fs::read_to_string(&p).unwrap(), content);

        let _ = fs::remove_dir_all(p.parent().unwrap().parent().unwrap().parent().unwrap());
    }

    #[tokio::test]
    async fn edit_file_single_occurrence() {
        let path = temp_path("edit_single.txt");
        fs::write(&path, "hello world").unwrap();

        let result = handle_edit_file(json!({
            "filePath": path.to_str().unwrap(),
            "oldString": "hello",
            "newString": "goodbye"
        })).await.unwrap();

        assert!(result.contains("Edited"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "goodbye world");

        cleanup("edit_single.txt");
    }

    #[tokio::test]
    async fn edit_file_replace_all() {
        let path = temp_path("edit_all.txt");
        fs::write(&path, "foo bar foo baz foo").unwrap();

        let result = handle_edit_file(json!({
            "filePath": path.to_str().unwrap(),
            "oldString": "foo",
            "newString": "qux",
            "replaceAll": true
        })).await.unwrap();

        assert!(result.contains("3 occurrence"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "qux bar qux baz qux");

        cleanup("edit_all.txt");
    }

    #[tokio::test]
    async fn edit_file_not_found() {
        let path = temp_path("edit_notfound.txt");
        fs::write(&path, "some content").unwrap();

        let result = handle_edit_file(json!({
            "filePath": path.to_str().unwrap(),
            "oldString": "nonexistent",
            "newString": "replacement"
        })).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));

        cleanup("edit_notfound.txt");
    }

    #[tokio::test]
    async fn edit_file_multiple_matches() {
        let path = temp_path("edit_multi.txt");
        fs::write(&path, "abc xyz abc").unwrap();

        let result = handle_edit_file(json!({
            "filePath": path.to_str().unwrap(),
            "oldString": "abc",
            "newString": "def"
        })).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Found 2 matches"));

        cleanup("edit_multi.txt");
    }

    #[tokio::test]
    async fn multi_edit_atomic_success() {
        let p1 = temp_path("multi_a.txt");
        let p2 = temp_path("multi_b.txt");
        fs::write(&p1, "apple").unwrap();
        fs::write(&p2, "banana").unwrap();

        let result = handle_multi_edit(json!({
            "edits": [
                {"filePath": p1.to_str().unwrap(), "oldString": "apple", "newString": "APPLE"},
                {"filePath": p2.to_str().unwrap(), "oldString": "banana", "newString": "BANANA"}
            ]
        })).await.unwrap();

        assert!(result.contains("2 edit(s)"));
        assert_eq!(fs::read_to_string(&p1).unwrap(), "APPLE");
        assert_eq!(fs::read_to_string(&p2).unwrap(), "BANANA");

        cleanup("multi_a.txt");
        cleanup("multi_b.txt");
    }

    #[tokio::test]
    async fn multi_edit_empty_edits() {
        let result = handle_multi_edit(json!({"edits": []})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn multi_edit_max_limit() {
        let edits: Vec<serde_json::Value> = (0..21).map(|_| json!({
            "filePath": "/tmp/nonexistent.txt",
            "oldString": "x",
            "newString": "y"
        })).collect();
        let result = handle_multi_edit(json!({"edits": edits})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("20"));
    }

    #[tokio::test]
    async fn multi_edit_rollback_on_failure() {
        let p1 = temp_path("rb_a.txt");
        let p2 = temp_path("rb_b.txt");
        let original = "original content";
        fs::write(&p1, original).unwrap();

        let result = handle_multi_edit(json!({
            "edits": [
                {"filePath": p1.to_str().unwrap(), "oldString": "original", "newString": "modified"},
                {"filePath": p2.to_str().unwrap(), "oldString": "foo", "newString": "bar"}
            ]
        })).await;

        assert!(result.is_err());
        // p1 should be rolled back to original
        assert_eq!(fs::read_to_string(&p1).unwrap(), original);

        cleanup("rb_a.txt");
        cleanup("rb_b.txt");
    }
}
