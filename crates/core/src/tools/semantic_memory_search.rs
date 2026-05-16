//! Semantic memory search via the adaptmem HTTP daemon.
//!
//! Issues a domain-tuned bi-encoder retrieval query against the contents of
//! `.aegis/memory/*.md`. Falls back gracefully when the daemon is not
//! running — the agent sees a structured error and can suggest the user
//! start `adaptmem serve`.
//!
//! Contract: see `~/Projects/adaptmem/docs/aegis_integration.md`.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:7800";
const CORPUS_ID: &str = "metis_memory";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
struct Args {
    query: String,
    #[serde(default = "default_top_k")]
    top_k: usize,
    /// When true (default), re-encodes the local memory dir before searching.
    /// Set to false for back-to-back queries where the corpus has not changed.
    #[serde(default = "default_true")]
    refresh_index: bool,
}

fn default_top_k() -> usize {
    5
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct ReindexDoc {
    id: String,
    text: String,
}

#[derive(Debug, Serialize)]
struct ReindexRequest {
    corpus_id: String,
    documents: Vec<ReindexDoc>,
}

#[derive(Debug, Serialize)]
struct SearchRequest {
    query: String,
    top_k: usize,
    corpus_id: String,
}

#[derive(Debug, Deserialize)]
struct SearchHit {
    id: String,
    text: String,
    score: f64,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    hits: Vec<SearchHit>,
    elapsed_ms: f64,
}

pub struct SemanticMemorySearch;

#[async_trait]
impl Tool for SemanticMemorySearch {
    fn name(&self) -> &str {
        "semantic_memory_search"
    }

    fn description(&self) -> &str {
        "Semantic search over .metis/memory/*.md via the adaptmem daemon. \
         Returns the top-k memory entries ranked by domain-tuned bi-encoder \
         similarity, not literal string match. Requires `adaptmem serve` \
         (default http://127.0.0.1:7800). When `refresh_index` is true \
         (default), re-encodes the local memory dir before searching."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language query — what concept / topic / decision to look for."
                },
                "top_k": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "default": 5,
                    "description": "Maximum number of memory entries to return."
                },
                "refresh_index": {
                    "type": "boolean",
                    "default": true,
                    "description": "Re-encode .metis/memory/ before searching. Pass false for back-to-back queries on an unchanged corpus."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let args: Args =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let daemon_url =
            std::env::var("ADAPTMEM_URL").unwrap_or_else(|_| DEFAULT_DAEMON_URL.to_string());
        // Optional Bearer token. Adaptmem daemon requires it when started
        // with `--api-key`; absent for localhost dev.
        let api_key = std::env::var("ADAPTMEM_API_KEY").ok();
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| daemon_error(&daemon_url, format!("client build: {e}")))?;

        if args.refresh_index {
            let docs = read_memory_dir(ctx)?;
            if docs.is_empty() {
                return Ok("No memory entries found under .metis/memory/. \
                     Use `save_memory` first."
                    .to_string());
            }
            let req = ReindexRequest {
                corpus_id: CORPUS_ID.to_string(),
                documents: docs,
            };
            let mut reindex_req = client.post(format!("{daemon_url}/reindex"));
            if let Some(k) = &api_key {
                reindex_req = reindex_req.bearer_auth(k);
            }
            let resp = reindex_req
                .json(&req)
                .send()
                .await
                .map_err(|e| daemon_error(&daemon_url, format!("reindex POST: {e}")))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(daemon_error(
                    &daemon_url,
                    format!("reindex returned {status}: {body}"),
                ));
            }
        }

        let req = SearchRequest {
            query: args.query.clone(),
            top_k: args.top_k,
            corpus_id: CORPUS_ID.to_string(),
        };
        let mut search_req = client.post(format!("{daemon_url}/search"));
        if let Some(k) = &api_key {
            search_req = search_req.bearer_auth(k);
        }
        let resp = search_req
            .json(&req)
            .send()
            .await
            .map_err(|e| daemon_error(&daemon_url, format!("search POST: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(daemon_error(
                &daemon_url,
                format!("search returned {status}: {body}"),
            ));
        }
        let body: SearchResponse = resp
            .json()
            .await
            .map_err(|e| daemon_error(&daemon_url, format!("search JSON: {e}")))?;

        Ok(format_hits(&args.query, &body))
    }
}

