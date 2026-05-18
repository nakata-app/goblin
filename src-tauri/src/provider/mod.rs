pub mod openai;
pub mod anthropic;
pub mod nvidia;
pub mod gemini;
pub mod glm;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "reasoning_content")]
    pub reasoning: Option<String>,
    /// Image attachments piggy-backed on this turn. Empty 99% of the
    /// time; populated when the user drag-drops images into InputBar.
    /// Provider serializers translate these into the right multimodal
    /// content block format (OpenAI image_url, Anthropic source block,
    /// Gemini inline_data).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// A base64-encoded image attachment. We carry only image attachments
/// for now — text files are pre-pasted into the message content, audio
/// is out of scope until any provider standardises it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// `image/png`, `image/jpeg`, `image/webp`, `image/gif`. Lower-case.
    pub mime_type: String,
    /// Raw base64 body (no `data:` prefix). Each provider adds the
    /// prefix shape it needs.
    pub data: String,
}

impl Message {
    pub fn has_images(&self) -> bool {
        !self.attachments.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub def_type: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderResponse {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub model: String,
    pub reasoning: Option<String>,
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
    ) -> Result<ProviderResponse, String>;

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
        on_chunk: Box<dyn Fn(String) + Send>,
        on_reasoning: Box<dyn Fn(String) + Send>,
    ) -> Result<ProviderResponse, String> {
        let _ = (messages, tools, model, on_chunk, on_reasoning);
        Err("Streaming not supported by this provider".to_string())
    }
}

#[allow(dead_code)]
pub fn tool_use_system_prompt() -> &'static str {
    "You have access to a set of tools you can use to answer the user's question.\n\
     Use tools only when necessary. For each tool call, return a tool_calls block.\n\
     After receiving tool results, you can make additional tool calls or provide a final answer.\n\
     Always think step by step before acting."
}
