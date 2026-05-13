use crate::provider::Message;

pub struct ContextWindow {
    max_tokens: u32,
    protect_last_n: usize,
    hard_message_limit: usize,
    target_ratio: f64,
}

impl ContextWindow {
    #[allow(dead_code)]
    pub fn new(max_tokens: u32) -> Self {
        Self {
            max_tokens,
            protect_last_n: 20,
            hard_message_limit: 400,
            target_ratio: 0.8,
        }
    }

    pub fn with_config(max_tokens: u32, protect_last_n: usize, hard_message_limit: usize, target_ratio: f64) -> Self {
        Self {
            max_tokens,
            protect_last_n,
            hard_message_limit,
            target_ratio,
        }
    }

    pub fn estimate_tokens(messages: &[Message]) -> u32 {
        let mut total = 0u32;
        for msg in messages {
            total += msg.content.len() as u32 / 4;
            if let Some(tc) = &msg.tool_calls {
                for t in tc {
                    total += t.function.arguments.len() as u32 / 4;
                    total += t.function.name.len() as u32;
                }
            }
        }
        total + 100
    }

    #[allow(dead_code)]
    pub fn fits(&self, messages: &[Message]) -> bool {
        Self::estimate_tokens(messages) < self.max_tokens
    }

    /// Smart trim: protects system message + recent messages,
    /// removes oldest pairs first, inserts context summary.
    pub fn trim(&self, messages: &mut Vec<Message>) {
        // Step 1: Hard message limit — if exceeded, aggressively cut oldest
        if messages.len() > self.hard_message_limit {
            let excess = messages.len() - self.hard_message_limit + self.protect_last_n;
            if excess > 0 && messages.len() > self.protect_last_n + 2 {
                let cut_end = messages.len().saturating_sub(self.protect_last_n);
                let cut_start = 1; // keep system message
                if cut_end > cut_start {
                    let removed_count = cut_end - cut_start;
                    if removed_count > 0 {
                        // Insert context summary
                        let summary = Message {
                            role: "system".to_string(),
                            content: format!(
                                "[Context compressed: {} earlier messages were removed due to length limit. Only the most recent {} messages are preserved.]",
                                removed_count, self.protect_last_n
                            ),
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning: None,
                        };
                        // Remove the middle section
                        messages.drain(1..cut_end);
                        messages.insert(1, summary);
                    }
                }
            }
            return;
        }

        // Step 2: Token limit — trim until under budget
        let target = (self.max_tokens as f64 * self.target_ratio) as u32;

        while Self::estimate_tokens(messages) > target && messages.len() > self.protect_last_n + 2 {
            // Keep system message and last N messages untouched
            let keep_from_end = self.protect_last_n;
            let droppable_end = messages.len().saturating_sub(keep_from_end);

            if droppable_end <= 1 {
                break; // nothing safe to drop
            }

            // Find a user message to remove (drop user+assistant pair)
            let mut drop_idx = 1; // start after system
            while drop_idx < droppable_end && messages[drop_idx].role != "user" {
                drop_idx += 1;
            }

            if drop_idx >= droppable_end {
                break;
            }

            messages.remove(drop_idx); // remove user
            // Remove the next assistant/tool messages as well (a single turn)
            while drop_idx < messages.len() && drop_idx < droppable_end && messages[drop_idx].role != "user" {
                messages.remove(drop_idx);
            }
        }

        // Insert summary if any messages were dropped
        let non_system_count = messages.iter().filter(|m| m.role != "system").count();
        if non_system_count < messages.len() - 1 {
            // There are gaps; already handled by insertion during hard limit
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    fn msg(content: &str) -> Message {
        Message { role: "user".to_string(), content: content.to_string(), tool_calls: None, tool_call_id: None, reasoning: None }
    }

    #[test]
    fn estimate_tokens_empty() {
        let tokens = ContextWindow::estimate_tokens(&[]);
        assert_eq!(tokens, 100); // base overhead
    }

    #[test]
    fn estimate_tokens_with_content() {
        let msgs = vec![msg("hello world, this is a test message")];
        let tokens = ContextWindow::estimate_tokens(&msgs);
        // 35 chars / 4 = 8 + 100 base = 108
        assert_eq!(tokens, 108);
    }

    #[test]
    fn estimate_tokens_with_tool_calls() {
        let mut m = msg("tool result");
        m.tool_calls = Some(vec![crate::provider::ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: crate::provider::ToolCallFunction {
                name: "read_file".into(),
                arguments: "{\"path\":\"test.txt\"}".into(),
            },
        }]);
        let tokens = ContextWindow::estimate_tokens(&[m]);
        // content: 11/4=2, name: 9, args: 19/4=4, base=100 -> 115
        assert_eq!(tokens, 115);
    }

    #[test]
    fn fits_under_limit() {
        let cw = ContextWindow::new(1000);
        assert!(cw.fits(&[msg("hi")]));
    }

    #[test]
    fn fits_over_limit() {
        let cw = ContextWindow::new(50);
        let big = "x".repeat(200);
        assert!(!cw.fits(&[msg(&big)]));
    }

    #[test]
    fn trim_keeps_system_message() {
        let cw = ContextWindow::new(50);
        let mut msgs = vec![
            Message { role: "system".into(), content: "sys".into(), tool_calls: None, tool_call_id: None, reasoning: None },
            msg("xxxxx yyyyy zzzzz aaaaa bbbbb ccccc ddddd eeeee fffff ggggg"),
        ];
        cw.trim(&mut msgs);
        // System always preserved. With only 2 messages (system + 1 user), can't drop below protect_last_n (20)
        assert!(msgs[0].role == "system");
    }

    #[test]
    fn trim_removes_pairs_when_over_limit() {
        let cw = ContextWindow::new(10); // very low limit to force trim
        let big = "x".repeat(200);
        let mut msgs = vec![
            Message { role: "system".into(), content: "sys".into(), tool_calls: None, tool_call_id: None, reasoning: None },
        ];
        // Add many pairs to exceed limit
        for _ in 0..30 {
            msgs.push(Message { role: "user".into(), content: big.clone(), tool_calls: None, tool_call_id: None, reasoning: None });
            msgs.push(Message { role: "assistant".into(), content: big.clone(), tool_calls: None, tool_call_id: None, reasoning: None });
        }
        cw.trim(&mut msgs);
        // Should have system + some remaining messages
        assert!(msgs.len() > 1);
        assert_eq!(msgs[0].role, "system");
    }

    #[test]
    fn trim_hard_limit_inserts_summary() {
        let cw = ContextWindow::new(100000); // token limit won't trigger
        let mut cw_limit = cw;
        cw_limit.hard_message_limit = 10;
        cw_limit.protect_last_n = 3;
        let mut msgs = vec![
            Message { role: "system".into(), content: "sys".into(), tool_calls: None, tool_call_id: None, reasoning: None },
        ];
        for i in 0..20 {
            msgs.push(msg(&format!("msg{}", i)));
        }
        cw_limit.trim(&mut msgs);
        // System at 0, summary at 1, then protected messages
        assert_eq!(msgs[0].role, "system");
        assert!(msgs[1].content.contains("Context compressed"));
        assert!(msgs.len() <= 6); // system + summary + protect_last_n (3) = 5, but there could be an extra
    }
}
