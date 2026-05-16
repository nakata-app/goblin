//! Terminal input overlay, echo suppression, and width helpers.
//!
//! `InputOverlay` runs a typing buffer visible under the spinner while
//! the agent streams output (unix only — Windows uses the fallback
//! path). `disable_echo` / `EchoGuard` toggle termios for the
//! non-overlay REPL path. `erase_input_line` and `terminal_width`
//! are cross-platform terminal primitives used by repl.rs.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// InputOverlay — echo suppression during agent.run().
//
// Only disables terminal echo (ICANON kept on). Keystrokes still go into
// the kernel's canonical line buffer so backspace/line-editing work, but
// they don't appear on screen and don't interleave with stream output.
// After agent.run(), drain_pending_lines() harvests submitted lines.
// No background thread → no chars consumed from kernel buffer → no
// double-display or first-char-lost bugs.
// ---------------------------------------------------------------------------

#[cfg(unix)]
pub(super) struct InputOverlay {
    tty: bool,
    pub(super) buffer: Arc<Mutex<String>>,
    pub(super) queued: Arc<Mutex<std::collections::VecDeque<String>>>,
    pub(super) term_lock: Arc<Mutex<()>>,
    pub(super) drawn: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    saved: Mutex<Option<libc::termios>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// True once the first stream token arrives; controls scroll-region input bar.
    streaming_active: Arc<AtomicBool>,
    /// Terminal row count — captured when streaming activates.
    term_rows: Arc<std::sync::atomic::AtomicU32>,
}

/// Query terminal height via TIOCGWINSZ; returns 24 as a safe fallback.
#[cfg(unix)]
fn get_term_rows() -> u16 {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 {
            ws.ws_row
        } else {
            24
        }
    }
}

#[cfg(unix)]
impl InputOverlay {
    pub(super) fn new() -> Self {
        use std::io::IsTerminal;
        Self {
            tty: std::io::stdin().is_terminal(),
            buffer: Arc::new(Mutex::new(String::new())),
            queued: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            term_lock: Arc::new(Mutex::new(())),
            drawn: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            saved: Mutex::new(None),
            thread: Mutex::new(None),
            streaming_active: Arc::new(AtomicBool::new(false)),
            term_rows: Arc::new(std::sync::atomic::AtomicU32::new(24)),
        }
    }

