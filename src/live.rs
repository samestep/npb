//! A small inline multi-line live display — the eval progress readout
//! (`crate::eval`). It redraws a block of lines in place on stderr, each line
//! **truncated to the current terminal width** so it always occupies exactly
//! one row. That one invariant is the whole point: moving the cursor up `n` rows
//! then lands on the block's top even after the window is resized, whereas
//! indicatif pads every line out to the full width — which reflows into garbage
//! the moment the width changes (its cursor math is fixed at the *previous*
//! width). Truncated content leaves nothing to reflow.
//!
//! **Flicker-free, especially over a laggy SSH link.** A frame is built as one
//! string and written once (one packet, not one per line), and:
//! - content is *overwritten in place* then the tail cleared (`content` + `\x1b[K`),
//!   never blanked first (`\x1b[2K` then write) — so there's no blank flash while
//!   the new bytes are in flight;
//! - lines unchanged since the last frame are skipped (the cursor just steps
//!   over them), so a steady line isn't rewritten 10×/s;
//! - the whole frame is wrapped in the *synchronized output* private mode
//!   (`\x1b[?2026h`…`l`), so terminals that support it (iTerm2, kitty, WezTerm,
//!   tmux ≥3.4) render it atomically — no tearing — and others ignore it.
//!
//! Render-only: no raw mode, no alternate screen, cursor left visible. So a ^C
//! mid-run just leaves the last (short, unpadded) block on screen, which reflows
//! like ordinary command output rather than the old full-width mess — no signal
//! handler required to keep resize sane.
//!
//! ratatui's inline viewport was the other renderer considered: it re-queries the
//! width and re-lays out a diffed frame each draw, so resize is free — but it must
//! hide the cursor and restore it on *every* exit path including ^C (a missed
//! teardown leaks a hidden cursor into the shell), and it anchors itself with a DSR
//! cursor-position query the terminal must answer, so it errors under a pipe or a
//! non-interactive pty. This relative-move renderer needs neither.

use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use console::{Term, style, truncate_str};

/// Braille spinner frames (indicatif's default set).
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The cyan spinner glyph for tick `n` — the leading char of a timer line.
/// Callers advance `n` once per redraw to animate it.
pub fn spinner(n: usize) -> String {
    style(SPINNER[n % SPINNER.len()]).cyan().to_string()
}

/// A block of lines redrawn in place. `drawn` is the number of rows the last
/// frame occupied — equal to the line count, since every line is one row.
pub struct Live {
    term: Term,
    drawn: usize,
    /// Lines shown last frame, and the width they were truncated at — so a frame
    /// only rewrites the lines that changed (and forces a full redraw on resize).
    prev: Vec<String>,
    prev_width: usize,
}

impl Live {
    pub fn new() -> Self {
        Self {
            term: Term::stderr(),
            drawn: 0,
            prev: Vec::new(),
            prev_width: 0,
        }
    }

    fn width(&self) -> usize {
        self.term.size_checked().map_or(80, |(_, w)| w as usize)
    }

    /// Redraw the block in place. A no-op on a non-terminal stderr (piped / CI):
    /// there is no cursor to move, and the caller's final summary still prints.
    pub fn draw(&mut self, lines: &[String]) {
        if !self.term.is_term() {
            return;
        }
        let w = self.width();
        // A resize changes every line's truncation, so redraw all lines then.
        let full = w != self.prev_width || lines.len() != self.prev.len();

        let mut buf = String::from("\x1b[?2026h"); // begin synchronized update
        if self.drawn > 0 {
            let _ = write!(buf, "\x1b[{}A", self.drawn); // up to the block's top row
        }
        buf.push('\r');
        for (i, line) in lines.iter().enumerate() {
            if full || self.prev.get(i).map(String::as_str) != Some(line.as_str()) {
                // Overwrite in place, then clear only the tail — no blank flash.
                buf.push_str(&truncate_str(line, w, ""));
                buf.push_str("\x1b[K");
            }
            buf.push_str("\r\n"); // step to column 0 of the next row
        }
        // Fewer lines than last frame? Erase the now-orphaned rows below.
        if lines.len() < self.drawn {
            buf.push_str("\x1b[J");
        }
        buf.push_str("\x1b[?2026l"); // end synchronized update
        let _ = self.term.write_str(&buf);
        let _ = self.term.flush();

        self.drawn = lines.len();
        self.prev = lines.to_vec();
        self.prev_width = w;
    }

    /// Emit `msg` as permanent output *above* the live block (a one-off note,
    /// e.g. a requeued shard). The block is erased and reappears on the next
    /// [`Live::draw`], below the now-permanent message.
    pub fn print_above(&mut self, msg: &str) {
        if !self.term.is_term() {
            eprintln!("{msg}");
            return;
        }
        let w = self.width();
        let mut buf = String::from("\x1b[?2026h");
        if self.drawn > 0 {
            let _ = write!(buf, "\x1b[{}A", self.drawn);
        }
        buf.push_str("\r\x1b[J"); // to the block's top, erase it and everything below
        for l in msg.lines() {
            buf.push_str(&truncate_str(l, w, ""));
            buf.push_str("\x1b[K\r\n");
        }
        buf.push_str("\x1b[?2026l");
        let _ = self.term.write_str(&buf);
        let _ = self.term.flush();
        self.drawn = 0;
        self.prev.clear(); // next draw redraws the block in full
    }

