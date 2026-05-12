use super::{Message, Provider, ProviderResponse, ToolDefinition, ToolCall};
use serde::{Deserialize, Serialize};
use futures_util::StreamExt;

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

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {}", e))?;
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
            reasoning: None,
        })
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
        on_chunk: Box<dyn Fn(String) + Send>,
        _on_reasoning: Box<dyn Fn(String) + Send>,
    ) -> Result<ProviderResponse, String> {
        let url = format!("{}/messages", self.base_url.trim_end_matches('/'));

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
            tools: if anthropic_tools.is_empty() { None } else { Some(anthropic_tools) },
            tool_choice: if tools.is_empty() { None } else {
                Some(AnthropicToolChoice { choice_type: "auto".to_string() })
            },
        };

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

        let resp = client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(format!("Anthropic API error {}: {}", status, body_text));
        }

        let mut stream = resp.bytes_stream();
        let mut content_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut tool_use_accum: std::collections::HashMap<String, (String, String)> = std::collections::HashMap::new();
        let mut tokens_in = 0u32;
        let mut tokens_out = 0u32;
        let mut response_model = String::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| format!("Stream read error: {}", e))?;
            let text = String::from_utf8_lossy(&chunk);

            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || !line.starts_with("data: ") {
                    continue;
                }
                let data = &line[6..];

                if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                    let event_type = event["type"].as_str().unwrap_or("");

                    match event_type {
                        "content_block_start" => {
                            if let Some(block) = event["content_block"].as_object() {
                                let block_type = block["type"].as_str().unwrap_or("");
                                if block_type == "tool_use" {
                                    let id = block["id"].as_str().unwrap_or("").to_string();
                                    let name = block["name"].as_str().unwrap_or("").to_string();
                                    tool_use_accum.insert(id.clone(), (name, String::new()));
                                }
                            }
                        }
                        "content_block_delta" => {
                            if let Some(delta) = event["delta"].as_object() {
                                let delta_type = delta["type"].as_str().unwrap_or("");
                                if delta_type == "text_delta" {
                                    let text_delta = delta["text"].as_str().unwrap_or("");
                                    content_text.push_str(text_delta);
                                    on_chunk(text_delta.to_string());
                                } else if delta_type == "input_json_delta" {
                                    let partial = delta["partial_json"].as_str().unwrap_or("");
                                    if let Some(index) = event["index"].as_u64() {
                                        let id = tool_use_accum.keys().nth(index as usize).cloned();
                                        if let Some(ref key) = id {
                                            if let Some((_, ref mut acc)) = tool_use_accum.get_mut(key) {
                                                acc.push_str(partial);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        "message_delta" => {
                            if let Some(usage) = event["usage"].as_object() {
                                tokens_out = usage["output_tokens"].as_u64().unwrap_or(0) as u32;
                            }
                        }
                        "message_start" => {
                            if let Some(msg) = event["message"].as_object() {
                                tokens_in = msg["usage"].as_object()
                                    .and_then(|u| u["input_tokens"].as_u64())
                                    .unwrap_or(0) as u32;
                                response_model = msg["model"].as_str().unwrap_or(model).to_string();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        for (id, (name, args_json)) in &tool_use_accum {
            tool_calls.push(ToolCall {
                id: id.clone(),
                call_type: "function".to_string(),
                function: super::ToolCallFunction {
                    name: name.clone(),
                    arguments: args_json.clone(),
                },
            });
        }

        Ok(ProviderResponse {
            content: if content_text.is_empty() { None } else { Some(content_text) },
            tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
            tokens_in,
            tokens_out,
            model: if response_model.is_empty() { model.to_string() } else { response_model },
            reasoning: None,
        })
    }
}
