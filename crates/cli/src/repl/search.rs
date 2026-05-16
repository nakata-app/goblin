//! File-search primitives for the `/search` slash command.
//!
//! All items are `pub(crate)` because repl.rs is the only consumer.
//! `search_directory` walks the workspace tree, skipping hidden files
//! and binary heuristics; `highlight_pattern` renders matches with
//! ANSI color.

use std::io::Read;
use std::path::Path;

use regex::Regex;
use walkdir::WalkDir;

/// Search result from file search
#[derive(Debug, Clone)]
pub(crate) struct SearchResult {
    pub(crate) file_path: String,
    pub(crate) line_number: usize,
    pub(crate) line_content: String,
}

/// Parse search command options
/// Format: /search [flags] pattern
/// Flags:
///   -i: case insensitive
///   -r: use regex
///   -n N: max results (default: 1000)
///   -t ext1,ext2: filter by file extensions
pub(crate) fn parse_search_options(input: &str) -> (String, bool, bool, usize, Vec<String>) {
    let mut pattern = String::new();
    let mut case_sensitive = true;
    let mut use_regex = false;
    let mut max_results = 1000;
    let mut file_types = Vec::new();

    let mut parts = input.split_whitespace();
    let mut in_flags = true;

    while let Some(part) = parts.next() {
        if in_flags && part.starts_with('-') {
            // Only known flags are treated as flags
            match part {
                "-i" => case_sensitive = false,
                "-r" => use_regex = true,
                "-n" => {
                    if let Some(next) = parts.next() {
                        max_results = next.parse().unwrap_or(1000);
                    }
                }
                "-t" => {
                    if let Some(next) = parts.next() {
                        file_types = next.split(',').map(|s| s.trim().to_string()).collect();
                    }
                }
                _ => {
                    // Not a known flag, treat as part of pattern
                    pattern.push_str(part);
                    pattern.push(' ');
                    in_flags = false;
                }
            }
        } else {
            pattern.push_str(part);
            pattern.push(' ');
            in_flags = false;
        }
    }

    (
        pattern.trim().to_string(),
        case_sensitive,
        use_regex,
        max_results,
        file_types,
    )
}

/// Search directory recursively for pattern matches
pub(crate) fn search_directory(
    workspace: &Path,
    pattern: &str,
    case_sensitive: bool,
    use_regex: bool,
    file_types: &[String],
    results: &mut Vec<SearchResult>,
    max_results: usize,
) -> usize {
    let mut files_searched = 0;

    // Walk the directory
    let entries_iter = WalkDir::new(workspace)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e: &walkdir::DirEntry| {
            let name = e.file_name().to_string_lossy();
            !name.starts_with('.') && !name.ends_with('~')
        });

    for entry_result in entries_iter {
        let entry = match entry_result {
            Ok(e) => e,
            Err(_) => continue,
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();

        // Check file extension filter
        if !file_types.is_empty() {
            if let Some(ext) = path.extension().and_then(|e: &std::ffi::OsStr| e.to_str()) {
                if !file_types.iter().any(|ft| ft.eq_ignore_ascii_case(ext)) {
                    continue;
                }
            } else {
                continue;
            }
        }

        // Skip binary files (heuristic: check first few bytes)
        if let Ok(mut file) = std::fs::File::open(path) {
            let mut buffer = [0; 1024];
            if let Ok(n) = file.read(&mut buffer) {
                if n > 0 && buffer[..n].contains(&0) {
                    continue; // Binary file
                }
            }
        }

        files_searched += 1;

        // Read file content
        match std::fs::read_to_string(path) {
            Ok(content) => {
                // Search each line
                for (i, line) in content.lines().enumerate() {
                    let line_num = i + 1;

                    let matched = if use_regex {
                        let regex = if case_sensitive {
                            Regex::new(pattern).ok()
                        } else {
                            // Only add (?i) if not already present
                            if pattern.starts_with("(?i)") || pattern.starts_with("(?-i)") {
                                Regex::new(pattern).ok()
                            } else {
                                let regex_str = format!("(?i){}", pattern);
                                Regex::new(&regex_str).ok()
                            }
                        };
                        regex.map(|re: Regex| re.is_match(line)).unwrap_or(false)
                    } else {
                        if case_sensitive {
                            line.contains(pattern)
                        } else {
                            line.to_lowercase().contains(&pattern.to_lowercase())
                        }
                    };

                    if matched {
                        results.push(SearchResult {
                            file_path: path.to_string_lossy().to_string(),
                            line_number: line_num,
                            line_content: line.to_string(),
                        });

                        if results.len() >= max_results {
                            return files_searched;
                        }
                    }
                }
            }
            Err(_) => {
                // Could not read as text, skip
                continue;
            }
        }
    }

    files_searched
}

/// Highlight pattern matches in text with ANSI colors
pub(crate) fn highlight_pattern(text: &str, pattern: &str, case_sensitive: bool) -> String {
    if pattern.is_empty() {
        return text.to_string();
    }

    // For regex patterns, we need to parse them
    if pattern.starts_with('^')
        || pattern.contains('$')
        || pattern.contains('[')
        || pattern.contains('(')
        || pattern.contains('*')
        || pattern.contains('+')
        || pattern.contains('?')
        || pattern.contains('\\')
        || pattern.contains('|')
    {
        // Try to compile as regex
        let regex_pattern = if case_sensitive {
            pattern.to_string()
        } else {
            format!("(?i){}", pattern)
        };
        if let Ok(re) = regex::Regex::new(&regex_pattern) {
            let mut result = String::new();
            let mut last_end = 0;

            for mat in re.find_iter(text) {
                // Add text before match
                result.push_str(&text[last_end..mat.start()]);
                // Add highlighted match
                result.push_str("\x1b[1;31m");
                result.push_str(mat.as_str());
                result.push_str("\x1b[0m");
                last_end = mat.end();
            }

            // Add remaining text
            result.push_str(&text[last_end..]);
            return result;
        }
    }

    // Simple text search
    let search_text = if case_sensitive {
        text.to_string()
    } else {
        text.to_lowercase()
    };
    let search_pattern = if case_sensitive {
        pattern.to_string()
    } else {
        pattern.to_lowercase()
    };

    let mut result = String::new();
    let mut last_end = 0;

    for (start, _) in search_text.match_indices(&search_pattern) {
        let end = start + pattern.len();

        // Add text before match
        result.push_str(&text[last_end..start]);
        // Add highlighted match
        result.push_str("\x1b[1;31m");
        result.push_str(&text[start..end]);
        result.push_str("\x1b[0m");
        last_end = end;
    }

    // Add remaining text
    result.push_str(&text[last_end..]);
    result
}
