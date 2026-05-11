use crate::config::Config;
use crate::provider::{Message, Provider, ProviderResponse, ToolDefinition};
use super::prompt;
use super::context::ContextWindow;

pub struct AgentLoop {
    pub config: Config,
    pub provider: Box<dyn Provider>,
    pub conversation: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub context_window: ContextWindow,
}

impl AgentLoop {
    pub fn new(config: Config, provider: Box<dyn Provider>) -> Self {
        let max_tokens = config.agent.max_tokens;
        Self {
            config,
            provider,
            conversation: Vec::new(),
            tools: Vec::new(),
            context_window: ContextWindow::new(max_tokens),
        }
    }

    pub fn set_tools(&mut self, tools: Vec<ToolDefinition>) {
        self.tools = tools;
    }

    pub async fn send_message(
        &mut self,
        user_input: &str,
        project_context: Option<&str>,
        memories: &[String],
        learned: &[String],
    ) -> Result<AgentResponse, String> {
        let system_prompt = prompt::build_system_prompt(project_context, memories, learned);
        let messages = prompt::build_messages(&system_prompt, &self.conversation, user_input);

        let model = self.config.default_model().to_string();

        let resp = self
            .provider
            .chat(&messages, &self.tools, &model)
            .await?;

        self.conversation.push(Message {
            role: "user".to_string(),
            content: user_input.to_string(),
            tool_calls: None,
            tool_call_id: None,
        });

        self.conversation.push(Message {
            role: "assistant".to_string(),
            content: resp.content.clone().unwrap_or_default(),
            tool_calls: resp.tool_calls.clone(),
            tool_call_id: None,
        });

        self.context_window.trim(&mut self.conversation);

        Ok(AgentResponse {
            content: resp.content.unwrap_or_default(),
            tool_calls: resp.tool_calls,
            tokens_in: resp.tokens_in,
            tokens_out: resp.tokens_out,
            model: resp.model,
        })
    }

    pub fn clear(&mut self) {
        self.conversation.clear();
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentResponse {
    pub content: String,
    pub tool_calls: Option<Vec<crate::provider::ToolCall>>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub model: String,
}
