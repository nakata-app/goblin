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