fn daemon_error(url: &str, reason: String) -> ToolError {
    ToolError::Io {
        path: url.to_string(),
        source: std::io::Error::other(format!(
            "adaptmem daemon at {url} unreachable: {reason}. \
             Start it with `pip install \"adaptmem[server]\" && adaptmem serve`."
        )),
    }
}

fn read_memory_dir(ctx: &ToolContext) -> Result<Vec<ReindexDoc>, ToolError> {
    let memory_dir = ctx.workspace_root.join(".metis/memory");
    if !memory_dir.exists() {
        return Ok(Vec::new());
    }
    let mut docs = Vec::new();
    let entries = std::fs::read_dir(&memory_dir).map_err(|e| ToolError::Io {
        path: memory_dir.display().to_string(),
        source: e,
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| ToolError::Io {
            path: memory_dir.display().to_string(),
            source: e,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        // Skip the index file itself.
        if path.file_name().and_then(|s| s.to_str()) == Some("MEMORY.md") {
            continue;
        }
        let id = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let text = std::fs::read_to_string(&path).map_err(|e| ToolError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        docs.push(ReindexDoc { id, text });
    }
    Ok(docs)
}

fn format_hits(query: &str, body: &SearchResponse) -> String {
    if body.hits.is_empty() {
        return format!("No memory entries matched `{query}`.");
    }
    let mut out = format!(
        "Top {} matches for `{}` ({:.0}ms):\n\n",
        body.hits.len(),
        query,
        body.elapsed_ms
    );
    for hit in &body.hits {
        let preview = preview_line(&hit.text);
        out.push_str(&format!(
            "- **[{}]** (score {:.3}): {}\n",
            hit.id, hit.score, preview
        ));
    }
    out
}

fn preview_line(text: &str) -> String {
    // Skip frontmatter delimiters and the frontmatter block; return the
    // first non-empty content line, truncated to ~120 chars.
    let mut in_frontmatter = false;
    let mut started_frontmatter = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            if !started_frontmatter {
                in_frontmatter = true;
                started_frontmatter = true;
                continue;
            } else if in_frontmatter {
                in_frontmatter = false;
                continue;
            }
        }
        if in_frontmatter || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut s = trimmed.to_string();
        if s.len() > 120 {
            s.truncate(117);
            s.push('…');
        }
        return s;
    }
    "(no preview)".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_skips_frontmatter() {
        let text = "---\nname: foo\n---\n\nThe actual content line is here.";
        assert_eq!(preview_line(text), "The actual content line is here.");
    }

    #[test]
    fn preview_handles_no_frontmatter() {
        let text = "First sentence.\nSecond.";
        assert_eq!(preview_line(text), "First sentence.");
    }

    #[test]
    fn preview_truncates_long_lines() {
        let long = "x".repeat(200);
        let text = format!("---\nname: foo\n---\n{long}");
        let p = preview_line(&text);
        assert!(p.len() <= 120);
        assert!(p.ends_with("…"));
    }

    #[test]
    fn format_hits_includes_score_and_id() {
        let body = SearchResponse {
            hits: vec![SearchHit {
                id: "feedback_caching.md".into(),
                text: "---\nname: caching\n---\n\nUse Redis for hot keys.".into(),
                score: 0.823,
            }],
            elapsed_ms: 12.5,
        };
        let out = format_hits("redis", &body);
        assert!(out.contains("feedback_caching.md"));
        assert!(out.contains("0.823"));
        assert!(out.contains("Use Redis for hot keys"));
    }

    #[test]
    fn format_hits_empty_message() {
        let body = SearchResponse {
            hits: vec![],
            elapsed_ms: 1.0,
        };
        assert!(format_hits("x", &body).contains("No memory entries"));
    }
}
