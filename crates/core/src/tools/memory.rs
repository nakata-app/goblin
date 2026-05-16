//! Memory tools — save, list, read, and delete memory entries.
//!
//! Memory entries live under `.aegis/memory/` as markdown files with
//! YAML frontmatter, indexed by `MEMORY.md`. See `crate::memory` for
//! the storage primitives.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};
use crate::memory::{MemoryEntry, MemoryMeta, MemoryStore, MemoryType};

/// Convert a `MemoryError` into a `ToolError::Io`.
fn memory_err(e: crate::memory::MemoryError) -> ToolError {
    ToolError::Io {
        path: ".metis/memory".to_string(),
        source: std::io::Error::other(e.to_string()),
    }
}

pub struct SaveMemory;

#[derive(Debug, Deserialize)]
struct SaveMemoryArgs {
    filename: String,
    name: String,
    description: String,
    #[serde(rename = "type")]
    memory_type: String,
    body: String,
}

#[async_trait]
impl Tool for SaveMemory {
    fn name(&self) -> &str {
        "save_memory"
    }
    fn description(&self) -> &str {
        "Save a memory entry to disk. Creates a new file under .metis/memory/ with YAML frontmatter and updates the MEMORY.md index. Use `type` = user|feedback|project|reference."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "filename": { "type": "string", "description": "File name, e.g. feedback_testing.md" },
                "name": { "type": "string", "description": "Short title for the memory" },
                "description": { "type": "string", "description": "One-line summary (used for relevance matching)" },
                "type": { "type": "string", "enum": ["user", "feedback", "project", "reference"] },
                "body": { "type": "string", "description": "Memory content (markdown)" }
            },
            "required": ["filename", "name", "description", "type", "body"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: SaveMemoryArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let mt = MemoryType::parse(&args.memory_type).ok_or_else(|| {
            ToolError::InvalidArgs(format!("unknown memory type `{}`", args.memory_type))
        })?;
        let store = MemoryStore::open(&ctx.workspace_root).map_err(memory_err)?;
        let entry = MemoryEntry {
            meta: MemoryMeta {
                name: args.name,
                description: args.description,
                memory_type: mt,
            },
            body: args.body,
            filename: args.filename.clone(),
        };

        // Try save first; if file exists, update instead.
        match store.save(&entry) {
            Ok(path) => Ok(format!("Memory saved: {}", path.display())),
            Err(crate::memory::MemoryError::AlreadyExists(_)) => {
                store.update(&entry).map_err(memory_err)?;
                Ok(format!("Memory updated: {}", args.filename))
            }
            Err(e) => Err(memory_err(e)),
        }
    }
}

pub struct ListMemories;

#[async_trait]
impl Tool for ListMemories {
    fn name(&self) -> &str {
        "list_memories"
    }
    fn description(&self) -> &str {
        "List all memory entries stored under .metis/memory/. Returns the MEMORY.md index content."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        })
    }
    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let store = MemoryStore::open(&ctx.workspace_root).map_err(memory_err)?;
        let index = store.read_index().map_err(memory_err)?;
        if index.is_empty() {
            Ok("No memories stored yet.".to_string())
        } else {
            Ok(index)
        }
    }
}

pub struct ReadMemory;

#[derive(Debug, Deserialize)]
struct ReadMemoryArgs {
    filename: String,
}

#[async_trait]
impl Tool for ReadMemory {
    fn name(&self) -> &str {
        "read_memory"
    }
    fn description(&self) -> &str {
        "Read a specific memory file from .metis/memory/. Returns the full content including frontmatter."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "filename": { "type": "string", "description": "File name to read, e.g. feedback_testing.md" }
            },
            "required": ["filename"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: ReadMemoryArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let store = MemoryStore::open(&ctx.workspace_root).map_err(memory_err)?;
        let entry = store.read(&args.filename).map_err(memory_err)?;
        Ok(crate::memory::format_memory_file(&entry.meta, &entry.body))
    }
}

pub struct DeleteMemory;

#[derive(Debug, Deserialize)]
struct DeleteMemoryArgs {
    filename: String,
}

#[async_trait]
impl Tool for DeleteMemory {
    fn name(&self) -> &str {
        "delete_memory"
    }
    fn description(&self) -> &str {
        "Delete a memory file from .metis/memory/ and remove its MEMORY.md index entry."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "filename": { "type": "string", "description": "File name to delete, e.g. old_note.md" }
            },
            "required": ["filename"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: DeleteMemoryArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let store = MemoryStore::open(&ctx.workspace_root).map_err(memory_err)?;
        store.delete(&args.filename).map_err(memory_err)?;
        Ok(format!("Memory deleted: {}", args.filename))
    }
}
