use anyhow::Result;
use aegis_core::{MemoryEntry, MemoryStore, MemoryType};
use std::path::Path;

/// A lightweight match result from searching memory entries.
struct MemoryHit {
    entry: MemoryEntry,
    score: f64,
}

/// `aegis recall <query>` — search workspace memories and print ranked results.
pub fn run(query: &str, workspace: &Path, limit: usize) -> Result<()> {
    let query = query.trim();
    if query.is_empty() {
        anyhow::bail!("usage: aegis recall <query>");
    }
    if limit == 0 {
        anyhow::bail!("--limit must be greater than 0");
    }
    let store = MemoryStore::open(workspace)?;
    let entries = store.list()?;

    // Simple keyword match: score = number of query words matched in name + body
    let query_lower = query.to_lowercase();
    let query_words: Vec<&str> = query_lower.split_whitespace().collect();

    let mut hits: Vec<MemoryHit> = entries
        .into_iter()
        .filter_map(|entry| {
            let haystack = format!(
                "{} {} {}",
                entry.meta.name.to_lowercase(),
                entry.meta.description.to_lowercase(),
                entry.body.to_lowercase()
            );
            let matches: usize = query_words
                .iter()
                .filter(|w| haystack.contains(*w))
                .count();
            if matches == 0 {
                return None;
            }
            let bonus = if haystack.contains(&query_lower) { 0.5 } else { 0.0 };
            Some(MemoryHit {
                entry,
                score: (matches as f64 / query_words.len() as f64) + bonus,
            })
        })
        .collect();

    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(limit);

    if hits.is_empty() {
        println!("no memories matched `{query}`");
        return Ok(());
    }
    for hit in &hits {
        println!("{}", format_hit(hit));
    }
    Ok(())
}

/// Tag prefix shown next to each hit so users can scan by memory type.
fn type_tag(mt: MemoryType) -> &'static str {
    match mt {
        MemoryType::User => "[usr]",
        MemoryType::Feedback => "[fbk]",
        MemoryType::Project => "[proj]",
        MemoryType::Reference => "[ref]",
    }
}

/// Render a single hit as two lines: `[tag] [score]  name — description`
/// followed by the filename indented underneath.
fn format_hit(hit: &MemoryHit) -> String {
    let tag = type_tag(hit.entry.meta.memory_type);
    format!(
        "{tag} [{:.3}]  {} — {}\n        {}",
        hit.score, hit.entry.meta.name, hit.entry.meta.description, hit.entry.filename,
    )
}

#[cfg(test)]
mod tests {

    use super::*;
    use aegis_core::MemoryMeta;

    fn hit(mt: MemoryType, score: f64) -> MemoryHit {
        MemoryHit {
            entry: MemoryEntry {
                meta: MemoryMeta {
                    name: "Sample Entry".to_string(),
                    description: "A short hook for testing memory search".to_string(),
                    memory_type: mt,
                },
                body: String::new(),
                filename: "sample.md".to_string(),
            },
            score,
        }
    }

    #[test]
    fn type_tag_covers_all_variants() {
        assert_eq!(type_tag(MemoryType::User), "[usr]");
        assert_eq!(type_tag(MemoryType::Feedback), "[fbk]");
        assert_eq!(type_tag(MemoryType::Project), "[proj]");
        assert_eq!(type_tag(MemoryType::Reference), "[ref]");
    }

    #[test]
    fn format_hit_includes_tag_score_name_description_and_filename() {
        let out = format_hit(&hit(MemoryType::Feedback, 0.4218));
        assert!(
            out.starts_with("[fbk] [0.422]"),
            "got: {out}"
        );
        assert!(out.contains("Sample Entry — A short hook for testing memory search"));
        assert!(out.contains("sample.md"));
    }

    #[test]
    fn format_hit_rounds_score_to_three_decimals() {
        let out = format_hit(&hit(MemoryType::User, 0.123456));
        assert!(
            out.contains("[0.123]"),
            "expected 3-decimal score, got: {out}"
        );
    }

    #[test]
    fn format_hit_uses_two_lines() {
        let out = format_hit(&hit(MemoryType::Project, 1.0));
        assert_eq!(out.lines().count(), 2);
    }
}

