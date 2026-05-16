//! "Thinking…" spinner widget for the REPL.
//!
//! Runs on a background thread, prints a 2-line status over stderr
//! (spinner frame + rotating sub-message), and erases itself as soon
//! as `note_event` or `end_turn` fires. `begin_turn_with_overlay`
//! coordinates with `InputOverlay` so typed input stays visible below
//! the spinner without interleaving.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Thinking marker — one static "⠋ thinking…" string printed to stderr at
// the start of every turn and erased as soon as the first stream event
// arrives. No animation, no background ticker, no scroll region — those
// are what made the previous attempts clobber user typing and garble the
// output. Its sole job is to confirm the turn is alive during the dead
// period before the first token, after which real content takes over.
// ---------------------------------------------------------------------------

/// Shared flag coordinating the stream callback and the REPL turn hooks.
/// The only shared state is `visible` (has the marker been drawn and
/// not yet cleared?) plus a captured tty check so piped runs are a
/// silent no-op.
/// Animated "⏺ Thinking..." spinner that runs on a background thread.
/// Displays rotating sub-messages and tracks elapsed time.
/// Cleared when the first real response event arrives.
pub(super) struct ThinkingSpinner {
    tty: bool,
    stop: Arc<AtomicBool>,
    drawn: Arc<AtomicBool>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    start: Mutex<Option<std::time::Instant>>,
    // Set during begin_turn_with_overlay; used by note_event / end_turn
    // so they can acquire the shared lock and erase 3 lines when overlay is on line 3.
    term_lock_ref: Mutex<Option<Arc<Mutex<()>>>>,
    overlay_drawn_ref: Mutex<Option<Arc<AtomicBool>>>,
}

// Sub-messages shown below the spinner, rotating every ~2.5s
const THINKING_MSGS: &[&str] = &[
    "working through it",
    "thinking it over",
    "analyzing",
    "considering the approach",
    "reasoning through this",
    "figuring it out",
    "thinking",
    "processing",
];

// Sparkle spinner chars (macOS-safe), ping-pong animated
const SPINNER_BASE: &[char] = &['·', '✢', '✳', '✶', '✻', '✽'];

impl ThinkingSpinner {
    pub(super) fn new() -> Self {
        use std::io::IsTerminal;
        Self {
            tty: std::io::stderr().is_terminal(),
            stop: Arc::new(AtomicBool::new(false)),
            drawn: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(None),
            start: Mutex::new(None),
            term_lock_ref: Mutex::new(None),
            overlay_drawn_ref: Mutex::new(None),
        }
    }

