//! Prototype: the eval progress readout rendered with **ratatui's inline
//! viewport** instead of npd's hand-rolled `console` renderer (`src/live.rs`).
//! It's a standalone comparison driven by *fake* shard progress — nothing here
//! is wired into npd, and ratatui/crossterm are dev-dependencies only.
//!
//!     cargo run --example ratatui_eval
//!
//! What it demonstrates versus the custom renderer:
//!
//! * Resize is handled *for free*: ratatui re-queries the terminal size every
//!   draw and re-lays out a diffed frame to the current width, so widening or
//!   narrowing mid-run never leaves reflow garbage — the thing that takes care
//!   in a hand-rolled renderer (truncate-to-width every frame) is intrinsic.
//! * The cost is a heavier model and explicit teardown: it takes over a fixed
//!   inline region, hides the cursor, and you *must* restore it on every exit
//!   path — including ^C and any early error. We install a `ctrlc` handler so ^C
//!   sets a flag and the loop breaks, and a `Drop` guard shows the cursor no
//!   matter how we leave. Forget that and a crash/^C leaves the cursor hidden.
//! * It needs a real interactive terminal: the inline viewport anchors itself by
//!   *reading the cursor position* (a DSR query the terminal must answer), so it
//!   errors out under a pipe or a pty that doesn't respond (some CI, some
//!   capture harnesses). The `console` renderer uses only relative moves and has
//!   no such requirement — so run this one in an actual terminal.
//!
//! Try it: run it, resize the window while it's going, and hit ^C partway.

use std::io::{self, Write, stderr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::TerminalOptions;
use ratatui::Viewport;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::execute;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

struct Eval {
    short: &'static str,
    system: &'static str,
    total_shards: usize,
    final_attrs: usize,
    /// Seconds after start at which this eval begins (staggered, like the FIFO
    /// shard queue draining one eval before the next).
    offset: f64,
}

struct Snap {
    done: usize,
    running: usize,
    attrs: usize,
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Restores the cursor on *every* exit path — normal return, ^C, or an early
/// `?`. Without it a hidden cursor leaks into the user's shell.
struct CursorGuard;
impl Drop for CursorGuard {
    fn drop(&mut self) {
        let _ = execute!(stderr(), Show);
    }
}

fn snapshot(ev: &Eval, elapsed: f64) -> Snap {
    let prog = ((elapsed - ev.offset) / 9.0).clamp(0.0, 1.0);
    let done = (prog * ev.total_shards as f64) as usize;
    let running = if prog > 0.0 && prog < 1.0 {
        7.min(ev.total_shards - done)
    } else {
        0
    };
    Snap {
        done,
        running,
        attrs: (prog * ev.final_attrs as f64) as usize,
    }
}

fn eval_line(ev: &Eval, s: &Snap, sys_w: usize) -> String {
    let verb = if s.done == ev.total_shards {
        "evaluated"
    } else {
        "evaluating"
    };
    let shards = if s.running > 0 {
        format!("{}+{}/{}", s.done, s.running, ev.total_shards)
    } else {
        format!("{}/{}", s.done, ev.total_shards)
    };
    format!(
        "  {verb:<10} {short} {system:<sys_w$}  {attrs:>6} attrs  {shards} shards",
        short = ev.short,
        system = ev.system,
        attrs = s.attrs,
    )
}

fn main() -> io::Result<()> {
    let evals = [
        Eval {
            short: "3af24d1a5fc8",
            system: "x86_64-linux",
            total_shards: 69,
            final_attrs: 113976,
            offset: 0.0,
        },
        Eval {
            short: "3af24d1a5fc8",
            system: "aarch64-linux",
            total_shards: 69,
            final_attrs: 98010,
            offset: 2.0,
        },
        Eval {
            short: "aafbabe44c3b",
            system: "x86_64-linux",
            total_shards: 69,
            final_attrs: 113900,
            offset: 4.0,
        },
        Eval {
            short: "aafbabe44c3b",
            system: "aarch64-linux",
            total_shards: 69,
            final_attrs: 97980,
            offset: 6.0,
        },
    ];
    let sys_w = evals.iter().map(|e| e.system.len()).max().unwrap_or(0);

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst)).expect("install ^C handler");
    }

    execute!(stderr(), Hide)?;
    let _cursor = CursorGuard; // shows the cursor again on any exit path
    let mut term = Terminal::with_options(
        CrosstermBackend::new(stderr()),
        TerminalOptions {
            viewport: Viewport::Inline((evals.len() + 1) as u16),
        },
    )?;

    let start = Instant::now();
    let mut finished = false;
    while running.load(Ordering::SeqCst) && !finished {
        let elapsed = start.elapsed().as_secs_f64();
        let snaps: Vec<Snap> = evals.iter().map(|e| snapshot(e, elapsed)).collect();
        finished = snaps
            .iter()
            .zip(&evals)
            .all(|(s, e)| s.done == e.total_shards);

        let (mut d, mut r, mut t, mut a) = (0, 0, 0, 0);
        for (e, s) in evals.iter().zip(&snaps) {
            d += s.done;
            r += s.running;
            t += e.total_shards;
            a += s.attrs;
        }
        let verb = if d == t { "evaluated" } else { "evaluating" };
        let shards = if r > 0 {
            format!("{d}+{r}/{t}")
        } else {
            format!("{d}/{t}")
        };
        let spin = SPINNER[(elapsed * 10.0) as usize % SPINNER.len()];

        let mut lines: Vec<Line> = evals
            .iter()
            .zip(&snaps)
            .map(|(e, s)| Line::raw(eval_line(e, s, sys_w)))
            .collect();
        lines.push(Line::from(vec![
            Span::styled(spin, Style::default().fg(Color::Cyan)),
            Span::raw(format!(
                " ⏱ {}s {verb} {shards} shards, {a} attrs",
                elapsed as u64
            )),
        ]));

        term.draw(|f| f.render_widget(Paragraph::new(lines), f.area()))?;
        if finished {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    // Teardown on *every* exit path (normal finish or ^C): clear the viewport,
    // show the cursor, then print the summary as ordinary scrollback lines.
    term.clear()?;
    execute!(stderr(), Show)?;
    let elapsed = start.elapsed().as_secs_f64();
    let snaps: Vec<Snap> = evals.iter().map(|e| snapshot(e, elapsed)).collect();
    let mut err = stderr();
    for (e, s) in evals.iter().zip(&snaps) {
        writeln!(err, "{}", eval_line(e, s, sys_w))?;
    }
    Ok(())
}
