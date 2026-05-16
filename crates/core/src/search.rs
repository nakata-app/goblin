//! Semantic-lite search — TF-IDF based code search that finds relevant
//! code chunks by keyword relevance rather than exact text match.
//!
//! Splits source files into chunks (functions/classes), builds a simple
//! term frequency index, and ranks chunks by TF-IDF similarity to the
//! query. No external ML model needed — pure Rust, zero dependencies.

use std::collections::HashMap;
use std::path::Path;

/// A code chunk with its source location and content.
#[derive(Debug, Clone)]
pub struct CodeChunk {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub name: String, // function/class name or "top-level"
    pub content: String,
}

/// Search result with relevance score.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub chunk: CodeChunk,
    pub score: f64,
}

/// Build a searchable index from source files in the workspace.
pub fn build_index(root: &Path, max_files: usize) -> Vec<CodeChunk> {
    let files = crate::repomap::discover_source_files(root, max_files);
    let mut chunks = Vec::new();

    for file in &files {
        let rel = file
            .strip_prefix(root)
            .unwrap_or(file)
            .display()
            .to_string();
        let content = match std::fs::read_to_string(file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        chunks.extend(split_into_chunks(&content, &rel));
    }

    chunks
}

/// Split source code into logical chunks (functions, classes, etc.)
fn split_into_chunks(content: &str, filename: &str) -> Vec<CodeChunk> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let defs = crate::repomap::extract_definitions(content, filename);
    if defs.is_empty() {
        // No definitions found — treat entire file as one chunk
        return vec![CodeChunk {
            file: filename.to_string(),
            start_line: 1,
            end_line: lines.len(),
            name: "top-level".to_string(),
            content: content.to_string(),
        }];
    }

    let mut chunks = Vec::new();
    for (i, def) in defs.iter().enumerate() {
        let start = def.line.saturating_sub(1); // 0-indexed
        let end = if i + 1 < defs.len() {
            defs[i + 1].line.saturating_sub(2)
        } else {
            lines.len().saturating_sub(1)
        };
        let chunk_lines: Vec<&str> = lines[start..=end.min(lines.len() - 1)].to_vec();
        chunks.push(CodeChunk {
            file: filename.to_string(),
            start_line: start + 1,
            end_line: end + 1,
            name: format!("{} {}", def.kind, def.name),
            content: chunk_lines.join("\n"),
        });
    }

    chunks
}

/// Tokenize text into lowercase words for TF-IDF.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() > 1)
        .map(|w| w.to_lowercase())
        .collect()
}

/// Compute TF (term frequency) for a document.
fn term_frequency(tokens: &[String]) -> HashMap<&str, f64> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for token in tokens {
        *counts.entry(token.as_str()).or_default() += 1;
    }
    let total = tokens.len() as f64;
    counts
        .into_iter()
        .map(|(k, v)| (k, v as f64 / total))
        .collect()
}

/// Compute IDF (inverse document frequency) across all chunks.
fn inverse_document_frequency(chunks: &[Vec<String>]) -> HashMap<String, f64> {
    let n = chunks.len() as f64;
    let mut doc_count: HashMap<String, usize> = HashMap::new();

    for tokens in chunks {
        let unique: std::collections::HashSet<&str> = tokens.iter().map(|s| s.as_str()).collect();
        for word in unique {
            *doc_count.entry(word.to_string()).or_default() += 1;
        }
    }

    doc_count
        .into_iter()
        .map(|(word, count)| (word, (n / count as f64).ln()))
        .collect()
}

