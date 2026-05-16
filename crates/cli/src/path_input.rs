//! User-facing path parsing for slash commands like `/image`.
//!
//! Handles the things users actually paste in: `~/Desktop/foo.png`,
//! `"/path with spaces/x.png"`, `'a.png'`, drag-dropped
//! `file:///Users/me/x.png`, backslash-escaped spaces (`a\ b.png`).
//!
//! Shared by both REPL and TUI so `/image <whatever>` behaves the same.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Outcome of validating an image path for attachment.
pub enum ImagePrep {
    /// Ready-to-send path (either the original, or a JPEG converted
    /// from HEIC/HEIF in a temp file).
    Ok(PathBuf),
    NotFound(PathBuf),
    Unsupported(String),
    ConversionFailed(String),
}

/// Validate `path` exists and has a supported image extension. If
/// HEIC/HEIF, convert to JPEG in a temp file via macOS `sips`.
pub fn prepare_image(path: &Path) -> ImagePrep {
    if !path.exists() {
        return ImagePrep::NotFound(path.to_path_buf());
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if matches!(ext.as_str(), "heic" | "heif") {
        match convert_heic_to_jpeg(path) {
            Ok(p) => ImagePrep::Ok(p),
            Err(e) => ImagePrep::ConversionFailed(e.to_string()),
        }
    } else if matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    ) {
        ImagePrep::Ok(path.to_path_buf())
    } else {
        ImagePrep::Unsupported(ext)
    }
}

/// Read an image from the system clipboard and write it to a temp PNG
/// file. Returns the temp path. macOS-only: uses `pbpaste -Prefer png`.
///
/// Validates the output is a real PNG by checking the magic bytes, so
/// an empty clipboard or a text-only clipboard fails loudly rather
/// than attaching a 0-byte junk file.
pub fn paste_image_from_clipboard() -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let out = std::env::temp_dir().join(format!("metis-paste-{ts}.png"));
    let file = std::fs::File::create(&out)
        .with_context(|| format!("could not create {}", out.display()))?;
    let status = std::process::Command::new("pbpaste")
        .arg("-Prefer")
        .arg("png")
        .stdout(std::process::Stdio::from(file))
        .stderr(std::process::Stdio::null())
        .status()
        .context("failed to invoke `pbpaste` (macOS only)")?;
    if !status.success() {
        let _ = std::fs::remove_file(&out);
        anyhow::bail!("pbpaste exited with {status}");
    }
    let bytes = std::fs::read(&out).context("failed to re-read clipboard temp file")?;
    if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        let _ = std::fs::remove_file(&out);
        anyhow::bail!("clipboard does not contain a PNG (copy an image or screenshot first)");
    }
    Ok(out)
}

fn convert_heic_to_jpeg(src: &Path) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let mut out = std::env::temp_dir();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    out.push(format!("metis-heic-{ts}-{stem}.jpg"));
    let status = std::process::Command::new("sips")
        .arg("-s")
        .arg("format")
        .arg("jpeg")
        .arg(src)
        .arg("--out")
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("failed to invoke `sips` (macOS only)")?;
    if !status.success() {
        anyhow::bail!("sips exited with {status}");
    }
    if !out.exists() {
        anyhow::bail!("sips reported success but output file missing");
    }
    Ok(out)
}

/// Expand a single user-supplied path string to an absolute `PathBuf`.
///
/// - Strips matching outer quotes (single or double).
/// - Unescapes backslash-escaped characters (`\ ` → ` `).
/// - Strips `file://` URL prefix (drag-drop on some terminals).
/// - Expands leading `~/` and bare `~` via `dirs::home_dir()`.
/// - Joins relative paths onto `workspace`.
pub fn resolve(raw: &str, workspace: &Path) -> PathBuf {
    let trimmed = raw.trim();
    let unquoted = strip_outer_quotes(trimmed);
    let unescaped = unescape_backslashes(unquoted);
    let no_scheme = unescaped
        .strip_prefix("file://")
        .unwrap_or(&unescaped)
        .to_string();
    let expanded = expand_tilde(&no_scheme);
    let p = PathBuf::from(expanded.as_ref());
    if p.is_absolute() {
        p
    } else {
        workspace.join(p)
    }
}

