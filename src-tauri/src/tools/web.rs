use crate::provider::ToolDefinition;
use serde_json::json;
use std::time::Duration;

pub fn web_fetch_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "web_fetch".into(),
            description: "Fetches content from a URL and returns it as text. Use for reading documentation, API responses, or any web page. Returns summarized if content is very large.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch"
                    },
                    "format": {
                        "type": "string",
                        "enum": ["text", "markdown", "html"],
                        "description": "Format to return content in (default: text)"
                    }
                },
                "required": ["url"]
            }),
        },
    }
}

pub fn web_search_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "web_search".into(),
            description: "Searches the web using DuckDuckGo and returns results. Use for finding current information, documentation, or anything you don't know.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query"
                    },
                    "maxResults": {
                        "type": "integer",
                        "description": "Maximum results (default 10, max 20)"
                    }
                },
                "required": ["query"]
            }),
        },
    }
}

pub async fn handle_web_fetch(args: serde_json::Value) -> Result<String, String> {
    let url = args["url"].as_str().ok_or("url required")?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("Mozilla/5.0 (compatible; Goblin/1.0)")
        .build()
        .map_err(|e| format!("Client build error: {}", e))?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {} for {}", status, url));
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if content_type.contains("application/json") {
        let text = resp.text().await.map_err(|e| format!("Read error: {}", e))?;
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
            return Ok(serde_json::to_string_pretty(&parsed).unwrap_or(text));
        }
        return Ok(text);
    }

    let body = resp.text().await.map_err(|e| format!("Read error: {}", e))?;

    if body.len() > 15000 {
        Ok(format!("{}...\n\n[content truncated at 15000 chars, total {} bytes]", &body[..15000], body.len()))
    } else {
        Ok(body)
    }
}

pub async fn handle_web_search(args: serde_json::Value) -> Result<String, String> {
    let query = args["query"].as_str().ok_or("query required")?;
    let max_results = args["maxResults"].as_u64().unwrap_or(10).min(20);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (compatible; Goblin/1.0)")
        .build()
        .map_err(|e| format!("Client build error: {}", e))?;

    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        urlencoding::encode(query)
    );

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Search request failed: {}", e))?;

    let body = resp.text().await.map_err(|e| format!("Read error: {}", e))?;

    let mut results: Vec<String> = Vec::new();

    for (title, snippet, link) in parse_ddg_results(&body) {
        if results.len() >= max_results as usize {
            break;
        }
        results.push(format!("- {} ({})\n  {}", title, link, snippet));
    }

    if results.is_empty() {
        Ok(format!("No results found for '{}'", query))
    } else {
        Ok(results.join("\n\n"))
    }
}

fn parse_ddg_results(html: &str) -> Vec<(String, String, String)> {
    let mut results = Vec::new();
    let mut current_title = String::new();
    let mut current_snippet = String::new();
    let mut current_link = String::new();

    let mut in_result = false;
    let mut in_snippet = false;

    for line in html.lines() {
        let line = line.trim();

        if line.contains("result__title") {
            in_result = true;
            current_title.clear();
            current_snippet.clear();
            current_link.clear();
        }

        if in_result {
            if line.contains("result__snippet") {
                in_snippet = true;
                continue;
            }

            if let Some(start) = line.find("href=\"") {
                let rest = &line[start + 6..];
                if let Some(end) = rest.find('"') {
                    current_link = rest[..end].to_string();
                }
            }

            if in_snippet {
                let clean = line
                    .replace("<b>", "")
                    .replace("</b>", "")
                    .replace("&amp;", "&")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .trim()
                    .to_string();
                if !clean.is_empty() && !clean.starts_with('<') {
                    current_snippet.push_str(&clean);
                    current_snippet.push(' ');
                }
            } else {
                let clean = line
                    .replace("result__title", "")
                    .replace("<b>", "")
                    .replace("</b>", "")
                    .replace("&amp;", "&")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("class=\"\"", "")
                    .trim()
                    .to_string();
                if !clean.is_empty() && !clean.starts_with('<') && !clean.starts_with("class") {
                    current_title.push_str(&clean);
                    current_title.push(' ');
                }
            }

            if line.contains("result__snippet") && in_snippet {
                in_snippet = false;
            }

            if line.contains("result--") || line.contains("</div>") && !current_title.trim().is_empty() {
                let title = current_title.trim().to_string();
                let snippet = current_snippet.trim().to_string();
                if !title.is_empty() {
                    results.push((title, snippet, current_link.clone()));
                }
                in_result = false;
                in_snippet = false;
            }
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ddg_empty_html() {
        let results = parse_ddg_results("");
        assert!(results.is_empty());
    }

    #[test]
    fn parse_ddg_no_results() {
        let html = "<html><body>no results here</body></html>";
        let results = parse_ddg_results(html);
        assert!(results.is_empty());
    }

    #[test]
    fn parse_ddg_single_result() {
        let html = r#"
result__title
Example Title
<a href="https://example.com">click</a>
result__snippet
This is a sample snippet
</div>"#;
        let results = parse_ddg_results(html);
        assert_eq!(results.len(), 1, "expected 1 result, got: {:?}", results);
        assert!(results[0].0.contains("Example Title"));
        assert!(results[0].1.contains("This is a sample snippet"));
        assert_eq!(results[0].2, "https://example.com");
    }

    #[test]
    fn parse_ddg_multiple_results() {
        let html = r#"
result__title
Title A
<a href="https://a.com">click</a>
result__snippet
Snippet A
</div>
result__title
Title B
<a href="https://b.com">click</a>
result__snippet
Snippet B
</div>"#;
        let results = parse_ddg_results(html);
        assert_eq!(results.len(), 2, "expected 2 results, got: {:?}", results);
        assert_eq!(results[0].0.trim(), "Title A");
        assert_eq!(results[1].0.trim(), "Title B");
    }

    #[test]
    fn parse_ddg_handles_html_entities() {
        let html = r#"
result__title
Test &amp; More &lt;code&gt;
<a href="https://x.com">click</a>
result__snippet
Result &amp; stuff
</div>"#;
        let results = parse_ddg_results(html);
        assert_eq!(results.len(), 1, "expected 1 result, got: {:?}", results);
        assert!(results[0].0.contains("Test & More"));
        assert!(!results[0].0.contains("&amp;"));
    }

    #[test]
    fn parse_ddg_strips_bold_tags() {
        let html = r#"
result__title
<b>Bold</b> Text
<a href="https://b.com">click</a>
result__snippet
Some <b>bold</b> snippet
</div>"#;
        let results = parse_ddg_results(html);
        assert_eq!(results.len(), 1);
        assert!(!results[0].0.contains("<b>"));
        assert!(!results[0].1.contains("<b>"));
    }

    #[test]
    fn web_fetch_def_check() {
        let def = web_fetch_def();
        assert_eq!(def.function.name, "web_fetch");
        assert!(def.function.parameters["required"][0].as_str().unwrap() == "url");
    }

    #[test]
    fn web_search_def_check() {
        let def = web_search_def();
        assert_eq!(def.function.name, "web_search");
        assert!(def.function.parameters["required"][0].as_str().unwrap() == "query");
    }
}
