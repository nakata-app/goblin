use super::{Message, Provider, ProviderResponse, ToolDefinition};
use serde::{Deserialize, Serialize};

pub struct OpenAIProvider {
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
    #[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[tokio::test]
    async fn real_deepseek_v4_pro_api_call() {
        let config = Config::load().expect("Failed to load config");
        let openai_cfg = config.providers.openai
            .expect("No openai provider in config. Set up ~/.goblin/config.toml");

        let provider = OpenAIProvider {
            api_key: openai_cfg.api_key.clone(),
            base_url: openai_cfg.base_url.clone(),
        };

        let messages = vec![
            Message {
                role: "user".into(),
                content: "Say 'Goblin test OK' and nothing else.".into(),
                tool_calls: None,
                tool_call_id: None,
                reasoning: None,
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
    async fn real_deepseek_v4_flash_api_call() {
        let config = Config::load().expect("Failed to load config");
        let openai_cfg = config.providers.openai
            .expect("No openai provider in config");

        let provider = OpenAIProvider {
            api_key: openai_cfg.api_key.clone(),
            base_url: openai_cfg.base_url.clone(),
        };

        let messages = vec![
            Message {
                role: "user".into(),
                content: "Reply with just the number 42.".into(),
                tool_calls: None,
                tool_call_id: None,
                reasoning: None,
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
            messages,
            tools: if tools.is_empty() { None } else { Some(tools) },
            tool_choice: if tools.is_empty() { None } else { Some("auto") },
            stream: false,
            temperature: Some(0.0),
            max_tokens: Some(8192),
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

        Ok(ProviderResponse {
            content: choice.message.content,
            tool_calls: choice.message.tool_calls,
            tokens_in: usage.prompt_tokens,
            tokens_out: usage.completion_tokens,
            model: model.to_string(),
            reasoning: choice.message.reasoning_content,
        })
    }
}
