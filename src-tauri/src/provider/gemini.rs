use super::{Message, Provider, ProviderResponse, ToolDefinition, ToolCall};
use serde::{Deserialize, Serialize};

pub struct GeminiProvider {
    pub api_key: String,
    pub base_url: String,
}

#[derive(Serialize)]
struct GeminiRequest<'a> {
    contents: Vec<GeminiContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<GeminiToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction<'a>>,
    generation_config: GeminiGenerationConfig,
}

#[derive(Serialize)]
struct GeminiContent<'a> {
    role: &'a str,
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum GeminiPart<'a> {
    Text { text: &'a str },
    FunctionCall {
        function_call: GeminiFunctionCall,
    },
    FunctionResponse {
        function_response: GeminiFunctionResponse<'a>,
    },
}

#[derive(Serialize)]
struct GeminiFunctionCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Serialize)]
struct GeminiFunctionResponse<'a> {
    name: String,
    response: GeminiResponseContent<'a>,
}

#[derive(Serialize)]
struct GeminiResponseContent<'a> {
    content: &'a str,
}

#[derive(Serialize)]
struct GeminiSystemInstruction<'a> {
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Serialize)]
struct GeminiTool {
    function_declarations: Vec<GeminiFunctionDecl>,
}

#[derive(Serialize)]
struct GeminiFunctionDecl {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize)]
struct GeminiToolConfig {
    function_calling_config: GeminiFunctionCallingConfig,
}

#[derive(Serialize)]
struct GeminiFunctionCallingConfig {
    mode: String,
}

#[derive(Serialize)]
struct GeminiGenerationConfig {
    temperature: f32,
    max_output_tokens: u32,
}

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    usage_metadata: Option<GeminiUsage>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: GeminiResponseContent_,
}

#[derive(Deserialize)]
struct GeminiResponseContent_ {
    parts: Vec<GeminiResponsePart>,
    #[serde(default)]
    role: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GeminiResponsePart {
    Text { text: String },
    FunctionCall {
        #[serde(default)]
        function_call: Option<GeminiFunctionCall_>,
    },
}

#[derive(Deserialize)]
struct GeminiFunctionCall_ {
    name: String,
    args: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct GeminiUsage {
    prompt_token_count: u32,
    candidates_token_count: u32,
}

#[async_trait::async_trait]
impl Provider for GeminiProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
    ) -> Result<ProviderResponse, String> {
        let url = format!(
            "{}/models/{}:generateContent?key={}",
            self.base_url.trim_end_matches('/'),
            model,
            self.api_key
        );

        let mut contents = Vec::new();
        let mut system_instruction = None;

        for msg in messages {
            if msg.role == "system" {
                system_instruction = Some(GeminiSystemInstruction {
                    parts: vec![GeminiPart::Text {
                        text: &msg.content,
                    }],
                });
                continue;
            }

            let role = match msg.role.as_str() {
                "user" => "user",
                "assistant" => "model",
                "tool" => "function",
                _ => "user",
            };

            let parts: Vec<GeminiPart> = if msg.role == "tool" {
                let name = msg.tool_call_id.as_deref().unwrap_or("unknown");
                vec![GeminiPart::FunctionResponse {
                    function_response: GeminiFunctionResponse {
                        name: name.to_string(),
                        response: GeminiResponseContent {
                            content: &msg.content,
                        },
                    },
                }]
            } else if let Some(ref tcs) = msg.tool_calls {
                tcs.iter()
                    .map(|tc| {
                        let args: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null);
                        GeminiPart::FunctionCall {
                            function_call: GeminiFunctionCall {
                                name: tc.function.name.clone(),
                                args,
                            },
                        }
                    })
                    .collect()
            } else {
                vec![GeminiPart::Text {
                    text: &msg.content,
                }]
            };

            contents.push(GeminiContent { role, parts });
        }

        let gemini_tools: Option<Vec<GeminiTool>> = if tools.is_empty() {
            None
        } else {
            Some(vec![GeminiTool {
                function_declarations: tools
                    .iter()
                    .map(|t| GeminiFunctionDecl {
                        name: t.function.name.clone(),
                        description: t.function.description.clone(),
                        parameters: t.function.parameters.clone(),
                    })
                    .collect(),
            }])
        };

        let tool_config = if tools.is_empty() {
            None
        } else {
            Some(GeminiToolConfig {
                function_calling_config: GeminiFunctionCallingConfig {
                    mode: "AUTO".to_string(),
                },
            })
        };

        let body = GeminiRequest {
            contents,
            tools: gemini_tools,
            tool_config,
            system_instruction,
            generation_config: GeminiGenerationConfig {
                temperature: 0.0,
                max_output_tokens: 8192,
            },
        };

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(format!("Gemini API error {}: {}", status, body_text));
        }

        let response: GeminiResponse = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse Gemini response: {}", e))?;

        let candidate = response
            .candidates
            .into_iter()
            .next()
            .ok_or_else(|| "No candidates in response".to_string())?;

        let mut content_text = String::new();
        let mut tool_calls = Vec::new();

        for part in &candidate.content.parts {
            match part {
                GeminiResponsePart::Text { text } => {
                    content_text.push_str(text);
                }
                GeminiResponsePart::FunctionCall { function_call } => {
                    if let Some(fc) = function_call {
                        tool_calls.push(ToolCall {
                            id: format!("call_{}", tool_calls.len()),
                            call_type: "function".to_string(),
                            function: super::ToolCallFunction {
                                name: fc.name.clone(),
                                arguments: serde_json::to_string(&fc.args)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            },
                        });
                    }
                }
            }
        }

        let usage = response.usage_metadata.unwrap_or(GeminiUsage {
            prompt_token_count: 0,
            candidates_token_count: 0,
        });

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
            tokens_in: usage.prompt_token_count,
            tokens_out: usage.candidates_token_count,
            model: model.to_string(),
        })
    }
}
