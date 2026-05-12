use super::{Message, Provider, ProviderResponse, ToolDefinition};
use serde::{Deserialize, Serialize};

pub struct NvidiaProvider {
    pub api_key: String,
    pub base_url: String,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolDefinition]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
    stream: bool,
    temperature: f32,
    max_tokens: u32,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<super::ToolCall>>,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

#[async_trait::async_trait]
impl Provider for NvidiaProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
    ) -> Result<ProviderResponse, String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let request_body = ChatRequest {
            model,
            messages,
            tools: if tools.is_empty() { None } else { Some(tools) },
            tool_choice: if tools.is_empty() { None } else { Some("auto") },
            stream: false,
            temperature: 0.0,
            max_tokens: 8192,
        };

        let client = reqwest::Client::new();
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
            return Err(format!("NVIDIA API error {}: {}", status, body));
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

        Ok(ProviderResponse {
            content: choice.message.content,
            tool_calls: choice.message.tool_calls,
            tokens_in: usage.prompt_tokens,
            tokens_out: usage.completion_tokens,
            model: model.to_string(),
            reasoning: None,
        })
    }
}
