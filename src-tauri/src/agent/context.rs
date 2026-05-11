use crate::provider::Message;

pub struct ContextWindow {
    max_tokens: u32,
}

impl ContextWindow {
    pub fn new(max_tokens: u32) -> Self {
        Self { max_tokens }
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

    pub fn fits(&self, messages: &[Message]) -> bool {
        Self::estimate_tokens(messages) < self.max_tokens
    }

    pub fn trim(&self, messages: &mut Vec<Message>) {
        while !self.fits(messages) && messages.len() > 3 {
            let sys = messages.remove(0);
            messages.remove(0);
            messages.insert(0, sys);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    fn msg(content: &str) -> Message {
        Message { role: "user".to_string(), content: content.to_string(), tool_calls: None, tool_call_id: None }
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
            Message { role: "system".into(), content: "sys".into(), tool_calls: None, tool_call_id: None },
            msg("xxxxx yyyyy zzzzz aaaaa bbbbb ccccc ddddd eeeee fffff ggggg"),
        ];
        cw.trim(&mut msgs);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
    }

    #[test]
    fn trim_removes_pairs() {
        let cw = ContextWindow::new(50);
        let big = "x".repeat(200);
        let mut msgs = vec![
            Message { role: "system".into(), content: "sys".into(), tool_calls: None, tool_call_id: None },
            Message { role: "user".into(), content: big.clone(), tool_calls: None, tool_call_id: None },
            Message { role: "assistant".into(), content: big.clone(), tool_calls: None, tool_call_id: None },
            Message { role: "user".into(), content: big.clone(), tool_calls: None, tool_call_id: None },
        ];
        cw.trim(&mut msgs);
        // should have system + 2 remaining pairs (the last user+assistant)
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "system");
    }
}
