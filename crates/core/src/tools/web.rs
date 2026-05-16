//! Web tools — HTTP fetch and search.
//!
//! `WebFetch` pulls a URL and strips HTML to plain text for the model.
//! `WebSearch` uses Tavily when `TAVILY_API_KEY` is set and falls back
//! to scraping DuckDuckGo's HTML endpoint otherwise. Both keep their
//! dependencies to `reqwest` + `regex` so we don't ship a full HTML
//! parser in core.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

// ---------------------------------------------------------------------
// WebFetch — HTTP GET → stripped text for the model
// ---------------------------------------------------------------------

/// Reject URLs that could be used for SSRF:
/// - non-http(s) schemes
/// - loopback / link-local / private IP ranges
/// - cloud metadata endpoints (169.254.169.254)
fn validate_web_url(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err(format!("only http/https URLs are allowed (got `{url}`)"));
    }
    // Extract host: strip scheme, optional user@, optional :port, path
    let after_slash = url.split("//").nth(1).unwrap_or("");
    let host_port = after_slash.split('/').next().unwrap_or("");
    let host_port = host_port.split('@').next_back().unwrap_or(""); // strip user@
    let host = if host_port.starts_with('[') {
        // IPv6 literal: [::1] or [::1]:port
        host_port
            .split(']')
            .next()
            .unwrap_or("")
            .trim_start_matches('[')
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    let host_lower = host.to_ascii_lowercase();

    // Loopback & unspecified
    if matches!(
        host_lower.as_str(),
        "localhost" | "127.0.0.1" | "::1" | "0.0.0.0" | ""
    ) {
        return Err(format!("blocked: `{host}` is a loopback/internal address"));
    }
    // IPv6 loopback range
    if host_lower == "::1" || host_lower.starts_with("::ffff:127.") {
        return Err(format!("blocked: `{host}` is a loopback address"));
    }
    // Link-local (AWS IMDS, Azure, GCP metadata)
    if host_lower.starts_with("169.254.") {
        return Err(format!(
            "blocked: `{host}` is a link-local/cloud-metadata address"
        ));
    }
    // Private ranges: 10.x, 192.168.x
    if host_lower.starts_with("10.") || host_lower.starts_with("192.168.") {
        return Err(format!("blocked: `{host}` is a private network address"));
    }
    // Private range: 172.16–31.x
    if host_lower.starts_with("172.") {
        if let Some(second) = host_lower.split('.').nth(1) {
            if let Ok(n) = second.parse::<u8>() {
                if (16..=31).contains(&n) {
                    return Err(format!("blocked: `{host}` is a private network address"));
                }
            }
        }
    }
    // 127.x.x.x class-A loopback
    if host_lower.starts_with("127.") {
        return Err(format!("blocked: `{host}` is a loopback address"));
    }
    Ok(())
}

pub struct WebFetch;

#[derive(Debug, Deserialize)]
struct WebFetchArgs {
    url: String,
    /// Maximum bytes to return. Defaults to 48000 (same cap as read_file).
    #[serde(default)]
    max_bytes: Option<usize>,
}