    /// Start capturing input. `spinner_drawn` is the spinner's own `drawn`
    /// flag — when true the spinner occupies 2 lines and the overlay must
    /// write on line 3 (below them). Both the overlay thread and the spinner
    /// thread share `self.term_lock` so their writes never interleave.
    pub(super) fn start(&self, spinner_drawn: Arc<AtomicBool>) {
        if !self.tty {
            return;
        }
        self.stop.store(false, Ordering::Relaxed);
        self.drawn.store(false, Ordering::Relaxed);
        self.streaming_active.store(false, Ordering::Relaxed);
        self.buffer.lock().unwrap().clear();
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                return;
            }
            *self.saved.lock().unwrap() = Some(t);
            let mut raw = t;
            raw.c_lflag &= !(libc::ECHO | libc::ICANON);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 1;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
        }

        let stop = Arc::clone(&self.stop);
        let buffer = Arc::clone(&self.buffer);
        let queued = Arc::clone(&self.queued);
        let drawn = Arc::clone(&self.drawn);
        let lock = Arc::clone(&self.term_lock);
        let streaming_active = Arc::clone(&self.streaming_active);
        let term_rows = Arc::clone(&self.term_rows);

        let handle = std::thread::spawn(move || {
            let mut esc_skip: u8 = 0;
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let mut byte = [0u8; 1];
                let n = unsafe {
                    libc::read(
                        libc::STDIN_FILENO,
                        byte.as_mut_ptr() as *mut libc::c_void,
                        1,
                    )
                };
                if n <= 0 {
                    continue;
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let b = byte[0];
                if esc_skip > 0 {
                    esc_skip -= 1;
                    continue;
                }
                if b == 0x1b {
                    esc_skip = 2;
                    continue;
                }

                match b {
                    0x03 => break,
                    b'\r' | b'\n' => {
                        let line = {
                            let mut buf = buffer.lock().unwrap();
                            let l = buf.trim().to_string();
                            buf.clear();
                            l
                        };
                        let _lk = lock.lock().unwrap();
                        let sa = streaming_active.load(Ordering::Relaxed);
                        let sp = spinner_drawn.load(Ordering::Relaxed);
                        if sa {
                            // Streaming mode: clear input bar row via absolute positioning
                            let rows = term_rows.load(Ordering::Relaxed) as u16;
                            eprint!("\x1b[s\x1b[{rows};1H\x1b[2K\x1b[u");
                            drawn.store(false, Ordering::Relaxed);
                        } else if drawn.swap(false, Ordering::Relaxed) {
                            if sp {
                                // Overlay was on line 3; clear it and go back to line 2
                                eprint!("\r\x1b[K\x1b[1A");
                            } else {
                                eprint!("\r\x1b[K");
                            }
                        }
                        if !line.is_empty() {
                            queued.lock().unwrap().push_back(line);
                            // Only show ↵ queued when not in streaming or spinner mode
                            if !sa && !sp {
                                eprintln!("\x1b[2m  ↵ queued\x1b[0m");
                            }
                        }
                        let _ = std::io::stderr().flush();
                    }
                    0x7f | 0x08 => {
                        let display = {
                            let mut buf = buffer.lock().unwrap();
                            buf.pop();
                            buf.clone()
                        };
                        let _lk = lock.lock().unwrap();
                        let sa = streaming_active.load(Ordering::Relaxed);
                        if sa {
                            // Streaming mode: update input bar via absolute positioning
                            let rows = term_rows.load(Ordering::Relaxed) as u16;
                            eprint!("\x1b[s\x1b[{rows};1H\x1b[2K");
                            if !display.is_empty() {
                                eprint!("\x1b[2m❯ {display}\x1b[0m");
                            }
                            eprint!("\x1b[u");
                            drawn.store(!display.is_empty(), Ordering::Relaxed);
                            let _ = std::io::stderr().flush();
                        } else if drawn.load(Ordering::Relaxed) {
                            let sp = spinner_drawn.load(Ordering::Relaxed);
                            if sp && display.is_empty() {
                                // Buffer cleared: leave line 3, go up to spinner's line 2
                                eprint!("\r\x1b[K\x1b[1A");
                                drawn.store(false, Ordering::Relaxed);
                            } else {
                                eprint!("\r\x1b[K\x1b[2m❯ {display}\x1b[0m");
                            }
                            let _ = std::io::stderr().flush();
                        }
                    }
                    0x20..=0x7e => {
                        let display = {
                            let mut buf = buffer.lock().unwrap();
                            buf.push(b as char);
                            buf.clone()
                        };
                        let _lk = lock.lock().unwrap();
                        let sa = streaming_active.load(Ordering::Relaxed);
                        let sp = spinner_drawn.load(Ordering::Relaxed);
                        if sa {
                            // Streaming mode: write at absolute bottom row via
                            // save-cursor / move / draw / restore-cursor so the
                            // model output position is never disturbed.
                            let rows = term_rows.load(Ordering::Relaxed) as u16;
                            eprint!("\x1b[s\x1b[{rows};1H\x1b[2K\x1b[2m❯ {display}\x1b[0m\x1b[u");
                            drawn.store(true, Ordering::Relaxed);
                        } else if sp {
                            // Spinner mode: write on line 3 below spinner
                            if drawn.load(Ordering::Relaxed) {
                                eprint!("\r\x1b[K\x1b[2m❯ {display}\x1b[0m");
                            } else {
                                eprint!("\n\x1b[2m❯ {display}\x1b[0m");
                                drawn.store(true, Ordering::Relaxed);
                            }
                        } else {
                            if drawn.load(Ordering::Relaxed) {
                                eprint!("\r\x1b[K\x1b[2m❯ {display}\x1b[0m");
                            } else {
                                eprint!("\x1b[2m❯ {display}\x1b[0m");
                                drawn.store(true, Ordering::Relaxed);
                            }
                        }
                        let _ = std::io::stderr().flush();
                    }
                    _ => {}
                }
            }
        });
        *self.thread.lock().unwrap() = Some(handle);
    }

    /// Clear input line before stream output.
    pub(super) fn before_output(&self) {
        if !self.tty {
            return;
        }
        let _lk = self.term_lock.lock().unwrap();

        if !self.streaming_active.load(Ordering::Relaxed) {
            // First stream event this turn: activate the scroll-region input bar.
            // \x1b[1;{N-1}r   — restrict scrolling to rows 1..N-1; row N = input bar.
            //                    The VT100 spec moves cursor to home after this.
            // \x1b[{N};1H     — move to input bar row, clear it.
            // \x1b[2K
            // \x1b[{N-1};1H   — park cursor at bottom of scroll region so model
            //                    output starts there and scrolls upward within the region.
            let rows = get_term_rows().max(4);
            self.term_rows.store(rows as u32, Ordering::Relaxed);
            self.streaming_active.store(true, Ordering::Relaxed);
            eprint!(
                "\x1b[1;{sr}r\x1b[{ib};1H\x1b[2K\x1b[{sr};1H",
                sr = rows - 1,
                ib = rows,
            );
            let _ = std::io::stderr().flush();
            // drawn is already false; input bar was just cleared
        }

        // In streaming mode the scroll region keeps the input bar row (row N)
        // completely separate from model output — no clearing needed here.
        // Clearing it on every TextDelta caused the typed text to flash/disappear.
    }

    /// Redraw input line after output that ends with newline (e.g. tool result).
    pub(super) fn after_output(&self) {
        if !self.tty {
            return;
        }
        let _lk = self.term_lock.lock().unwrap();
        let buf = self.buffer.lock().unwrap().clone();

        if self.streaming_active.load(Ordering::Relaxed) {
            // Input bar mode: absolute positioning.
            let rows = self.term_rows.load(Ordering::Relaxed) as u16;
            eprint!("\x1b[s\x1b[{rows};1H\x1b[2K");
            if !buf.is_empty() {
                eprint!("\x1b[2m❯ {buf}\x1b[0m");
            }
            eprint!("\x1b[u");
            self.drawn.store(!buf.is_empty(), Ordering::Relaxed);
        } else {
            eprint!("\x1b[2m❯ {buf}\x1b[0m");
            self.drawn.store(true, Ordering::Relaxed);
        }
        let _ = std::io::stderr().flush();
    }

    /// Returns (queued_lines, partial_buffer).
    pub(super) fn stop_and_collect(&self) -> (std::collections::VecDeque<String>, String) {
        if !self.tty {
            return (std::collections::VecDeque::new(), String::new());
        }
        self.stop.store(true, Ordering::Relaxed);
        // 1. Wait for thread to stop reading stdin (max ~100ms VTIME)
        if let Ok(mut g) = self.thread.lock() {
            if let Some(h) = g.take() {
                let _ = h.join();
            }
        }
        // 2. Restore terminal echo
        if let Some(saved) = *self.saved.lock().unwrap() {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved);
            }
        }
        // 3. If streaming input bar was active, reset scroll region and clear bar row.
        //    Do this BEFORE clearing drawn so the terminal is left in a clean state.
        {
            let _lk = self.term_lock.lock().unwrap();
            if self.streaming_active.swap(false, Ordering::Relaxed) {
                let rows = self.term_rows.load(Ordering::Relaxed) as u16;
                // Reset scroll region to full screen, clear the input bar row.
                eprint!("\x1b[r\x1b[{rows};1H\x1b[2K");
                let _ = std::io::stderr().flush();
            } else if self.drawn.swap(false, Ordering::Relaxed) {
                eprint!("\r\x1b[K");
                let _ = std::io::stderr().flush();
            }
            self.drawn.store(false, Ordering::Relaxed);
        }
        let partial = self.buffer.lock().unwrap().clone();
        let mut q = self.queued.lock().unwrap();
        (std::mem::take(&mut *q), partial)
    }
}

