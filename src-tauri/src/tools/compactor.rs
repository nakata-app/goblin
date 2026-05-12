/// RTK-style tool output compactor.
/// Compresses tool results before sending to LLM to save tokens.
/// Safe: never mutates, only truncates or filters. Returns original if unsure.

const MAX_CHARS: usize = 4000;
const HEAD_CHARS: usize = 1200;
const TAIL_CHARS: usize = 800;

pub fn compact(tool_name: &str, output: &str) -> String {
    // Small outputs: pass through unchanged
    if output.len() <= MAX_CHARS {
        return output.to_string();
    }

    match tool_name {
        "git_diff" => compact_diff(output),
        "git_status" => compact_status(output),
        "git_log" => compact_log(output),
        "grep" => compact_grep(output),
        "glob" => compact_list(output, "files"),
        "bash" => compact_bash(output),
        "read_file" => compact_file(output),
        _ => compact_generic(output),
    }
}

fn compact_diff(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();

    // Keep: diff headers + hunk headers + +/- lines. Skip context lines without prefix.
    let meaningful: Vec<&str> = lines
        .iter()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("diff ") || t.starts_with("index ") || t.starts_with("--- ") || t.starts_with("+++ ") || t.starts_with("@@") || t.starts_with('+') || t.starts_with('-')
        })
        .copied()
        .collect();

    let result = meaningful.join("\n");
    if result.len() <= MAX_CHARS {
        return result;
    }

    // Still too big: take head + tail
    head_tail(&meaningful, HEAD_CHARS, TAIL_CHARS, total, "diff lines")
}

fn compact_status(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();
    // Keep only non-empty meaningful lines, drop blank separator lines
    let filtered: Vec<&str> = lines.into_iter().filter(|l| !l.trim().is_empty()).collect();
    let result = filtered.join("\n");
    if result.len() <= MAX_CHARS {
        return result;
    }
    let _flen = filtered.len();
    head_tail(&filtered, HEAD_CHARS, TAIL_CHARS, total, "status lines")
}

fn compact_log(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();
    // Keep first N commits, drop the rest
    let kept: Vec<&str> = lines.into_iter().take(30).collect();
    let result = kept.join("\n");
    if result.len() <= MAX_CHARS {
        return format!("{result}\n[{total} total commits, showing first 30]");
    }
    head_tail(&kept, HEAD_CHARS, TAIL_CHARS, total, "commits")
}

fn compact_grep(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();
    if total <= 40 {
        return output.to_string();
    }
    let head: Vec<&str> = lines.iter().take(25).copied().collect();
    let tail: Vec<&str> = lines.iter().rev().take(10).rev().copied().collect();
    let mut result = head.join("\n");
    if !tail.is_empty() {
        result.push_str("\n...\n");
        result.push_str(&tail.join("\n"));
    }
    format!("{result}\n[{total} total matches]")
}

fn compact_list(output: &str, label: &str) -> String {
    let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = lines.len();
    if total <= 40 {
        return output.to_string();
    }
    let head: Vec<&str> = lines.iter().take(25).copied().collect();
    let tail: Vec<&str> = lines.iter().rev().take(10).rev().copied().collect();
    let mut result = head.join("\n");
    if !tail.is_empty() {
        result.push_str("\n...\n");
        result.push_str(&tail.join("\n"));
    }
    format!("{result}\n[{total} total {label}]")
}

fn compact_bash(output: &str) -> String {
    // Detect diff output within bash
    if output.contains("diff --git") {
        return compact_diff(output);
    }
    // Detect list-like output
    if output.lines().count() > 50 && output.lines().all(|l| !l.contains('\t') || l.len() < 200) {
        return compact_list(output, "lines");
    }
    compact_generic(output)
}

fn compact_file(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();
    if total <= 100 {
        return output.to_string();
    }
    let head: Vec<&str> = lines.iter().take(40).copied().collect();
    let tail: Vec<&str> = lines.iter().rev().take(30).rev().copied().collect();
    let mut result = head.join("\n");
    result.push_str(&format!("\n... [{total} total lines, {} shown] ...\n", head.len() + tail.len()));
    result.push_str(&tail.join("\n"));
    result
}

fn compact_generic(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();
    head_tail(&lines, HEAD_CHARS, TAIL_CHARS, total, "lines")
}

fn head_tail(lines: &[&str], head_chars: usize, tail_chars: usize, total: usize, label: &str) -> String {
    let mut head = String::new();
    let mut head_count = 0usize;
    for l in lines {
        if head.len() + l.len() + 1 > head_chars {
            break;
        }
        if !head.is_empty() { head.push('\n'); }
        head.push_str(l);
        head_count += 1;
    }

    let mut tail = String::new();
    let mut tail_count = 0usize;
    for l in lines.iter().rev() {
        let candidate = format!("{}\n{}", l, tail);
        if candidate.len() > tail_chars + 50 {
            break;
        }
        tail = format!("{}\n{}", l, tail.trim());
        tail_count += 1;
    }
    tail = tail.trim().to_string();

    let skipped = total.saturating_sub(head_count + tail_count);
    if skipped == 0 {
        return format!("{head}\n{tail}");
    }

    format!("{head}\n... [{skipped} {label} skipped] ...\n{tail}")
}