/// Rough HTML → text: strip tags, collapse whitespace, decode common
/// entities. Good enough for the model to read; no heavy dependency.
pub(super) fn strip_html(html: &str) -> String {
    // Remove <script> and <style> blocks entirely
    let re_script = regex::Regex::new(r"(?is)<script[^>]*>.*?</script>").unwrap();
    let no_script = re_script.replace_all(html, "");
    let re_style = regex::Regex::new(r"(?is)<style[^>]*>.*?</style>").unwrap();
    let no_script = re_style.replace_all(&no_script, "");

    // Replace <br>, <p>, <div>, <li>, <tr>, <h1-6> with newlines
    let re_block = regex::Regex::new(r"(?i)<(br|/p|/div|/li|/tr|/h[1-6])[^>]*>").unwrap();
    let with_nl = re_block.replace_all(&no_script, "\n");

    // Strip remaining tags
    let re_tags = regex::Regex::new(r"<[^>]+>").unwrap();
    let text = re_tags.replace_all(&with_nl, "");

    // Decode common HTML entities
    let text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");

    // Collapse runs of whitespace / blank lines
    let re_blank = regex::Regex::new(r"\n{3,}").unwrap();
    let text = re_blank.replace_all(&text, "\n\n");

    text.trim().to_string()
}

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch a URL and return its content as plain text. HTML pages are stripped to readable text. Use this to read web pages, API docs, or raw text files from the internet."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The URL to fetch" },
                "max_bytes": { "type": "integer", "description": "Max bytes to return (default 48000)" }
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let args: WebFetchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let max_bytes = args.max_bytes.unwrap_or(48_000);

        if let Err(reason) = validate_web_url(&args.url) {
            return Err(ToolError::InvalidArgs(reason));
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("metis/0.1 (rust agent cli)")
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                // Re-validate redirect targets to prevent SSRF via open redirect.
                if validate_web_url(attempt.url().as_str()).is_err() {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|e| ToolError::Spawn(format!("http client: {e}")))?;

        let response = client
            .get(&args.url)
            .send()
            .await
            .map_err(|e| ToolError::Io {
                path: args.url.clone(),
                source: std::io::Error::other(e.to_string()),
            })?;

        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !status.is_success() {
            return Ok(format!("[web_fetch] HTTP {} for {}\n", status, args.url));
        }

        let body = response.text().await.map_err(|e| ToolError::Io {
            path: args.url.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;

        let text =
            if content_type.contains("text/html") || content_type.contains("application/xhtml") {
                strip_html(&body)
            } else {
                body
            };

        if text.len() > max_bytes {
            // Find a char boundary at or before max_bytes.
            let mut end = max_bytes;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            let truncated = &text[..end];
            Ok(format!(
                "{truncated}\n\n[truncated: showing {max_bytes} of {} bytes]\n",
                text.len()
            ))
        } else {
            Ok(text)
        }
    }
}

// ---------------------------------------------------------------------
// WebSearch — Tavily (if TAVILY_API_KEY set) → DuckDuckGo fallback
// ---------------------------------------------------------------------

pub struct WebSearch;

#[derive(Debug, Deserialize)]
struct WebSearchArgs {
    query: String,
    /// Maximum results to return (default 8).
    #[serde(default)]
    max_results: Option<usize>,
}

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web for a query and return a list of results with titles, \
         URLs, and snippets. Uses Tavily (if TAVILY_API_KEY set) or DuckDuckGo. \
         Good for finding documentation, recent information, or answers to questions."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Max results to return (default 8)",
                    "minimum": 1,
                    "maximum": 20
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let args: WebSearchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let max = args.max_results.unwrap_or(8);

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
            .build()
            .map_err(|e| ToolError::Spawn(format!("http client: {e}")))?;

        // Try Tavily first if API key is available
        if let Ok(tavily_key) = std::env::var("TAVILY_API_KEY") {
            if !tavily_key.is_empty() {
                match search_tavily(&client, &args.query, max, &tavily_key).await {
                    Ok(out) => return Ok(out),
                    Err(_) => {
                        // Fall through to DDG silently — eprintln corrupts TUI alt-screen
                    }
                }
            }
        }

        // Fallback: DuckDuckGo HTML scrape
        search_duckduckgo(&client, &args.query, max).await
    }
}

