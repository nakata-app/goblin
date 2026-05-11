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