/// Search code chunks by query, returning top-k results ranked by TF-IDF.
pub fn search(chunks: &[CodeChunk], query: &str, top_k: usize) -> Vec<SearchResult> {
    if chunks.is_empty() {
        return Vec::new();
    }

    let query_tokens = tokenize(query);
    if query_tokens.is_empty() {
        return Vec::new();
    }

    // Tokenize all chunks
    let chunk_tokens: Vec<Vec<String>> = chunks.iter().map(|c| tokenize(&c.content)).collect();

    // Compute IDF
    let idf = inverse_document_frequency(&chunk_tokens);

    // Score each chunk
    let mut results: Vec<SearchResult> = chunks
        .iter()
        .zip(chunk_tokens.iter())
        .map(|(chunk, tokens)| {
            let tf = term_frequency(tokens);
            let score: f64 = query_tokens
                .iter()
                .map(|qt| {
                    let tf_val = tf.get(qt.as_str()).unwrap_or(&0.0);
                    let idf_val = idf.get(qt.as_str()).unwrap_or(&0.0);
                    tf_val * idf_val
                })
                .sum();
            // Boost score for name matches
            let name_lower = chunk.name.to_lowercase();
            let name_boost = if query_tokens
                .iter()
                .any(|qt| name_lower.contains(qt.as_str()))
            {
                2.0
            } else {
                1.0
            };
            SearchResult {
                chunk: chunk.clone(),
                score: score * name_boost,
            }
        })
        .filter(|r| r.score > 0.0)
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(top_k);
    results
}

/// Format search results as a readable string.
pub fn format_results(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "(no results)\n".to_string();
    }
    let mut out = String::new();
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!(
            "{}. {} — {}:{}-{} (score: {:.3})\n",
            i + 1,
            r.chunk.name,
            r.chunk.file,
            r.chunk.start_line,
            r.chunk.end_line,
            r.score
        ));
        // Show first 3 lines of content
        let preview: Vec<&str> = r.chunk.content.lines().take(3).collect();
        for line in &preview {
            out.push_str(&format!("   {line}\n"));
        }
        if r.chunk.content.lines().count() > 3 {
            out.push_str("   ...\n");
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_correctly() {
        let tokens = tokenize("fn hello_world(x: i32) -> String");
        assert!(tokens.contains(&"fn".to_string()));
        assert!(tokens.contains(&"hello_world".to_string()));
        assert!(tokens.contains(&"string".to_string()));
    }

    #[test]
    fn search_finds_relevant_chunks() {
        let chunks = vec![
            CodeChunk {
                file: "a.rs".into(),
                start_line: 1,
                end_line: 5,
                name: "fn process_request".into(),
                content: "fn process_request(req: Request) -> Response {\n    validate(req)\n}"
                    .into(),
            },
            CodeChunk {
                file: "b.rs".into(),
                start_line: 1,
                end_line: 3,
                name: "fn validate".into(),
                content: "fn validate(data: &str) -> bool {\n    !data.is_empty()\n}".into(),
            },
            CodeChunk {
                file: "c.rs".into(),
                start_line: 1,
                end_line: 3,
                name: "struct Config".into(),
                content: "struct Config {\n    port: u16,\n    host: String,\n}".into(),
            },
        ];

        let results = search(&chunks, "request process", 5);
        assert!(!results.is_empty());
        assert_eq!(results[0].chunk.name, "fn process_request");
    }

    #[test]
    fn search_empty_query_returns_empty() {
        let chunks = vec![CodeChunk {
            file: "a.rs".into(),
            start_line: 1,
            end_line: 1,
            name: "fn test".into(),
            content: "fn test() {}".into(),
        }];
        assert!(search(&chunks, "", 5).is_empty());
    }

    #[test]
    fn format_results_readable() {
        let results = vec![SearchResult {
            chunk: CodeChunk {
                file: "agent.rs".into(),
                start_line: 10,
                end_line: 20,
                name: "fn run".into(),
                content: "async fn run(&mut self) {\n    loop {\n        // agent loop\n    }\n}"
                    .into(),
            },
            score: 1.5,
        }];
        let formatted = format_results(&results);
        assert!(formatted.contains("fn run"));
        assert!(formatted.contains("agent.rs"));
        assert!(formatted.contains("1.500"));
    }

    #[test]
    fn index_on_self() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let chunks = build_index(&root, 20);
        assert!(!chunks.is_empty(), "should find code chunks");
        let results = search(&chunks, "agent run tool", 5);
        assert!(!results.is_empty(), "should find agent-related chunks");
    }
}
