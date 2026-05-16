//! Server-Sent Events frame parsers used by the streaming provider
//! clients.
//!
//! Factored out of `lib.rs` so the sync parser can be unit-tested against
//! a [`std::io::Cursor`] without needing a live HTTP response, and so the
//! async variant lives next to its sync sibling instead of in the middle
//! of the client bodies.

use crate::{ApiError, ApiResult};

/// Consume a Server-Sent Events byte stream, forwarding each complete
/// `data:` payload to `handle_data`. Returning `Ok(false)` from the
/// callback ends the loop early (used for OpenAI's `[DONE]` sentinel and
/// Anthropic's `message_stop`). Comments, `event:`, `id:`, and `retry:`
/// fields are ignored — the two providers we speak only care about
/// `data:`. Multi-line `data:` values are joined with `\n` per the spec.
///
/// Factored out so it can be unit-tested against a [`std::io::Cursor`]
/// without needing a live HTTP response.
pub fn consume_sse<R, F>(mut reader: R, mut handle_data: F) -> ApiResult<()>
where
    R: std::io::BufRead,
    F: FnMut(&str) -> ApiResult<bool>,
{
    let mut line_buf = String::new();
    let mut pending: Vec<String> = Vec::new();
    loop {
        line_buf.clear();
        let n = reader
            .read_line(&mut line_buf)
            .map_err(|e| ApiError::Decode(format!("sse read: {e}")))?;
        if n == 0 {
            // EOF — flush any trailing frame that wasn't terminated by a
            // blank line. Real servers always terminate, but being
            // forgiving keeps the parser testable without trailing
            // whitespace tricks.
            if !pending.is_empty() {
                let payload = pending.join("\n");
                let _ = handle_data(&payload)?;
            }
            return Ok(());
        }
        let line = line_buf.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if !pending.is_empty() {
                let payload = pending.join("\n");
                pending.clear();
                if !handle_data(&payload)? {
                    return Ok(());
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            // `data: hello` and `data:hello` are both legal per the SSE
            // spec; strip exactly one leading space if present.
            pending.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
        // Everything else (`event:`, `id:`, `retry:`, `:` comments) is
        // intentionally ignored.
    }
}

/// Async variant of [`consume_sse`] that reads SSE frames from an async
/// [`reqwest::Response`] using chunked transfer. Each `data:` payload is
/// forwarded to `handle_data`; returning `Ok(false)` ends the loop early.
pub async fn consume_sse_async<F>(
    mut response: reqwest::Response,
    mut handle_data: F,
) -> ApiResult<()>
where
    F: FnMut(&str) -> ApiResult<bool>,
{
    let mut buf = String::new();
    let mut pending: Vec<String> = Vec::new();

    while let Some(chunk) = response.chunk().await.map_err(ApiError::Http)? {
        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(newline_pos) = buf.find('\n') {
            let line = buf[..newline_pos].trim_end_matches('\r').to_string();
            buf = buf[newline_pos + 1..].to_string();

            if line.is_empty() {
                if !pending.is_empty() {
                    let payload = pending.join("\n");
                    pending.clear();
                    if !handle_data(&payload)? {
                        return Ok(());
                    }
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("data:") {
                pending.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
        }
    }

    // EOF — flush any trailing frame
    if !pending.is_empty() {
        let payload = pending.join("\n");
        let _ = handle_data(&payload)?;
    }
    Ok(())
}