#[cfg(unix)]
#[allow(dead_code)] // kept for non-overlay fallback path
pub(super) fn disable_echo() -> Option<EchoGuard> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return None;
    }
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut termios) == 0 {
            let saved = termios;
            termios.c_lflag &= !libc::ECHO;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &termios);
            Some(EchoGuard { saved })
        } else {
            None
        }
    }
}

/// Read all complete lines (`\n`-terminated) currently sitting in
/// stdin's input buffer, without blocking. Used after `agent.run`
/// returns to capture any prompts the user typed *during* the
/// streaming response so they can be queued as the next REPL input(s)
/// instead of being silently discarded by `flush_pending_input`.
///
/// Behavior notes:
/// - Only complete (Enter-submitted) lines are captured. Anything
///   typed but not yet submitted stays in the line discipline edit
///   buffer and is unreachable from a regular `read()`. We don't
///   try to recover it.
/// - Lines containing control characters (escape sequences from
///   stray arrow keys, etc.) are filtered out so we never replay
///   garbage as a model prompt.
/// - The drain is hard-capped at 64 KiB to keep a runaway paste
///   from eating unbounded memory.
/// - Non-blocking mode on stdin is set via `fcntl(O_NONBLOCK)` and
///   restored via an RAII guard so a panic mid-drain can't leave
///   the fd in non-blocking mode (which would break rustyline).
#[cfg(unix)]
pub(super) fn drain_pending_lines() -> Vec<String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Vec::new();
    }
    let fd = libc::STDIN_FILENO;
    let original = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if original < 0 {
        return Vec::new();
    }
    struct NonBlockGuard {
        fd: i32,
        original: i32,
    }
    impl Drop for NonBlockGuard {
        fn drop(&mut self) {
            unsafe {
                libc::fcntl(self.fd, libc::F_SETFL, self.original);
            }
        }
    }
    let _guard = NonBlockGuard { fd, original };
    unsafe {
        if libc::fcntl(fd, libc::F_SETFL, original | libc::O_NONBLOCK) < 0 {
            return Vec::new();
        }
    }

    let mut all: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            // EAGAIN/EWOULDBLOCK (no more data) or EOF — done.
            break;
        }
        all.extend_from_slice(&buf[..n as usize]);
        if all.len() > 64 * 1024 {
            break;
        }
    }

    // Do NOT flush partial input — let it flow into the next rustyline
    // prompt so the user can continue editing where they left off.
    // Concealed mode (SGR 8) keeps keystrokes invisible during streaming,
    // and rustyline picks up the buffered characters when it switches to
    // raw mode.

    if all.is_empty() {
        return Vec::new();
    }

    let text = String::from_utf8_lossy(&all);
    let parts: Vec<&str> = text.split('\n').collect();
    if parts.len() < 2 {
        // No complete line — only a trailing partial — discard.
        return Vec::new();
    }
    // Everything except the last element was followed by a `\n`.
    // The last element is the trailing partial input we drop.
    let complete = &parts[..parts.len() - 1];
    let mut lines: Vec<String> = Vec::new();
    for raw in complete {
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        // Reject control chars except tab — guards against arrow
        // keys, function keys, and other terminal escape sequences
        // accidentally landing in the buffer and then being sent
        // to the model as a "prompt".
        if line.chars().any(|c| c.is_control() && c != '\t') {
            continue;
        }
        lines.push(line.to_string());
    }
    lines
}

