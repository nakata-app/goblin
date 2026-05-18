use super::{Message, Provider, ProviderResponse, ToolDefinition, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use futures_util::StreamExt;

pub struct OpenAIProvider {
    pub api_key: String,
    pub base_url: String,
    pub max_tokens: u32,
}

/// Convert one `Message` into the OpenAI Chat Completions wire shape.
/// When there are no attachments this is the boring `{ role, content }`
/// object that every existing call site already produces. With image
/// attachments the content morphs into the multimodal array form:
///   content: [
///     { "type": "text", "text": "..." },
///     { "type": "image_url", "image_url": { "url": "data:image/png;base64,..." } }
///   ]
/// NVIDIA NIM and ZhipuAI GLM accept the same shape, so nvidia.rs and
/// glm.rs reuse this helper instead of duplicating the logic.
pub(crate) fn to_openai_message(msg: &Message) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("role".to_string(), json!(msg.role));

    if msg.has_images() {
        let mut parts: Vec<Value> = Vec::with_capacity(msg.attachments.len() + 1);
        if !msg.content.is_empty() {
            parts.push(json!({ "type": "text", "text": msg.content }));
        }
        for att in &msg.attachments {
            parts.push(json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{}", att.mime_type, att.data),
                },
            }));
        }
        obj.insert("content".to_string(), Value::Array(parts));
    } else {
        obj.insert("content".to_string(), json!(msg.content));
    }

    if let Some(ref tcs) = msg.tool_calls {
        obj.insert("tool_calls".to_string(), serde_json::to_value(tcs).unwrap_or(Value::Null));
    }
    if let Some(ref id) = msg.tool_call_id {
        obj.insert("tool_call_id".to_string(), json!(id));
    }
    if let Some(ref r) = msg.reasoning {
        obj.insert("reasoning_content".to_string(), json!(r));
    }

    Value::Object(obj)
}

pub(crate) fn to_openai_messages(msgs: &[Message]) -> Vec<Value> {
    msgs.iter().map(to_openai_message).collect()
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolDefinition]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<super::ToolCall>>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    // finish_reason on stream chunks is unused (we react to the [DONE]
    // sentinel instead); kept so serde does not fail on providers that
    // include it.
    #[serde(default)]
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::Attachment;
    use crate::config::Config;

    fn user_msg(text: &str, atts: Vec<Attachment>) -> Message {
        Message {
            role: "user".into(),
            content: text.into(),
            tool_calls: None,
            tool_call_id: None,
            reasoning: None,
            attachments: atts,
        }
    }

    #[test]
    fn openai_message_text_only_stays_string_content() {
        let msg = user_msg("hello", vec![]);
        let v = to_openai_message(&msg);
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"], "hello");
        assert!(v["content"].is_string());
    }

    #[test]
    fn openai_message_with_image_becomes_array_content() {
        let att = Attachment {
            mime_type: "image/png".into(),
            data: "ZmFrZQ==".into(),
        };
        let msg = user_msg("describe this", vec![att]);
        let v = to_openai_message(&msg);
        assert!(v["content"].is_array());
        let parts = v["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "describe this");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,ZmFrZQ==");
    }

    #[test]
    fn openai_message_image_without_text_skips_text_block() {
        let att = Attachment {
            mime_type: "image/jpeg".into(),
            data: "Zm9v".into(),
        };
        let msg = user_msg("", vec![att]);
        let v = to_openai_message(&msg);
        let parts = v["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "image_url");
    }

    #[test]
    fn openai_message_multiple_images() {
        let atts = vec![
            Attachment { mime_type: "image/png".into(), data: "AAAA".into() },
            Attachment { mime_type: "image/webp".into(), data: "BBBB".into() },
        ];
        let msg = user_msg("compare", atts);
        let v = to_openai_message(&msg);
        let parts = v["content"].as_array().unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[2]["image_url"]["url"], "data:image/webp;base64,BBBB");
    }

    #[tokio::test]
    #[ignore = "requires network + ~/.goblin/config.toml with [providers.openai]"]
    async fn real_deepseek_v4_pro_api_call() {
        let config = Config::load().expect("Failed to load config");
        let openai_cfg = config.providers.openai
            .expect("No openai provider in config. Set up ~/.goblin/config.toml");

        let provider = OpenAIProvider {
            api_key: openai_cfg.api_key.clone(),
            base_url: openai_cfg.base_url.clone(),
            max_tokens: config.agent.max_tokens,
        };

        let messages = vec![
            Message {
                role: "user".into(),
                content: "Say 'Goblin test OK' and nothing else.".into(),
                tool_calls: None,
                tool_call_id: None,
                reasoning: None,
            attachments: vec![],
            },
        ];

        let result = provider.chat(&messages, &[], "deepseek-v4-pro").await;

        match &result {
            Ok(resp) => {
                eprintln!("=== DeepSeek v4-pro API TEST ===");
                eprintln!("Content: {:?}", resp.content);
                eprintln!("Tokens in: {}, out: {}", resp.tokens_in, resp.tokens_out);
                eprintln!("Model: {}", resp.model);
                assert!(resp.content.is_some(), "Response content should not be empty");
                assert!(resp.tokens_in > 0, "Should have input tokens");
                assert!(resp.tokens_out > 0, "Should have output tokens");
                assert!(!resp.model.is_empty(), "Model should be returned");
            }
            Err(e) => {
                panic!("DeepSeek v4-pro API call failed: {}", e);
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires network + ~/.goblin/config.toml with [providers.openai]"]
    async fn real_deepseek_v4_flash_api_call() {
        let config = Config::load().expect("Failed to load config");
        let openai_cfg = config.providers.openai
            .expect("No openai provider in config");

        let provider = OpenAIProvider {
            api_key: openai_cfg.api_key.clone(),
            base_url: openai_cfg.base_url.clone(),
            max_tokens: config.agent.max_tokens,
        };

        let messages = vec![
            Message {
                role: "user".into(),
                content: "Reply with just the number 42.".into(),
                tool_calls: None,
                tool_call_id: None,
                reasoning: None,
            attachments: vec![],
            },
        ];

        let result = provider.chat(&messages, &[], "deepseek-v4-flash").await;

        match &result {
            Ok(resp) => {
                eprintln!("=== DeepSeek v4-flash API TEST ===");
                eprintln!("Content: {:?}", resp.content);
                eprintln!("Tokens in: {}, out: {}", resp.tokens_in, resp.tokens_out);
                assert!(resp.content.is_some());
                assert!(resp.tokens_in > 0);
                assert!(resp.tokens_out > 0);
            }
            Err(e) => {
                panic!("DeepSeek v4-flash API call failed: {}", e);
            }
        }
    }
}

#[async_trait::async_trait]
impl Provider for OpenAIProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
    ) -> Result<ProviderResponse, String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let request_body = ChatRequest {
            model,
            messages: to_openai_messages(messages),
            tools: if tools.is_empty() { None } else { Some(tools) },
            tool_choice: if tools.is_empty() { None } else { Some("auto") },
            stream: false,
            temperature: Some(0.0),
            max_tokens: Some(self.max_tokens),
        };

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {}", e))?;
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("API error {}: {}", status, body));
        }

        let chat_response: ChatResponse = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse response: {}", e))?;

        let choice = chat_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| "No choices in response".to_string())?;

        let usage = chat_response.usage.unwrap_or(Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
        });

        let finish_reason = choice.finish_reason.as_deref().unwrap_or("");
        let content_empty = choice
            .message
            .content
            .as_ref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true);
        let tool_calls_empty = choice.message.tool_calls.as_ref().map(|v| v.is_empty()).unwrap_or(true);
        if finish_reason == "length" && content_empty && tool_calls_empty {
            return Err(format!(
                "Model used the entire token budget on internal reasoning and emitted no answer. \
                 Increase agent.max_tokens (current {}) in ~/.goblin/config.toml.",
                self.max_tokens
            ));
        }

        Ok(ProviderResponse {
            content: choice.message.content,
            tool_calls: choice.message.tool_calls,
            tokens_in: usage.prompt_tokens,
            tokens_out: usage.completion_tokens,
            model: model.to_string(),
            reasoning: choice.message.reasoning_content,
        })
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
        on_chunk: Box<dyn Fn(String) + Send>,
        on_reasoning: Box<dyn Fn(String) + Send>,
    ) -> Result<ProviderResponse, String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let request_body = ChatRequest {
            model,
            messages: to_openai_messages(messages),
            tools: if tools.is_empty() { None } else { Some(tools) },
            tool_choice: if tools.is_empty() { None } else { Some("auto") },
            stream: true,
            temperature: Some(0.0),
            max_tokens: Some(self.max_tokens),
        };

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("API error {}: {}", status, body));
        }

        parse_sse_stream(resp, model, on_chunk, on_reasoning).await
    }
}

