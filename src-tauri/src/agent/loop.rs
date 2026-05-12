use crate::config::Config;
use crate::provider::{Message, Provider, ToolDefinition};
use crate::tools::ToolRegistry;
use super::prompt;
use super::context::ContextWindow;
use std::collections::HashMap;
use tokio::sync::mpsc;

pub struct AgentLoop {
    pub config: Config,
    pub provider: Box<dyn Provider>,
    pub conversation: Vec<Message>,
    pub context_window: ContextWindow,
    pub tool_registry: ToolRegistry,
    pub max_tool_rounds: u32,
}

struct LoopGuard {
    exact_failure_counts: HashMap<(String, String), u32>,
    same_tool_failure_counts: HashMap<String, u32>,
    last_tool_results: HashMap<String, String>,
    idempotent_no_progress_counts: HashMap<String, u32>,
}

impl LoopGuard {
    fn new() -> Self {
        Self {
            exact_failure_counts: HashMap::new(),
            same_tool_failure_counts: HashMap::new(),
            last_tool_results: HashMap::new(),
            idempotent_no_progress_counts: HashMap::new(),
        }
    }

    fn check(
        &mut self,
        tool_name: &str,
        args: &str,
        result_text: &str,
        success: bool,
        round: u32,
    ) -> Option<String> {
        if success {
            self.exact_failure_counts.remove(&(tool_name.to_string(), args.to_string()));
            self.same_tool_failure_counts.remove(tool_name);
            self.last_tool_results.insert(tool_name.to_string(), result_text.to_string());
            self.idempotent_no_progress_counts.remove(tool_name);
            return None;
        }

        // Exact failure: same tool + same args failing
        let exact_key = (tool_name.to_string(), args.to_string());
        let exact_count = self.exact_failure_counts.entry(exact_key).or_insert(0);
        *exact_count += 1;

        if *exact_count == 2 {
            eprintln!(
                "[loop-guard] WARN: exact_failure '{}' with same args (round {}). Count={}",
                tool_name, round, exact_count
            );
        }
        if *exact_count >= 5 {
            return Some(format!(
                "Guard: exact_failure — '{}' failed {} times with identical arguments. Hard stop.",
                tool_name, exact_count
            ));
        }

        // Same tool failure (regardless of args)
        let tool_count = self.same_tool_failure_counts.entry(tool_name.to_string()).or_insert(0);
        *tool_count += 1;

        if *tool_count >= 4 {
            return Some(format!(
                "Guard: same_tool_failure — '{}' failed {} times in a row. Hard stop.",
                tool_name, tool_count
            ));
        }

        // Idempotent no progress: same tool returning identical result
        if let Some(prev_result) = self.last_tool_results.get(tool_name) {
            if prev_result == result_text {
                let nn = self.idempotent_no_progress_counts.entry(tool_name.to_string()).or_insert(0);
                *nn += 1;
                eprintln!(
                    "[loop-guard] WARN: idempotent_no_progress '{}' (round {}). Identical result returned {} times.",
                    tool_name, round, nn
                );
                if *nn >= 3 {
                    return Some(format!(
                        "Guard: idempotent_no_progress — '{}' returned identical result {} times. No progress. Hard stop.",
                        tool_name, nn
                    ));
                }
            }
        }
        self.last_tool_results.insert(tool_name.to_string(), result_text.to_string());

        None
    }
}

impl AgentLoop {
    pub fn new(config: Config, provider: Box<dyn Provider>, tool_registry: ToolRegistry) -> Self {
        let max_tokens = config.agent.max_tokens;
        let cw = ContextWindow::with_config(
            max_tokens,
            config.agent.context_protect_last_n,
            config.agent.context_hard_limit,
            config.agent.context_target_ratio,
        );
        Self {
            config,
            provider,
            conversation: Vec::new(),
            context_window: cw,
            tool_registry,
            max_tool_rounds: 10,
        }
    }