    /// Expose the `drawn` flag so InputOverlay can read it.
    pub(super) fn drawn_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.drawn)
    }

    /// Start animated thinking indicator. Spawns a background thread.
    pub(super) fn begin_turn(&self) {
        if !self.tty {
            return;
        }
        self.stop.store(false, Ordering::Relaxed);
        self.drawn.store(false, Ordering::Relaxed);
        *self.start.lock().unwrap() = Some(std::time::Instant::now());

        let stop = Arc::clone(&self.stop);
        let drawn = Arc::clone(&self.drawn);
        let handle = std::thread::spawn(move || {
            // Build ping-pong frames: forward then reverse
            let mut frames: Vec<char> = SPINNER_BASE.to_vec();
            let mut rev = SPINNER_BASE.to_vec();
            rev.reverse();
            frames.extend(rev);

            let mut frame = 0usize;
            let mut msg_idx = 0usize;
            let mut ticks = 0usize;
            let mut first = true;

            // ~80ms per tick, change sub-message every ~2.5s (31 ticks)
            while !stop.load(Ordering::Relaxed) {
                let spinner = frames[frame % frames.len()];
                let msg = THINKING_MSGS[msg_idx % THINKING_MSGS.len()];

                if first {
                    eprint!("\x1b[32m● Thinking\x1b[0m\n\x1b[2m  └ {msg} {spinner}\x1b[0m");
                    drawn.store(true, Ordering::Relaxed);
                    first = false;
                } else {
                    // Erase exactly 2 lines (no \x1b[J which clears to end-of-screen)
                    // \x1b[1A = up 1, \x1b[K = clear this line, \n\r = next line col0, \x1b[K = clear, \x1b[1A = back up
                    eprint!("\x1b[1A\r\x1b[K\n\r\x1b[K\x1b[1A\x1b[32m● Thinking\x1b[0m\n\x1b[2m  └ {msg} {spinner}\x1b[0m");
                }
                let _ = std::io::stderr().flush();
                frame += 1;
                ticks += 1;
                if ticks % 31 == 0 {
                    msg_idx += 1;
                }
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        });
        *self.thread.lock().unwrap() = Some(handle);
    }

    /// Like `begin_turn` but coordinates with `InputOverlay` via a shared
    /// `term_lock`. The spinner thread acquires the lock before every write
    /// so it never races with the overlay thread. When the overlay has typed
    /// characters on screen the spinner draws a 3rd line (`❯ text`) and
    /// erases it as part of each animation frame.
    pub(super) fn begin_turn_with_overlay(
        &self,
        term_lock: Arc<Mutex<()>>,
        overlay_buf: Arc<Mutex<String>>,
        overlay_drawn: Arc<AtomicBool>,
    ) {
        if !self.tty {
            return;
        }
        *self.term_lock_ref.lock().unwrap() = Some(Arc::clone(&term_lock));
        *self.overlay_drawn_ref.lock().unwrap() = Some(Arc::clone(&overlay_drawn));
        self.stop.store(false, Ordering::Relaxed);
        self.drawn.store(false, Ordering::Relaxed);
        *self.start.lock().unwrap() = Some(std::time::Instant::now());

        let stop = Arc::clone(&self.stop);
        let drawn = Arc::clone(&self.drawn);

        let handle = std::thread::spawn(move || {
            let mut frames: Vec<char> = SPINNER_BASE.to_vec();
            let mut rev = SPINNER_BASE.to_vec();
            rev.reverse();
            frames.extend(rev);

            let mut frame = 0usize;
            let mut msg_idx = 0usize;
            let mut ticks = 0usize;
            // Did we draw the overlay line in the previous frame?
            let mut prev_has_overlay = false;

            while !stop.load(Ordering::Relaxed) {
                let spinner_c = frames[frame % frames.len()];
                let msg = THINKING_MSGS[msg_idx % THINKING_MSGS.len()];
                let ov = overlay_drawn.load(Ordering::Relaxed);
                let ov_buf = overlay_buf.lock().unwrap().clone();
                let has_overlay = ov && !ov_buf.is_empty();

                {
                    let _lk = term_lock.lock().unwrap();
                    if !drawn.load(Ordering::Relaxed) {
                        // First frame — draw spinner (+ overlay if already typed)
                        eprint!("\x1b[32m● Thinking\x1b[0m\n\x1b[2m  └ {msg} {spinner_c}\x1b[0m");
                        if has_overlay {
                            eprint!("\n\x1b[2m❯ {ov_buf}\x1b[0m");
                        }
                        drawn.store(true, Ordering::Relaxed);
                        prev_has_overlay = has_overlay;
                    } else {
                        // Subsequent frames: erase previous, redraw.
                        // Cursor is at: line 3 if prev_has_overlay, line 2 otherwise.
                        if prev_has_overlay {
                            // Up 2 → erase 3 lines → back to top
                            eprint!("\x1b[2A\r\x1b[K\n\r\x1b[K\n\r\x1b[K\x1b[2A");
                        } else {
                            // Up 1 → erase 2 lines → back to top
                            eprint!("\x1b[1A\r\x1b[K\n\r\x1b[K\x1b[1A");
                        }
                        eprint!("\x1b[32m● Thinking\x1b[0m\n\x1b[2m  └ {msg} {spinner_c}\x1b[0m");
                        if has_overlay {
                            eprint!("\n\x1b[2m❯ {ov_buf}\x1b[0m");
                        }
                        prev_has_overlay = has_overlay;
                    }
                    let _ = std::io::stderr().flush();
                }

                frame += 1;
                ticks += 1;
                if ticks % 31 == 0 {
                    msg_idx += 1;
                }
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        });
        *self.thread.lock().unwrap() = Some(handle);
    }

    /// Called on every stream event. Stops animation and prints elapsed time.
    pub(super) fn note_event(&self) {
        if !self.tty {
            return;
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.thread.lock() {
            if let Some(h) = guard.take() {
                let _ = h.join();
            }
        }
        let was_drawn = self.drawn.swap(false, Ordering::Relaxed);
        let elapsed = self
            .start
            .lock()
            .unwrap()
            .take()
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0);

        // Collect overlay state before clearing refs
        let term_lock_opt: Option<Arc<Mutex<()>>> = self.term_lock_ref.lock().unwrap().clone();
        let ov_drawn = self
            .overlay_drawn_ref
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(false);
        // Mark overlay as cleared so its thread doesn't redraw on line 3
        if let Some(ref a) = *self.overlay_drawn_ref.lock().unwrap() {
            a.store(false, Ordering::Relaxed);
        }
        *self.term_lock_ref.lock().unwrap() = None;
        *self.overlay_drawn_ref.lock().unwrap() = None;

        if was_drawn {
            // Hold shared lock while erasing so overlay thread doesn't race
            if let Some(ref lk) = term_lock_opt {
                let _lk = lk.lock().unwrap();
                if ov_drawn {
                    eprint!("\x1b[2A\r\x1b[K\n\r\x1b[K\n\r\x1b[K\x1b[2A");
                } else {
                    eprint!("\x1b[1A\r\x1b[K\n\r\x1b[K\x1b[1A");
                }
                if elapsed > 0.5 {
                    eprintln!("\x1b[2m⏺ thought for {elapsed:.1}s\x1b[0m");
                }
            } else {
                eprint!("\x1b[1A\r\x1b[K\n\r\x1b[K\x1b[1A");
                if elapsed > 0.5 {
                    eprintln!("\x1b[2m⏺ thought for {elapsed:.1}s\x1b[0m");
                }
            }
        }
        let _ = std::io::stderr().flush();
    }

    /// Cleanup if turn ended without any stream events.
    pub(super) fn end_turn(&self) {
        if !self.tty {
            return;
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.thread.lock() {
            if let Some(h) = guard.take() {
                let _ = h.join();
            }
        }
        let was_drawn = self.drawn.swap(false, Ordering::Relaxed);

        let term_lock_opt: Option<Arc<Mutex<()>>> = self.term_lock_ref.lock().unwrap().clone();
        let ov_drawn = self
            .overlay_drawn_ref
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(false);
        if let Some(ref a) = *self.overlay_drawn_ref.lock().unwrap() {
            a.store(false, Ordering::Relaxed);
        }
        *self.term_lock_ref.lock().unwrap() = None;
        *self.overlay_drawn_ref.lock().unwrap() = None;

        if was_drawn {
            if let Some(ref lk) = term_lock_opt {
                let _lk = lk.lock().unwrap();
                if ov_drawn {
                    eprint!("\x1b[2A\r\x1b[K\n\r\x1b[K\n\r\x1b[K\x1b[2A");
                } else {
                    eprint!("\x1b[1A\r\x1b[K\n\r\x1b[K\x1b[1A");
                }
            } else {
                eprint!("\x1b[1A\r\x1b[K\n\r\x1b[K\x1b[1A");
            }
        }
        let _ = std::io::stderr().flush();
        *self.start.lock().unwrap() = None;
    }
}