    /// Erase the block, leaving the cursor at its top. The caller then prints a
    /// clean, unpadded final summary as ordinary output.
    pub fn clear(&mut self) {
        if self.term.is_term() && self.drawn > 0 {
            let _ = self
                .term
                .write_str(&format!("\x1b[{}A\r\x1b[J", self.drawn));
            let _ = self.term.flush();
        }
        self.drawn = 0;
        self.prev.clear();
    }
}

/// A handle into a running [`with_live`] block, handed to the worker body so it
/// can emit permanent output *above* the animated region (a one-off note like a
/// requeued shard). [`Copy`] so the body can share it across its own workers.
#[derive(Clone, Copy)]
pub struct LiveHandle<'a> {
    display: &'a Mutex<Live>,
}

impl LiveHandle<'_> {
    /// Print `msg` as permanent output above the live block; the block redraws
    /// below it on the next frame. Thread-safe — the workers and the refresher
    /// share the one `Live` behind a mutex.
    pub fn note(&self, msg: &str) {
        self.display.lock().unwrap().print_above(msg);
    }
}

/// Run `body` while a refresher thread animates a live progress block on stderr.
///
/// This is npd's single progress-display primitive: every phase that shows a
/// live readout — the shard scheduler ([`crate::eval::run_shards`], which backs
/// eval, `--tests`, enumeration, and instantiation) and the cache probe
/// ([`crate::build`]) — drives it through here, so they all animate identically
/// (a steady 100 ms redraw that keeps the spinner + timer moving even while the
/// work itself is silent) and tear down identically. `frame(tick)` returns the
/// block's lines for tick `tick` — the caller composes its own spinner/timer via
/// [`spinner`]/[`human_elapsed`] — and is only ever called from the refresher,
/// reading whatever atomics `body`'s workers bump (so those need no locking).
/// When `body` returns, the block is erased (the caller then prints any frozen
/// summary as ordinary output) and `body`'s value is returned; `body` gets a
/// [`LiveHandle`] for notes above the block.
pub fn with_live<R>(
    frame: impl Fn(usize) -> Vec<String> + Sync,
    body: impl FnOnce(LiveHandle<'_>) -> R,
) -> R {
    let display = Mutex::new(Live::new());
    let done = AtomicBool::new(false);
    let mut out = None;
    thread::scope(|s| {
        let (display, done, frame) = (&display, &done, &frame);
        s.spawn(move || {
            let mut tick = 0usize;
            while !done.load(Ordering::Relaxed) {
                display.lock().unwrap().draw(&frame(tick));
                thread::sleep(Duration::from_millis(100));
                tick += 1;
            }
        });
        out = Some(body(LiveHandle { display }));
        done.store(true, Ordering::Relaxed);
    });
    display.lock().unwrap().clear();
    out.unwrap()
}

/// `indicatif`'s `{elapsed}` (its `HumanDuration` in alternate form): the single
/// largest sensible unit, e.g. `51s`, `2m`, `1h`. Ported so dropping indicatif
/// doesn't change how the timer reads.
pub fn human_elapsed(d: Duration) -> String {
    // (seconds-per-unit, suffix), largest first.
    const UNITS: &[(u64, &str)] = &[
        (365 * 24 * 3600, "y"),
        (7 * 24 * 3600, "w"),
        (24 * 3600, "d"),
        (3600, "h"),
        (60, "m"),
        (1, "s"),
    ];
    let secs = d.as_secs();
    // Pick the unit at which rounding `secs` reads naturally (matches indicatif's
    // threshold: switch up a unit once we're within half of it).
    let mut idx = 0;
    for (i, &(cur, _)) in UNITS.iter().enumerate() {
        idx = i;
        match UNITS.get(i + 1) {
            Some(&(next, _)) if secs + next / 2 >= cur + cur / 2 => break,
            _ => continue,
        }
    }
    let (unit, suffix) = UNITS[idx];
    let mut t = ((secs as f64) / (unit as f64)).round() as u64;
    // Non-second units never show "1" (indicatif clamps to 2), so we jump
    // straight from e.g. "89s" to "2m".
    if idx < UNITS.len() - 1 {
        t = t.max(2);
    }
    format!("{t}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_matches_indicatif_shape() {
        assert_eq!(human_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(human_elapsed(Duration::from_secs(51)), "51s");
        assert_eq!(human_elapsed(Duration::from_secs(89)), "89s");
        // switches to minutes at 90s, and never shows "1m"
        assert_eq!(human_elapsed(Duration::from_secs(90)), "2m");
        assert_eq!(human_elapsed(Duration::from_secs(120)), "2m");
        assert_eq!(human_elapsed(Duration::from_secs(180)), "3m");
        // indicatif stays in minutes until ~1.5h and never shows "1h" (min-2
        // clamp), so 1h reads "60m" and it jumps straight to "2h".
        assert_eq!(human_elapsed(Duration::from_secs(3600)), "60m");
        assert_eq!(human_elapsed(Duration::from_secs(5400)), "2h");
    }
}