    #[allow(dead_code)]
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
        progress: Option<mpsc::UnboundedSender<serde_json::Value>>,
    ) -> Result<AgentResponse, String> {
        let system_prompt = prompt::build_system_prompt(project_context, memories, learned);
        let mut messages = prompt::build_messages(&system_prompt, &self.conversation, user_input);
        let pre_loop_len = messages.len();

        let model = model_override.unwrap_or(self.config.default_model());
        let tools = self.tool_registry.definitions();

        let mut total_tokens_in = 0u32;
        let mut total_tokens_out = 0u32;
        let mut final_content = String::new();
        let mut all_tool_calls: Vec<crate::provider::ToolCall> = Vec::new();
        let mut observations: Vec<ToolObservation> = Vec::new();
        let mut all_reasoning: Vec<String> = Vec::new();
        let mut loop_guard = LoopGuard::new();
        let mut guard_triggered = false;

        for round in 0..self.max_tool_rounds {
            if let Some(ref tx) = progress {
                let _ = tx.send(serde_json::json!({
                    "type": "round",
                    "round": round + 1,
                    "max": self.max_tool_rounds,
                }));
            }

            let resp = self
                .provider
                .chat(&messages, &tools, model)
                .await?;

            total_tokens_in += resp.tokens_in;
            total_tokens_out += resp.tokens_out;

            if let Some(ref r) = resp.reasoning {
                if !r.is_empty() {
                    all_reasoning.push(r.clone());
                }
            }

            let has_tool_calls = resp.tool_calls.as_ref().map(|tc| !tc.is_empty()).unwrap_or(false);

            if !has_tool_calls {
                final_content = resp.content.unwrap_or_default();
                break;
            }

            let tool_calls = resp.tool_calls.as_ref().unwrap();

            let reasoning = resp.reasoning.clone();

            messages.push(Message {
                role: "assistant".to_string(),
                content: resp.content.unwrap_or_default(),
                tool_calls: Some(tool_calls.clone()),
                tool_call_id: None,
                reasoning,
            });

            for tc in tool_calls {
                let args: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                    .unwrap_or(serde_json::Value::Null);

                if let Some(ref tx) = progress {
                    let _ = tx.send(serde_json::json!({
                        "type": "tool_start",
                        "tool": tc.function.name,
                        "args": truncate_str(&tc.function.arguments, 100),
                    }));
                }

                let tool_result = self
                    .tool_registry
                    .execute(&tc.function.name, args)
                    .await;

                let result_text = match tool_result {
                    Ok(output) => crate::tools::compactor::compact(&tc.function.name, &output),
                    Err(e) => format!("Error: {}", e),
                };

                let success = !result_text.starts_with("Error:");

                if let Some(ref tx) = progress {
                    let _ = tx.send(serde_json::json!({
                        "type": "tool_end",
                        "tool": tc.function.name,
                        "success": success,
                        "summary": truncate_str(&result_text, 150),
                    }));
                }
                observations.push(ToolObservation {
                    tool_name: tc.function.name.clone(),
                    args_summary: truncate_str(&tc.function.arguments, 200),
                    result_summary: truncate_str(&result_text, 200),
                    success,
                });

                // Loop guardrail check
                if let Some(guard_msg) = loop_guard.check(
                    &tc.function.name,
                    &tc.function.arguments,
                    &result_text,
                    success,
                    round,
                ) {
                    final_content = format!(
                        "{}\n\n{}",
                        result_text,
                        guard_msg
                    );
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
                        reasoning: None,
                    });
                    guard_triggered = true;
                    break;
                }

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
                    reasoning: None,
                });
            }

            if guard_triggered {
                break;
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
            reasoning: None,
        });

        // Copy all intermediate loop messages (assistant tool_calls + tool results)
        for msg in &messages[pre_loop_len..] {
            self.conversation.push(msg.clone());
        }

        // Add final assistant response without tool_calls
        self.conversation.push(Message {
            role: "assistant".to_string(),
            content: final_content.clone(),
            tool_calls: None,
            tool_call_id: None,
            reasoning: None,
        });

        self.context_window.trim(&mut self.conversation);

        Ok(AgentResponse {
            content: final_content,
            tool_calls: if all_tool_calls.is_empty() { None } else { Some(all_tool_calls) },
            tokens_in: total_tokens_in,
            tokens_out: total_tokens_out,
            model: model.to_string(),
            observations,
            reasoning: if all_reasoning.is_empty() { None } else { Some(all_reasoning.join("\n\n")) },
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
    pub reasoning: Option<String>,
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}
