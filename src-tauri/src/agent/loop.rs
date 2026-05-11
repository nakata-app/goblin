use crate::config::Config;
use crate::provider::{Message, Provider, ToolDefinition};
use crate::tools::ToolRegistry;
use super::prompt;
use super::context::ContextWindow;

pub struct AgentLoop {
    pub config: Config,
    pub provider: Box<dyn Provider>,
    pub conversation: Vec<Message>,
    pub context_window: ContextWindow,
    pub tool_registry: ToolRegistry,
    pub max_tool_rounds: u32,
}

impl AgentLoop {
    pub fn new(config: Config, provider: Box<dyn Provider>, tool_registry: ToolRegistry) -> Self {
        let max_tokens = config.agent.max_tokens;
        Self {
            config,
            provider,
            conversation: Vec::new(),
            context_window: ContextWindow::new(max_tokens),
            tool_registry,
            max_tool_rounds: 10,
        }
    }

    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tool_registry.definitions()
    }

    pub async fn send_message(
        &mut self,
        user_input: &str,
        project_context: Option<&str>,
        memories: &[String],
        learned: &[String],
        model_override: Option<&str>,
    ) -> Result<AgentResponse, String> {
        let system_prompt = prompt::build_system_prompt(project_context, memories, learned);
        let mut messages = prompt::build_messages(&system_prompt, &self.conversation, user_input);

        let model = model_override.unwrap_or(self.config.default_model());
        let tools = self.tool_registry.definitions();

        let mut total_tokens_in = 0u32;
        let mut total_tokens_out = 0u32;
        let mut final_content = String::new();
        let mut all_tool_calls: Vec<crate::provider::ToolCall> = Vec::new();
        let mut observations: Vec<ToolObservation> = Vec::new();

        for round in 0..self.max_tool_rounds {
            let resp = self
                .provider
                .chat(&messages, &tools, model)
                .await?;

            total_tokens_in += resp.tokens_in;
            total_tokens_out += resp.tokens_out;

            let has_tool_calls = resp.tool_calls.as_ref().map(|tc| !tc.is_empty()).unwrap_or(false);

            if !has_tool_calls {
                final_content = resp.content.unwrap_or_default();
                break;
            }

            let tool_calls = resp.tool_calls.as_ref().unwrap();

            messages.push(Message {
                role: "assistant".to_string(),
                content: resp.content.unwrap_or_default(),
                tool_calls: Some(tool_calls.clone()),
                tool_call_id: None,
            });

            for tc in tool_calls {
                let args: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                    .unwrap_or(serde_json::Value::Null);

                let tool_result = self
                    .tool_registry
                    .execute(&tc.function.name, args)
                    .await;

                let result_text = match tool_result {
                    Ok(output) => output,
                    Err(e) => format!("Error: {}", e),
                };

                let success = !result_text.starts_with("Error:");
                observations.push(ToolObservation {
                    tool_name: tc.function.name.clone(),
                    args_summary: truncate_str(&tc.function.arguments, 200),
                    result_summary: truncate_str(&result_text, 200),
                    success,
                });

                all_tool_calls.push(crate::provider::ToolCall {
                    id: tc.id.clone(),
                    call_type: "function".to_string(),
                    function: crate::provider::ToolCallFunction {
                        name: tc.function.name.clone(),
                        arguments: tc.function.arguments.clone(),
                    },
                });

                messages.push(Message {
                    role: "tool".to_string(),
                    content: result_text,
                    tool_calls: None,
                    tool_call_id: Some(tc.id.clone()),
                });
            }

            if round == self.max_tool_rounds - 1 {
                final_content = format!(
                    "[Max tool rounds ({}) reached. The agent made {} tool calls.]",
                    self.max_tool_rounds, all_tool_calls.len()
                );
            }
        }

        if final_content.is_empty() && !all_tool_calls.is_empty() {
            final_content = format!(
                "[Agent made {} tool call(s). Results above.]",
                all_tool_calls.len()
            );
        }

        self.conversation.push(Message {
            role: "user".to_string(),
            content: user_input.to_string(),
            tool_calls: None,
            tool_call_id: None,
        });

        self.conversation.push(Message {
            role: "assistant".to_string(),
            content: final_content.clone(),
            tool_calls: if all_tool_calls.is_empty() { None } else { Some(all_tool_calls.clone()) },
            tool_call_id: None,
        });

        self.context_window.trim(&mut self.conversation);

        Ok(AgentResponse {
            content: final_content,
            tool_calls: if all_tool_calls.is_empty() { None } else { Some(all_tool_calls) },
            tokens_in: total_tokens_in,
            tokens_out: total_tokens_out,
            model: model.to_string(),
            observations,
        })
    }

    pub fn clear(&mut self) {
        self.conversation.clear();
    }

    pub fn set_conversation(&mut self, messages: Vec<crate::provider::Message>) {
        self.conversation = messages;
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolObservation {
    pub tool_name: String,
    pub args_summary: String,
    pub result_summary: String,
    pub success: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentResponse {
    pub content: String,
    pub tool_calls: Option<Vec<crate::provider::ToolCall>>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub model: String,
    pub observations: Vec<ToolObservation>,
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}
