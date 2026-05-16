/// Context type inferred from the last user message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryContext {
    Code,
    Creative,
    Analysis,
    Factual,
    Chat,
}

/// Sampling parameters selected by AutoTune.
#[derive(Debug, Clone, Copy)]
pub struct TunedParams {
    pub temperature: f32,
    pub top_p: f32,
}

/// Classify the last user message and return optimal sampling parameters.
pub fn autotune(last_user_message: &str) -> TunedParams {
    let ctx = classify(last_user_message);
    params_for(ctx)
}

fn classify(text: &str) -> QueryContext {
    let lower = text.to_ascii_lowercase();

    let code_hits = [
        "fn ",
        "def ",
        "impl ",
        "class ",
        "function",
        "struct ",
        "implement",
        "refactor",
        "fix the bug",
        "compile",
        "debug",
        "write code",
        "write a function",
        "write a test",
    ]
    .iter()
    .filter(|kw| lower.contains(*kw))
    .count();

    let creative_hits = [
        "story",
        "poem",
        "creative",
        "imagine",
        "brainstorm",
        "write a scene",
        "write a letter",
        "invent",
        "fiction",
    ]
    .iter()
    .filter(|kw| lower.contains(*kw))
    .count();

    let analysis_hits = [
        "explain",
        "analyze",
        "analyse",
        "compare",
        "why does",
        "how does",
        "summarize",
        "summarise",
        "describe",
        "pros and cons",
    ]
    .iter()
    .filter(|kw| lower.contains(*kw))
    .count();

    let factual_hits = [
        "what is",
        "what are",
        "when did",
        "who is",
        "where is",
        "list all",
        "enumerate",
        "how many",
        "give me a list",
    ]
    .iter()
    .filter(|kw| lower.contains(*kw))
    .count();

    // Highest hit-count wins; ties fall through in priority order.
    let max = code_hits
        .max(creative_hits)
        .max(analysis_hits)
        .max(factual_hits);
    if max == 0 {
        return QueryContext::Chat;
    }
    if code_hits == max {
        QueryContext::Code
    } else if creative_hits == max {
        QueryContext::Creative
    } else if analysis_hits == max {
        QueryContext::Analysis
    } else {
        QueryContext::Factual
    }
}

fn params_for(ctx: QueryContext) -> TunedParams {
    match ctx {
        QueryContext::Code => TunedParams {
            temperature: 0.2,
            top_p: 0.90,
        },
        QueryContext::Creative => TunedParams {
            temperature: 0.9,
            top_p: 0.95,
        },
        QueryContext::Analysis => TunedParams {
            temperature: 0.3,
            top_p: 0.90,
        },
        QueryContext::Factual => TunedParams {
            temperature: 0.1,
            top_p: 0.85,
        },
        QueryContext::Chat => TunedParams {
            temperature: 0.7,
            top_p: 0.90,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_query() {
        let p = autotune("implement a binary search fn in Rust");
        assert_eq!(p.temperature, 0.2);
    }

    #[test]
    fn creative_query() {
        let p = autotune("write a short story about a robot");
        assert_eq!(p.temperature, 0.9);
    }

    #[test]
    fn analysis_query() {
        let p = autotune("explain how the borrow checker works");
        assert_eq!(p.temperature, 0.3);
    }

    #[test]
    fn factual_query() {
        let p = autotune("what is the capital of France");
        assert_eq!(p.temperature, 0.1);
    }

    #[test]
    fn chat_default() {
        let p = autotune("hey, how are you?");
        assert_eq!(p.temperature, 0.7);
    }
}