/// Split a raw argument string into multiple paths, then resolve each.
///
/// Supports space-separated unquoted paths, plus quoted paths containing
/// spaces. Backslash-escaped spaces keep the two tokens together.
/// Returns paths in argument order. Empty input → empty vec.
pub fn resolve_many(raw: &str, workspace: &Path) -> Vec<PathBuf> {
    tokenize(raw.trim())
        .into_iter()
        .map(|tok| resolve(&tok, workspace))
        .collect()
}

fn strip_outer_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn unescape_backslashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn expand_tilde(s: &str) -> Cow<'_, str> {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            let mut out = home.to_string_lossy().into_owned();
            if !out.ends_with('/') {
                out.push('/');
            }
            out.push_str(rest);
            return Cow::Owned(out);
        }
    }
    if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return Cow::Owned(home.to_string_lossy().into_owned());
        }
    }
    Cow::Borrowed(s)
}

/// Shell-lite tokenizer: splits on whitespace, respects matched
/// single/double quotes, treats `\<char>` as a literal (so `a\ b.png`
/// is one token). Keeps quotes inside the token so `resolve` can strip
/// them with the same rules as an un-tokenized single arg.
fn tokenize(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if !in_single => {
                if let Some(&next) = chars.peek() {
                    cur.push('\\');
                    cur.push(next);
                    chars.next();
                }
            }
            '\'' if !in_double => {
                in_single = !in_single;
                cur.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                cur.push(c);
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ws() -> PathBuf {
        PathBuf::from("/tmp/ws")
    }

    #[test]
    fn absolute_path_unchanged() {
        assert_eq!(resolve("/etc/hosts", &ws()), PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn relative_joined_to_workspace() {
        assert_eq!(resolve("img.png", &ws()), PathBuf::from("/tmp/ws/img.png"));
    }

    #[test]
    fn tilde_expands_to_home() {
        let got = resolve("~/foo.png", &ws());
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(got, home.join("foo.png"));
    }

    #[test]
    fn bare_tilde_expands_to_home() {
        let got = resolve("~", &ws());
        assert_eq!(got, dirs::home_dir().expect("home"));
    }

    #[test]
    fn double_quotes_stripped() {
        assert_eq!(
            resolve("\"/path with spaces/x.png\"", &ws()),
            PathBuf::from("/path with spaces/x.png")
        );
    }

    #[test]
    fn single_quotes_stripped() {
        assert_eq!(resolve("'/a/b.png'", &ws()), PathBuf::from("/a/b.png"));
    }

    #[test]
    fn backslash_escaped_space() {
        assert_eq!(
            resolve("/path\\ with\\ spaces/x.png", &ws()),
            PathBuf::from("/path with spaces/x.png")
        );
    }

    #[test]
    fn file_url_scheme_stripped() {
        assert_eq!(
            resolve("file:///Users/me/x.png", &ws()),
            PathBuf::from("/Users/me/x.png")
        );
    }

    #[test]
    fn file_url_with_tilde_fallback() {
        // file:// + ~ is pathological but shouldn't panic.
        let _ = resolve("file://~/x.png", &ws());
    }

    #[test]
    fn leading_trailing_whitespace_trimmed() {
        assert_eq!(resolve("   /a/b.png  ", &ws()), PathBuf::from("/a/b.png"));
    }

    #[test]
    fn tokenize_plain_split() {
        assert_eq!(tokenize("a b c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn tokenize_double_quoted() {
        assert_eq!(tokenize("a \"b c\" d"), vec!["a", "\"b c\"", "d"]);
    }

    #[test]
    fn tokenize_single_quoted() {
        assert_eq!(tokenize("'a b' c"), vec!["'a b'", "c"]);
    }

    #[test]
    fn tokenize_backslash_space_joins() {
        assert_eq!(tokenize("a\\ b c"), vec!["a\\ b", "c"]);
    }

    #[test]
    fn resolve_many_two_paths() {
        let got = resolve_many("/a.png /b.png", &ws());
        assert_eq!(got, vec![PathBuf::from("/a.png"), PathBuf::from("/b.png")]);
    }

    #[test]
    fn resolve_many_with_quoted_space() {
        let got = resolve_many("\"/p with/a.png\" /b.png", &ws());
        assert_eq!(
            got,
            vec![PathBuf::from("/p with/a.png"), PathBuf::from("/b.png"),]
        );
    }

    #[test]
    fn resolve_many_empty() {
        assert!(resolve_many("", &ws()).is_empty());
    }
}
