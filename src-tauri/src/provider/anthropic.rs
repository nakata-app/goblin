use super::{Message, Provider, ProviderResponse, ToolDefinition, ToolCall};
use serde::{Deserialize, Serialize};

pub struct AnthropicProvider {
    pub api_key: String,
    pub base_url: String,
}

#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: Option<String>,
    messages: Vec<AnthropicMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
}

#[derive(Serialize)]
struct AnthropicMessage<'a> {
    role: &'a str,
    content: Vec<AnthropicContent<'a>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum AnthropicContent<'a> {
    Text { #[serde(rename = "type")] content_type: &'a str, text: &'a str },
    ToolResult {
        #[serde(rename = "type")] content_type: &'a str,
        tool_use_id: &'a str,
        content: &'a str,
    },
}

#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Serialize)]
struct AnthropicToolChoice {
    #[serde(rename = "type")]
    choice_type: String,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicResponseBlock>,
    usage: AnthropicUsage,
    model: String,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum AnthropicResponseBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
    ) -> Result<ProviderResponse, String> {
        let url = format!("{}/messages", self.base_url.trim_end_matches('/'));

        // Extract system message if first message is "system"
        let mut system = None;
        let mut anthropic_msgs = Vec::new();

        for msg in messages {
            if msg.role == "system" {
                if system.is_none() {
                    system = Some(msg.content.clone());
                }
                continue;
            }

            let mut content = Vec::new();

            if msg.role == "tool" {
                content.push(AnthropicContent::ToolResult {
                    content_type: "tool_result",
                    tool_use_id: msg.tool_call_id.as_deref().unwrap_or(""),
                    content: &msg.content,
                });
            } else {
                content.push(AnthropicContent::Text {
                    content_type: "text",
                    text: &msg.content,
                });
            }

            anthropic_msgs.push(AnthropicMessage {
                role: if msg.role == "user" { "user" } else { "assistant" },
                content,
            });
        }

        let anthropic_tools: Vec<AnthropicTool> = tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.function.name.clone(),
                description: t.function.description.clone(),
                input_schema: t.function.parameters.clone(),
            })
            .collect();

        let body = AnthropicRequest {
            model,
            max_tokens: 8192,
            system,
            messages: anthropic_msgs,
            tools: if anthropic_tools.is_empty() {
                None
            } else {
                Some(anthropic_tools)
            },
            tool_choice: if tools.is_empty() {
                None
            } else {
                Some(AnthropicToolChoice {
                    choice_type: "auto".to_string(),
                })
            },
        };

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(format!("Anthropic API error {}: {}", status, body_text));
        }

        let response: AnthropicResponse = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse response: {}", e))?;

        let mut content_text = String::new();
        let mut tool_calls = Vec::new();

        for block in &response.content {
            match block {
                AnthropicResponseBlock::Text { text } => {
                    content_text.push_str(text);
                }
                AnthropicResponseBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id: id.clone(),
                        call_type: "function".to_string(),
                        function: super::ToolCallFunction {
                            name: name.clone(),
                            arguments: serde_json::to_string(input)
                                .unwrap_or_else(|_| "{}".to_string()),
                        },
                    });
                }
            }
        }

        Ok(ProviderResponse {
            content: if content_text.is_empty() {
                None
            } else {
                Some(content_text)
            },
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tokens_in: response.usage.input_tokens,
            tokens_out: response.usage.output_tokens,
            model: response.model,
        })
    }
}