#[cfg(not(unix))]
pub(super) fn drain_pending_lines() -> Vec<String> {
    Vec::new()
}

/// RAII guard that restores terminal echo on drop. Prevents the
/// terminal from staying in no-echo mode after panics or early returns.
///
/// On drop the guard *also* flushes any input the user typed while echo
/// was disabled. Without this flush, keystrokes typed during streaming
/// (which never appeared on screen) would be silently buffered in the
/// tty driver and replayed into the next `readline` call — the user
/// would see ghost characters they don't remember typing, or worse,
/// the buffered chars would trigger rustyline edit shortcuts. The
/// flush makes "out of sight" mean "discarded" for the duration of
/// the streaming turn.
#[cfg(unix)]
#[allow(dead_code)] // disabled in 0.5.x — see flush_pending_input above
pub(super) struct EchoGuard {
    saved: libc::termios,
}

#[cfg(unix)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        unsafe {
            // Discard anything the user typed while echo was off so it
            // doesn't replay into the next prompt.
            libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
    }
}

/// Replace the raw prompt line with a styled version showing the user's input.
/// Moves up, clears, and reprints as a dimmed echo so the user can see
/// what they typed while keeping the output clean.
pub(super) fn erase_input_line(input: &str, prompt_visible_width: usize) {
    use std::io::IsTerminal;
    if !std::io::stderr().is_terminal() {
        return;
    }
    let display_width: usize = input.chars().count();
    let visible_len = prompt_visible_width + display_width;
    let term_width = terminal_width().unwrap_or(80);
    let rows = ((visible_len as f64) / (term_width as f64)).ceil().max(1.0) as usize;
    // Move up and clear the raw prompt lines
    let mut esc = String::new();
    for _ in 0..rows {
        esc.push_str("\x1b[A\x1b[2K");
    }
    esc.push('\r');
    eprint!("{esc}");
    // Reprint the user's message in a compact styled form. We used
    // to truncate at 120 chars with a trailing "..." for readability,
    // but the user explicitly does not want their own input hidden —
    // they need to see what they actually typed in the scrollback,
    // long pastes included. The terminal will line-wrap naturally.
    let trimmed = input.trim();
    if !trimmed.is_empty() {
        eprintln!("\x1b[1m> {trimmed}\x1b[0m");
    }
    let _ = std::io::stderr().flush();
}

#[cfg(unix)]
pub(super) fn terminal_width() -> Option<usize> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            Some(ws.ws_col as usize)
        } else {
            None
        }
    }
}

#[cfg(not(unix))]
pub(super) fn terminal_width() -> Option<usize> {
    None
}
