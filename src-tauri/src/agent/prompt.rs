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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    #[test]
    fn system_prompt_base_only() {
        let prompt = build_system_prompt(None, &[], &[]);
        assert!(prompt.contains("Goblin"));
        assert!(prompt.contains("Tauri app"));
        assert!(!prompt.contains("Project Context"));
        assert!(!prompt.contains("Relevant Memories"));
        assert!(!prompt.contains("User Preferences"));
    }

    #[test]
    fn system_prompt_with_project_context() {
        let prompt = build_system_prompt(Some("My Project v2"), &[], &[]);
        assert!(prompt.contains("Project Context"));
        assert!(prompt.contains("My Project v2"));
    }

    #[test]
    fn system_prompt_with_memories() {
        let mems = vec!["User prefers Rust".to_string(), "Use dark theme".to_string()];
        let prompt = build_system_prompt(None, &mems, &[]);
        assert!(prompt.contains("Relevant Memories"));
        assert!(prompt.contains("User prefers Rust"));
        assert!(prompt.contains("Use dark theme"));
    }

    #[test]
    fn system_prompt_with_learned() {
        let learned = vec!["Avoid npm".to_string()];
        let prompt = build_system_prompt(None, &[], &learned);
        assert!(prompt.contains("User Preferences"));
        assert!(prompt.contains("Avoid npm"));
    }

    #[test]
    fn system_prompt_all_fields() {
        let mems = vec!["m1".to_string()];
        let learned = vec!["l1".to_string()];
        let prompt = build_system_prompt(Some("ctx"), &mems, &learned);
        assert!(prompt.contains("Project Context"));
        assert!(prompt.contains("Relevant Memories"));
        assert!(prompt.contains("User Preferences"));
    }

    #[test]
    fn build_messages_structure() {
        let existing = vec![
            Message { role: "user".into(), content: "previous".into(), tool_calls: None, tool_call_id: None },
        ];
        let result = build_messages("sys-prompt", &existing, "new message");
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].role, "system");
        assert_eq!(result[0].content, "sys-prompt");
        assert_eq!(result[1].role, "user");
        assert_eq!(result[1].content, "previous");
        assert_eq!(result[2].role, "user");
        assert_eq!(result[2].content, "new message");
    }

    #[test]
    fn build_messages_no_existing() {
        let result = build_messages("sys", &[], "hello");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "system");
        assert_eq!(result[1].role, "user");
    }
}
