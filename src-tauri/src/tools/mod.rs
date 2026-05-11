pub mod file_ops;
pub mod search;
pub mod shell;
pub mod web;

use crate::provider::ToolDefinition;
use std::collections::HashMap;
use std::pin::Pin;
use std::future::Future;

type AsyncToolResult = Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;

type ToolHandler = Box<dyn Fn(serde_json::Value) -> AsyncToolResult + Send + Sync>;

pub struct ToolRegistry {
    definitions: Vec<ToolDefinition>,
    handlers: HashMap<String, ToolHandler>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            definitions: Vec::new(),
            handlers: HashMap::new(),
        }
    }

    pub fn register<F, Fut>(&mut self, def: ToolDefinition, handler: F)
    where
        F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String, String>> + Send + 'static,
    {
        let name = def.function.name.clone();
        self.definitions.push(def);
        self.handlers.insert(
            name,
            Box::new(move |args| Box::pin(handler(args))),
        );
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions.clone()
    }

    pub async fn execute(&self, name: &str, args: serde_json::Value) -> Result<String, String> {
        let handler = self
            .handlers
            .get(name)
            .ok_or_else(|| format!("Unknown tool: {}", name))?;
        handler(args).await
    }

    pub fn names(&self) -> Vec<String> {
        self.handlers.keys().cloned().collect()
    }
}

pub fn create_tool_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    registry.register(file_ops::read_file_def(), file_ops::handle_read_file);
    registry.register(file_ops::write_file_def(), file_ops::handle_write_file);
    registry.register(file_ops::edit_file_def(), file_ops::handle_edit_file);
    registry.register(search::grep_def(), search::handle_grep);
    registry.register(search::glob_def(), search::handle_glob);
    registry.register(shell::bash_def(), shell::handle_bash);
    registry.register(web::web_fetch_def(), web::handle_web_fetch);
    registry.register(web::web_search_def(), web::handle_web_search);

    registry
}
