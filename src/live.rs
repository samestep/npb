//! A small inline multi-line live display — the eval progress readout
//! (`crate::eval`). It redraws a block of lines in place on stderr, each line
//! **truncated to the current terminal width** so it always occupies exactly
//! one row. That one invariant is the whole point: `move_cursor_up(n)` then
//! lands on the block's top even after the window is resized, whereas indicatif
//! pads every line out to the full width — which reflows into garbage the moment
//! the width changes (its cursor math is fixed at the *previous* width). We clear
//! stale cells with `\x1b[K`/`\x1b[0J` (via `console`) instead of overwriting
//! with spaces, so a shorter line leaves nothing behind and nothing to reflow.
//!
//! Render-only: no raw mode, no alternate screen, cursor left visible. So a ^C
//! mid-run just leaves the last (short, unpadded) block on screen, which reflows
//! like ordinary command output rather than the old full-width mess — no signal
//! handler required to keep resize sane.

use std::io;
use std::time::Duration;

use console::{Term, truncate_str};

/// A block of lines redrawn in place. `drawn` is the number of rows the last
/// frame occupied — equal to the line count, since every line is one row.
pub struct Live {
    term: Term,
    drawn: usize,
    /// Last lines drawn, so [`Live::print_above`] can repaint the block under a
    /// one-off message without the caller re-supplying them.
    last: Vec<String>,
}

impl Live {
    pub fn new() -> Self {
        Self {
            term: Term::stderr(),
            drawn: 0,
            last: Vec::new(),
        }
    }

    fn width(&self) -> usize {
        self.term.size_checked().map_or(80, |(_, w)| w as usize)
    }

    /// Redraw the block in place. A no-op on a non-terminal stderr (piped / CI):
    /// there is no cursor to move, and the caller's final summary still prints.
    pub fn draw(&mut self, lines: &[String]) {
        self.last = lines.to_vec();
        if self.term.is_term() {
            let _ = self.repaint();
        }
    }

    fn repaint(&mut self) -> io::Result<()> {
        let w = self.width();
        if self.drawn > 0 {
            self.term.move_cursor_up(self.drawn)?;
        }
        for line in &self.last {
            // `clear_line` is `\r\x1b[2K`: back to column 0 and erase the row, so
            // a line shorter than last frame's leaves no tail (no space-padding).
            self.term.clear_line()?;
            self.term.write_str(&truncate_str(line, w, ""))?;
            self.term.write_str("\n")?;
        }
        // Fewer lines than last frame? Erase the now-orphaned rows below.
        if self.last.len() < self.drawn {
            self.term.clear_to_end_of_screen()?;
        }
        self.term.flush()?;
        self.drawn = self.last.len();
        Ok(())
    }

    /// Emit `msg` as permanent output *above* the live block (a one-off note,
    /// e.g. a requeued shard), then repaint the block beneath it.
    pub fn print_above(&mut self, msg: &str) {
        if !self.term.is_term() {
            eprintln!("{msg}");
            return;
        }
        let _ = (|| -> io::Result<()> {
            let w = self.width();
            if self.drawn > 0 {
                self.term.move_cursor_up(self.drawn)?;
            }
            self.term.clear_to_end_of_screen()?; // wipe the block region
            for l in msg.lines() {
                self.term.write_str(&truncate_str(l, w, ""))?;
                self.term.write_str("\n")?;
            }
            self.drawn = 0;
            self.repaint() // block reappears below the now-permanent message
        })();
    }

    /// Erase the block, leaving the cursor at its top. The caller then prints a
    /// clean, unpadded final summary as ordinary output.
    pub fn clear(&mut self) {
        if self.term.is_term() && self.drawn > 0 {
            let _ = self.term.move_cursor_up(self.drawn);
            let _ = self.term.clear_to_end_of_screen();
            let _ = self.term.flush();
        }
        self.drawn = 0;
        self.last.clear();
    }
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