pub async fn parse_sse_stream(
    resp: reqwest::Response,
    model: &str,
    on_chunk: Box<dyn Fn(String) + Send>,
    on_reasoning: Box<dyn Fn(String) + Send>,
) -> Result<ProviderResponse, String> {
    let mut stream = resp.bytes_stream();
    let mut full_content = String::new();
    let mut full_reasoning = String::new();
    let mut full_tool_calls: Vec<ToolCall> = Vec::new();
    let mut tokens_in: u32 = 0;
    let mut tokens_out: u32 = 0;

    while let Some(chunk_result) = stream.next().await {
        let chunk_bytes = chunk_result.map_err(|e| format!("Stream read error: {}", e))?;
        let chunk_text = String::from_utf8_lossy(&chunk_bytes);

        for line in chunk_text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line == "data: [DONE]" {
                break;
            }
            if !line.starts_with("data: ") {
                continue;
            }

            let json_str = &line["data: ".len()..];
            let parsed: StreamChunk = match serde_json::from_str(json_str) {
                Ok(c) => c,
                Err(_) => continue,
            };

            if let Some(usage) = parsed.usage {
                tokens_in = usage.prompt_tokens;
                tokens_out = usage.completion_tokens;
            }

            let choices = parsed.choices;
            for choice in &choices {
                if let Some(ref content) = choice.delta.content {
                    full_content.push_str(content);
                    on_chunk(content.clone());
                }
                if let Some(ref reasoning) = choice.delta.reasoning_content {
                    full_reasoning.push_str(reasoning);
                    on_reasoning(reasoning.clone());
                }
                if let Some(ref tcs) = choice.delta.tool_calls {
                    for tc in tcs {
                        full_tool_calls.push(tc.clone());
                    }
                }
            }
        }
    }

    Ok(ProviderResponse {
        content: if full_content.is_empty() { None } else { Some(full_content) },
        tool_calls: if full_tool_calls.is_empty() { None } else { Some(full_tool_calls) },
        tokens_in,
        tokens_out,
        model: model.to_string(),
        reasoning: if full_reasoning.is_empty() { None } else { Some(full_reasoning) },
    })
}
