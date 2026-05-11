use crate::provider::Message;

pub fn build_system_prompt(
    project_context: Option<&str>,
    memories: &[String],
    learned: &[String],
) -> String {
    let mut parts: Vec<String> = Vec::new();

    parts.push("You are Goblin, a desktop AI agent. You run inside a Tauri app on the user's machine.".to_string());
    parts.push("You have access to tools (file system, shell, web, etc.) to help the user.".to_string());
    parts.push("Be concise, direct, and helpful. Communicate in the user's language.".to_string());

    if let Some(ctx) = project_context {
        parts.push(format!("\n## Project Context\n{}", ctx));
    }

    if !memories.is_empty() {
        let mem_block: String = memories
            .iter()
            .enumerate()
            .map(|(i, m)| format!("{}. {}", i + 1, m))
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!("\n## Relevant Memories\n{}", mem_block));
    }

    if !learned.is_empty() {
        let learn_block: String = learned
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{}. {}", i + 1, l))
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!("\n## User Preferences (Learned)\n{}", learn_block));
    }

    parts.join("\n")
}

pub fn build_messages(
    system_prompt: &str,
    conversation: &[Message],
    new_user_message: &str,
) -> Vec<Message> {
    let mut messages = Vec::with_capacity(conversation.len() + 2);

    messages.push(Message {
        role: "system".to_string(),
        content: system_prompt.to_string(),
        tool_calls: None,
        tool_call_id: None,
    });

    messages.extend(conversation.iter().cloned());

    messages.push(Message {
        role: "user".to_string(),
        content: new_user_message.to_string(),
        tool_calls: None,
        tool_call_id: None,
    });

    messages
}