async fn search_tavily(
    client: &reqwest::Client,
    query: &str,
    max: usize,
    api_key: &str,
) -> Result<String, ToolError> {
    let body = serde_json::json!({
        "query": query,
        "max_results": max.min(20),
        "include_answer": false,
    });

    let response = client
        .post("https://api.tavily.com/search")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| ToolError::Spawn(format!("tavily request: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(ToolError::Spawn(format!("Tavily HTTP {status}: {text}")));
    }

    let data: serde_json::Value = response
        .json()
        .await
        .map_err(|e| ToolError::Spawn(format!("tavily parse: {e}")))?;

    let results = data["results"].as_array();
    if results.map(|r| r.is_empty()).unwrap_or(true) {
        return Ok(format!("No results found for: {query}\n"));
    }

    let mut out = format!("Search results for: {query}\n\n");
    for (i, r) in results.unwrap().iter().enumerate() {
        let title = r["title"].as_str().unwrap_or("(no title)");
        let url = r["url"].as_str().unwrap_or("");
        let snippet = r["content"]
            .as_str()
            .or_else(|| r["snippet"].as_str())
            .unwrap_or("");
        out.push_str(&format!(
            "{}. **{}**\n   {}\n   {}\n\n",
            i + 1,
            title,
            url,
            snippet
        ));
    }
    Ok(out)
}

async fn search_duckduckgo(
    client: &reqwest::Client,
    query: &str,
    max: usize,
) -> Result<String, ToolError> {
    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        urlencoding::encode(query)
    );

    let response = client.get(&url).send().await.map_err(|e| ToolError::Io {
        path: url.clone(),
        source: std::io::Error::other(e.to_string()),
    })?;

    if !response.status().is_success() {
        return Ok(format!(
            "[web_search] HTTP {} for query: {}\n",
            response.status(),
            query
        ));
    }

    let body = response.text().await.map_err(|e| ToolError::Io {
        path: url,
        source: std::io::Error::other(e.to_string()),
    })?;

    let results = parse_ddg_results(&body, max);
    if results.is_empty() {
        return Ok(format!("No results found for: {query}\n"));
    }

    let mut out = format!("Search results for: {query}\n\n");
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!(
            "{}. **{}**\n   {}\n   {}\n\n",
            i + 1,
            r.title,
            r.url,
            r.snippet
        ));
    }
    Ok(out)
}

pub(super) struct SearchResult {
    pub(super) title: String,
    pub(super) url: String,
    pub(super) snippet: String,
}

/// Parse DuckDuckGo HTML results page. Each result is in a
/// `<div class="result">` with an `<a class="result__a">` for title/URL
/// and `<a class="result__snippet">` for the snippet.
pub(super) fn parse_ddg_results(html: &str, max: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();

    // Split on result blocks
    let re_result = regex::Regex::new(r#"(?s)class="result\s[^"]*results_links"#).unwrap();
    let positions: Vec<usize> = re_result.find_iter(html).map(|m| m.start()).collect();

    for (i, &pos) in positions.iter().enumerate() {
        if results.len() >= max {
            break;
        }
        let end = positions.get(i + 1).copied().unwrap_or(html.len());
        let block = &html[pos..end];

        // Extract title + URL from result__a link
        let title_url = extract_ddg_link(block);
        let snippet = extract_ddg_snippet(block);

        if let Some((title, url)) = title_url {
            if !url.is_empty() && !title.is_empty() {
                results.push(SearchResult {
                    title,
                    url,
                    snippet: snippet.unwrap_or_default(),
                });
            }
        }
    }
    results
}

/// Extract title text and href from `<a class="result__a" href="...">title</a>`
fn extract_ddg_link(block: &str) -> Option<(String, String)> {
    let re = regex::Regex::new(r#"(?s)<a[^>]+class="result__a"[^>]+href="([^"]*)"[^>]*>(.*?)</a>"#)
        .unwrap();
    if let Some(caps) = re.captures(block) {
        let raw_url = caps.get(1)?.as_str();
        let title_html = caps.get(2)?.as_str();
        // DDG wraps URLs in a redirect; extract the actual URL
        let url = decode_ddg_url(raw_url);
        let title = strip_inline_html(title_html);
        Some((title, url))
    } else {
        None
    }
}

/// Extract snippet from `<a class="result__snippet"...>text</a>`
fn extract_ddg_snippet(block: &str) -> Option<String> {
    let re = regex::Regex::new(r#"(?s)<a[^>]+class="result__snippet"[^>]*>(.*?)</a>"#).unwrap();
    re.captures(block)
        .and_then(|c| c.get(1))
        .map(|m| strip_inline_html(m.as_str()))
}

/// DDG redirect URLs look like `/l/?uddg=https%3A%2F%2F...&rut=...`
pub(super) fn decode_ddg_url(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("//duckduckgo.com/l/?") {
        // Find uddg= parameter
        for param in rest.split('&') {
            if let Some(encoded) = param.strip_prefix("uddg=") {
                return urlencoding::decode(encoded)
                    .map(|s| s.into_owned())
                    .unwrap_or_else(|_| encoded.to_string());
            }
        }
    }
    raw.to_string()
}

/// Strip inline HTML tags, leaving just text.
pub(super) fn strip_inline_html(s: &str) -> String {
    let re = regex::Regex::new(r"<[^>]+>").unwrap();
    let text = re.replace_all(s, "");
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}
