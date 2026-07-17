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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use console::{Term, truncate_str};

/// Braille spinner frames (indicatif's default set).
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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

/// Run `body` while a refresher thread reflects `tree`'s progress on stderr,
/// then freeze it — npd's single progress-display primitive for the whole
/// pre-build phase (resolution → probe). Two modes, on the one orthogonal axis
/// `NO_COLOR` does *not* touch — interactivity:
///
/// - **interactive** (stderr is a TTY): redraw the whole tree in place every
///   100 ms ([`Tree::render`]); on teardown erase it and reprint it frozen as
///   permanent scrollback.
/// - **plain** (non-TTY — piped, CI, an AI agent): can't move the cursor, so
///   emit an append-only log — each node's line once, the moment it completes
///   ([`Tree::emit_completed`]) — with a resting footer at the end. This gives
///   incremental output (and survives a mid-phase ^C) where the redraw would
///   have been silent until the final dump.
///
/// The refresher only reads the atomics `body`'s workers bump (no locking on the
/// hot path); `body` gets a [`LiveHandle`] for one-off notes above the block
/// (which fall back to plain `eprintln` off a TTY).
pub fn with_live<R>(tree: &Tree, body: impl FnOnce(LiveHandle<'_>) -> R) -> R {
    let interactive = Term::stderr().is_term();
    let display = Mutex::new(Live::new());
    let done = AtomicBool::new(false);
    let mut out = None;
    thread::scope(|s| {
        let (display, done) = (&display, &done);
        s.spawn(move || {
            let mut tick = 0usize;
            while !done.load(Ordering::Relaxed) {
                if interactive {
                    display.lock().unwrap().draw(&tree.render(tick));
                } else {
                    for l in tree.emit_completed() {
                        eprintln!("{l}");
                    }
                }
                thread::sleep(Duration::from_millis(100));
                tick += 1;
            }
        });
        out = Some(body(LiveHandle { display }));
        done.store(true, Ordering::Relaxed);
    });
    // Teardown, with the workers joined (no concurrency). An empty tree (a
    // fully-cached run) prints nothing either way.
    if interactive {
        display.lock().unwrap().clear();
        for l in tree.render_frozen() {
            eprintln!("{l}");
        }
    } else {
        for l in tree.emit_completed() {
            eprintln!("{l}");
        }
        if !tree.is_empty() {
            eprintln!("{}", tree.frozen_footer());
        }
    }
    out.unwrap()
}

/// Elapsed time as a plain `m:ss` clock, gaining an `h:` field once past an
/// hour, right-padded with spaces to a fixed width so the text after the timer
/// doesn't shift as it grows.
pub fn human_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    // `h`/`m`/`s` fields, dropping empty leading ones: `0s`, `51s`, `1m29s`,
    // `1h00m00s`. Lower fields are zero-padded once a higher one is present so
    // they don't jump width. The widest form is `9h59m59s` (8 chars, up to ~10h);
    // right-pad the rest so the text after the clock doesn't shift as it grows.
    let clock = if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    };
    format!("{clock:>8}")
}

// --- the progress tree (DESIGN §6, §9) ---------------------------------------
//
// One persistent, append-only tree spanning eval → probe: every piece of
// network or nontrivial work becomes a node the moment npd learns it needs it,
// nothing is ever removed, and cached/no-op work never appears at all. Phases
// (`enumerate`, `evaluate`, `tests`, `instantiate`, `probe`, and the network
// `fetch`/`download`) are top-level nodes; under them a system level (elided for
// a single-system run) and the per-side commit `display`s. State is one of three
// nom colors — blue waiting, yellow running, green done — carried by the label;
// counts are plain, the `/ total` denominator dim, nothing bold. See the
// rendering spec in `scratch/tree_demo.py`.

// Dull ANSI, matching nom (`lib/NOM/Print.hs`) and the demo.
const BLUE: &str = "\x1b[34m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

/// Two spaces per tree level.
const INDENT: &str = "  ";
/// Fixed width of each number column (a count, and its total). Six digits covers
/// the largest count — the ~119k attr eval — with headroom, and being fixed
/// means a count gaining a digit never shifts the column.
const NUM_W: usize = 6;

/// The three node states — the blue/yellow/green of nom.
const WAIT: u8 = 0;
const RUN: u8 = 1;
const DONE: u8 = 2;

/// A node in the progress [`Tree`]. Workers bump its atomics lock-free while the
/// refresher reads them, so updates need no locking. Two columns follow the
/// label: a middle plain **count** (the streamed item/drv count), shown for any
/// `counter` node once active; and a rightmost dim column shown only *while
/// running* — either ` / total` (a known item total, e.g. `instantiate`/`probe`)
/// or an `NN%` shard-progress readout (a `percent` node, e.g. `evaluate`, whose
/// true drv total is unknowable). A count-less node (a phase, a system, a network
/// ref, `enumerate`) has neither — just a state color.
pub struct Node {
    label: String,
    depth: usize,
    counter: bool,
    percent: bool,
    /// Orders this node among its phase's children for [`Tree::insert_sorted`]
    /// (the system index, so a `tests` subtree slots into fixed system order even
    /// when it becomes ready out of order). Ignored by nodes that are only ever
    /// pushed in creation order.
    sort_key: i64,
    state: AtomicU8,
    /// Items/drvs streamed so far — the middle plain count.
    count: AtomicI64,
    /// Item total for the ` / total` column, or `-1` when unknown / not a total
    /// node. (A `percent` node leaves this `-1`; its `%` comes from the shards.)
    total: AtomicI64,
    /// Shards done / currently-running / total, for a `percent` node's `NN%`
    /// readout. Counting a running shard as half-done makes the percentage climb
    /// smoothly rather than only stepping when a whole shard lands.
    shards_done: AtomicI64,
    shards_running: AtomicI64,
    shards_total: AtomicI64,
    /// Whether this node's line has already been printed by the non-interactive
    /// append-only log ([`Tree::emit_completed`]) — so each node prints exactly
    /// once. Unused (and untouched) in the interactive redraw path.
    emitted: AtomicBool,
}

impl Node {
    fn new(
        label: String,
        depth: usize,
        counter: bool,
        percent: bool,
        total: i64,
        sort_key: i64,
    ) -> Self {
        Self {
            label,
            depth,
            counter,
            percent,
            sort_key,
            state: AtomicU8::new(WAIT),
            count: AtomicI64::new(0),
            total: AtomicI64::new(total),
            shards_done: AtomicI64::new(0),
            shards_running: AtomicI64::new(0),
            shards_total: AtomicI64::new(0),
            emitted: AtomicBool::new(false),
        }
    }

    /// Move `WAIT` → `RUN`; never regress a node that has already finished (so
    /// concurrent shards of one group race harmlessly).
    pub fn set_running(&self) {
        let _ = self
            .state
            .compare_exchange(WAIT, RUN, Ordering::Relaxed, Ordering::Relaxed);
    }

    pub fn set_done(&self) {
        self.state.store(DONE, Ordering::Relaxed);
    }

    /// Add `n` to the running count (drives the live middle number).
    pub fn add_count(&self, n: i64) {
        self.count.fetch_add(n, Ordering::Relaxed);
    }

    pub fn set_total(&self, n: i64) {
        self.total.store(n, Ordering::Relaxed);
    }

    /// A streamed item surfaced — drives the middle count for any counter node
    /// (a count-less node has no number).
    pub fn stream(&self, n: i64) {
        if self.counter {
            self.count.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// The group's shard total, set up front (it's known at group creation) so a
    /// `percent` node's `NN%` has a real denominator from the first frame —
    /// otherwise a just-started shard divides by zero-clamped-to-one and reads
    /// 100% until the first shard lands.
    pub fn set_shards_total(&self, total: usize) {
        if self.percent {
            self.shards_total.store(total as i64, Ordering::Relaxed);
        }
    }

    /// A shard of this group started running (feeds a `percent` node's `NN%`,
    /// which counts a running shard as half-done for a smoother climb).
    pub fn shard_started(&self) {
        if self.percent {
            self.shards_running.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A shard stopped running (completed or aborted) — the mate to
    /// [`shard_started`], so the running count reflects only in-flight shards.
    pub fn shard_finished(&self) {
        if self.percent {
            self.shards_running.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// `done` shards of this group have now completed. Feeds a `percent` node's
    /// `NN%` column (its total was set up front by [`set_shards_total`]); every
    /// other kind ignores it.
    pub fn shard_progress(&self, done: usize) {
        if self.percent {
            self.shards_done.store(done as i64, Ordering::Relaxed);
        }
    }

    /// The group's last shard landed with `rows` assembled items: pin the middle
    /// count to the exact total (the streamed tally can drift), then mark done.
    pub fn group_done(&self, rows: i64) {
        if self.counter {
            self.count.store(rows, Ordering::Relaxed);
        }
        self.set_done();
    }
}

/// The one live progress tree, shared (`&Tree`) by every pre-build phase. Nodes
/// are appended under a mutex; their per-node state/counts are lock-free atomics
/// the refresher reads. The number columns start at a width fixed up front (see
/// [`plan_label_width`]) so nothing shifts horizontally as phases appear.
pub struct Tree {
    nodes: Mutex<Vec<Arc<Node>>>,
    start: Instant,
    min_label_w: usize,
    multi: bool,
    /// Whether to colorize stderr — a TTY with `NO_COLOR`/`CLICOLOR` allowing it,
    /// per `console` (DESIGN §11). Gates ALL color: the state-colored labels, the
    /// dim `/ total` and `%` columns, the spinner, and the resting `.`. Note this
    /// is orthogonal to *interactivity* (redraw-in-place), which is a plain TTY
    /// check in [`with_live`] — `NO_COLOR` on a TTY still redraws, just monochrome.
    color: bool,
}

impl Tree {
    /// `color` gates all SGR; production passes [`colors_enabled`], tests force it.
    pub fn new(min_label_w: usize, multi: bool, color: bool) -> Self {
        Self {
            nodes: Mutex::new(Vec::new()),
            start: Instant::now(),
            min_label_w,
            multi,
            color,
        }
    }

    /// Whether the run spans more than one system (so phases nest a system level).
    pub fn multi(&self) -> bool {
        self.multi
    }

    /// Append a count-less node (a phase, a system, a network ref, `enumerate`).
    pub fn node(&self, label: impl Into<String>, depth: usize) -> Arc<Node> {
        let n = Arc::new(Node::new(label.into(), depth, false, false, -1, 0));
        self.nodes.lock().unwrap().push(n.clone());
        n
    }

    /// Append a counting leaf; `total` is `-1` when the denominator is unknown.
    pub fn counter(&self, label: impl Into<String>, depth: usize, total: i64) -> Arc<Node> {
        let n = Arc::new(Node::new(label.into(), depth, true, false, total, 0));
        self.nodes.lock().unwrap().push(n.clone());
        n
    }

    /// Append a leaf that shows a dim `NN%` shard-progress readout — for a phase
    /// whose true item total is unknowable ahead of time (`evaluate`).
    pub fn percent(&self, label: impl Into<String>, depth: usize) -> Arc<Node> {
        let n = Arc::new(Node::new(label.into(), depth, true, true, 0, 0));
        self.nodes.lock().unwrap().push(n.clone());
        n
    }

    /// Build a count-less node WITHOUT appending it — a subtree spine (a `tests`
    /// system level) to hand to [`insert_sorted`]. `sort_key` orders it among a
    /// phase's children.
    pub fn detached_node(
        &self,
        label: impl Into<String>,
        depth: usize,
        sort_key: i64,
    ) -> Arc<Node> {
        Arc::new(Node::new(label.into(), depth, false, false, -1, sort_key))
    }

    /// Build a counting leaf WITHOUT appending it (for [`insert_sorted`]).
    pub fn detached_counter(
        &self,
        label: impl Into<String>,
        depth: usize,
        total: i64,
        sort_key: i64,
    ) -> Arc<Node> {
        Arc::new(Node::new(label.into(), depth, true, false, total, sort_key))
    }

    /// Splice a subtree in among `phase`'s children, keeping them ordered by
    /// `sort_key` — so a subtree that becomes ready out of order (a platform whose
    /// eval finished late but sorts early) still lands at its fixed position. The
    /// subtree is a contiguous block (its root, then deeper descendants); its
    /// root's `sort_key` decides placement. Inserted before the first existing
    /// child with a larger `sort_key`, else after `phase`'s last descendant.
    pub fn insert_sorted(&self, phase: &Arc<Node>, subtree: Vec<Arc<Node>>) {
        let Some(root) = subtree.first() else { return };
        let key = root.sort_key;
        let pd = phase.depth;
        let mut nodes = self.nodes.lock().unwrap();
        let p = nodes
            .iter()
            .position(|n| Arc::ptr_eq(n, phase))
            .expect("phase node must already be in the tree");
        // Walk `phase`'s descendants; note the first direct child (depth pd+1)
        // whose key exceeds ours. `i` ends at the first node past phase's subtree.
        let mut i = p + 1;
        let mut at = None;
        while i < nodes.len() && nodes[i].depth > pd {
            if nodes[i].depth == pd + 1 && nodes[i].sort_key > key {
                at = Some(i);
                break;
            }
            i += 1;
        }
        let at = at.unwrap_or(i);
        for (k, n) in subtree.into_iter().enumerate() {
            nodes.insert(at + k, n);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.lock().unwrap().is_empty()
    }

    /// The live frame for tick `t`: node lines plus a spinner + clock footer
    /// (colorized only when [`Tree::color`]).
    pub fn render(&self, t: usize) -> Vec<String> {
        self.lines(Some(t), self.color)
    }

    /// The frozen reprint (permanent scrollback): the same node lines with a
    /// resting cyan `.` in place of the spinner. Colorized only on a terminal.
    pub fn render_frozen(&self) -> Vec<String> {
        self.lines(None, self.color)
    }

    fn lines(&self, tick: Option<usize>, color: bool) -> Vec<String> {
        let nodes = self.nodes.lock().unwrap();
        if nodes.is_empty() {
            // An empty tree draws nothing at all (a fully-cached run stays quiet).
            return Vec::new();
        }
        // Snapshot the raw per-node fields, then roll parent states up from their
        // descendant leaves.
        let snap: Vec<Row> = nodes.iter().map(node_row).collect();
        let eff: Vec<u8> = (0..snap.len()).map(|i| eff_state(&snap, i)).collect();

        // The number columns start past the widest label of ANY node, so a
        // vertical line between the tree and the numbers clips neither.
        let mut left_w = self.min_label_w;
        for r in &snap {
            left_w = left_w.max(INDENT.len() * r.depth + r.label.chars().count());
        }

        let mut out: Vec<String> = snap
            .iter()
            .enumerate()
            .map(|(i, r)| render_row(r, eff[i], left_w, color))
            .collect();

        let clock = human_elapsed(self.start.elapsed());
        let footer = match tick {
            Some(t) => {
                let g = SPINNER[t % SPINNER.len()];
                if color {
                    format!("{CYAN}{g}{RESET} {clock}")
                } else {
                    format!("{g} {clock}")
                }
            }
            None => self.frozen_footer(),
        };
        out.push(footer);
        out
    }

    /// The resting footer — a `.` (cyan when colorized) and the final elapsed —
    /// shown once a phase is done (the frozen interactive reprint, and the tail
    /// of the non-interactive append log).
    fn frozen_footer(&self) -> String {
        let clock = human_elapsed(self.start.elapsed());
        if self.color {
            format!("{CYAN}.{RESET} {clock}")
        } else {
            format!(". {clock}")
        }
    }

    /// The non-interactive (non-TTY) counterpart to [`render`]: instead of
    /// redrawing in place, emit each node's line exactly once, the moment it's
    /// done — a leaf when it turns green, a parent (phase / system) lazily just
    /// before its first emitted descendant (ancestors top-down). Returns the
    /// newly-emitted lines (empty when nothing new completed); the refresher
    /// prints them and calls again next tick. The result reads like the final
    /// interactive frame, minus color and animation, in completion order.
    pub fn emit_completed(&self) -> Vec<String> {
        let nodes = self.nodes.lock().unwrap();
        if nodes.is_empty() {
            return Vec::new();
        }
        let snap: Vec<Row> = nodes.iter().map(node_row).collect();
        let mut left_w = self.min_label_w;
        for r in &snap {
            left_w = left_w.max(INDENT.len() * r.depth + r.label.chars().count());
        }
        let mut out = Vec::new();
        for i in 0..snap.len() {
            // A leaf: no deeper node follows. Emit only once it's actually done.
            let is_leaf = i + 1 >= snap.len() || snap[i + 1].depth <= snap[i].depth;
            if !is_leaf || snap[i].state != DONE {
                continue;
            }
            // Its ancestors: the nearest node at each shallower depth, collected
            // deepest-first then reversed so headers print top-down before the leaf.
            let mut ancestors = Vec::new();
            let mut want = snap[i].depth as i64 - 1;
            let mut j = i as i64 - 1;
            while want >= 0 && j >= 0 {
                if snap[j as usize].depth as i64 == want {
                    ancestors.push(j as usize);
                    want -= 1;
                }
                j -= 1;
            }
            for &a in ancestors.iter().rev().chain(std::iter::once(&i)) {
                if !nodes[a].emitted.swap(true, Ordering::Relaxed) {
                    out.push(render_row(
                        &snap[a],
                        eff_state(&snap, a),
                        left_w,
                        self.color,
                    ));
                }
            }
        }
        out
    }
}

/// Snapshot one node's fields for rendering — atomics read once so a frame is
/// self-consistent and [`eff_state`] can roll parents up over a plain slice.
fn node_row(n: &Arc<Node>) -> Row<'_> {
    Row {
        depth: n.depth,
        label: n.label.as_str(),
        counter: n.counter,
        state: n.state.load(Ordering::Relaxed),
        count: n.count.load(Ordering::Relaxed),
        total: n.total.load(Ordering::Relaxed),
        percent: n.percent,
        sdone: n.shards_done.load(Ordering::Relaxed),
        srunning: n.shards_running.load(Ordering::Relaxed),
        stotal: n.shards_total.load(Ordering::Relaxed),
    }
}

/// Render one node's line — the label (state-colored when `color`), then for a
/// counter its plain middle count and dim right column (` / total` or `NN%`).
/// Shared by the interactive frame and the append-only log so they match.
fn render_row(r: &Row, eff: u8, left_w: usize, color: bool) -> String {
    let col = state_color(eff);
    let indent = INDENT.repeat(r.depth);
    // A count-less node (a phase, a system, `enumerate`) is just a state color.
    if !r.counter {
        return if color {
            format!("{col}{indent}{}{RESET}", r.label)
        } else {
            format!("{indent}{}", r.label)
        };
    }
    // A count populates immediately for any counter node — even while blue
    // (waiting) it reads `0`, not blank until it turns yellow.
    let left = format!("{indent}{}", r.label);
    let pad = " ".repeat(left_w.saturating_sub(left.chars().count()));
    let count_s = format!("{:>NUM_W$}", r.count);
    // The rightmost column stays for the node's whole life (waiting → running →
    // done, never dropped): a `percent` node's dim `NN%` (right-aligned in the
    // number column, `%`, no slash), else a dim ` / total` when the item total is
    // known. A running shard counts as half-done — the mean of finished and
    // finished+running shards — so the percentage climbs smoothly instead of only
    // stepping when a whole shard lands.
    let right = if r.percent {
        let denom = (2 * r.stotal).max(1);
        let pct = ((2 * r.sdone + r.srunning) * 100 / denom).clamp(0, 100);
        let p = format!("{pct:>NUM_W$}");
        if color {
            format!("{DIM}   {p}%{RESET}")
        } else {
            format!("   {p}%")
        }
    } else if r.total >= 0 {
        let t = format!("{:>NUM_W$}", r.total);
        if color {
            format!("{DIM} / {t}{RESET}")
        } else {
            format!(" / {t}")
        }
    } else {
        String::new()
    };
    // Only the label carries the state color; the count is plain (like the clock),
    // the ` / total` / percent columns dim.
    if color {
        format!("{col}{left}{pad}{RESET}  {count_s}{right}")
    } else {
        format!("{left}{pad}  {count_s}{right}")
    }
}

/// A rendered snapshot of one [`Node`] — its atomics read once, so the frame is
/// consistent and [`eff_state`] can roll parents up over a plain slice.
struct Row<'a> {
    depth: usize,
    label: &'a str,
    counter: bool,
    state: u8,
    count: i64,
    total: i64,
    percent: bool,
    sdone: i64,
    srunning: i64,
    stotal: i64,
}

/// A node's effective (rolled-up) state: any descendant leaf running → running;
/// all done → done; some done but not all → running; else waiting. A node with
/// no descendants uses its own state.
fn eff_state(snap: &[Row], i: usize) -> u8 {
    let d = snap[i].depth;
    let (mut any_run, mut any_done, mut any_wait, mut any_leaf) = (false, false, false, false);
    let mut j = i + 1;
    while j < snap.len() && snap[j].depth > d {
        let is_leaf = j + 1 >= snap.len() || snap[j + 1].depth <= snap[j].depth;
        if is_leaf {
            any_leaf = true;
            match snap[j].state {
                RUN => any_run = true,
                DONE => any_done = true,
                _ => any_wait = true,
            }
        }
        j += 1;
    }
    if !any_leaf {
        return snap[i].state;
    }
    if any_run {
        RUN
    } else if !any_wait {
        DONE
    } else if any_done {
        RUN
    } else {
        WAIT
    }
}

fn state_color(state: u8) -> &'static str {
    match state {
        RUN => YELLOW,
        DONE => GREEN,
        _ => BLUE,
    }
}

/// The width the tree's number columns start at, computed once up front from
/// every label the run can produce (DESIGN §6): the fixed phase names, the
/// systems, the two `display`s at their nesting depth, and any PR refs or
/// `--patch` compare expr. Passed to [`Tree::new`] so the columns never shift as
/// phases appear (all these labels are known at resolution).
pub fn plan_label_width(systems: &[String], pr: Option<u64>, compare: Option<&str>) -> usize {
    let ind = INDENT.len();
    let mut w = [
        "fetch",
        "download",
        "enumerate",
        "evaluate",
        "tests",
        "instantiate",
        "probe",
    ]
    .iter()
    .map(|p| p.len())
    .max()
    .unwrap();
    // The base/head `display`s are absorbed dynamically: a phase adds all its
    // commit nodes atomically (as WAIT) before any of them shows a number, so the
    // column already clears them by the first frame with a count — nothing shifts,
    // and they need not be known here (they aren't until resolution finishes).
    if systems.len() > 1 {
        for s in systems {
            w = w.max(ind + s.chars().count());
        }
    }
    if let Some(n) = pr {
        w = w.max(ind + format!("refs/pull/{n}/merge").len());
    }
    if let Some(c) = compare {
        w = w.max(ind + c.chars().count());
    }
    w
}

/// Whether npd should emit color (SGR) on stderr — a TTY with `NO_COLOR` /
/// `CLICOLOR` permitting it, per `console` (which also honors `CLICOLOR_FORCE`).
/// The single source of truth for color; orthogonal to interactivity (a plain
/// TTY check), so `NO_COLOR` on a TTY still redraws in place, just monochrome.
pub fn colors_enabled() -> bool {
    console::colors_enabled_stderr()
}

/// npd's one visual separator, on stderr, between each of its phases (the live
/// tree, nom's build, the report): a blank line, a dim rule, a blank line — the
/// spacing does the separating, the rule just marks it. Dimmed only on a
/// terminal, so a redirected stderr gets plain hyphens.
pub fn separator() {
    let rule = "---";
    eprintln!();
    if console::colors_enabled_stderr() {
        eprintln!("{DIM}{rule}{RESET}");
    } else {
        eprintln!("{rule}");
    }
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_is_a_fixed_width_clock() {
        // h/m/s fields dropping empty leading ones, starting at `0s`, right-padded
        // to a constant width so the text after the timer doesn't shift as it grows.
        assert_eq!(human_elapsed(Duration::from_secs(0)), "      0s");
        assert_eq!(human_elapsed(Duration::from_secs(51)), "     51s");
        assert_eq!(human_elapsed(Duration::from_secs(89)), "   1m29s");
        assert_eq!(human_elapsed(Duration::from_secs(90)), "   1m30s");
        assert_eq!(human_elapsed(Duration::from_secs(3600)), "1h00m00s");
        assert_eq!(human_elapsed(Duration::from_secs(5400)), "1h30m00s");
        // Every rendering up to ~10h is the same width; `9h59m59s` is the widest.
        for s in [0, 51, 599, 3600, 35999] {
            assert_eq!(human_elapsed(Duration::from_secs(s)).len(), 8);
        }
    }

    /// All node lines except the (time-dependent) footer.
    fn node_lines(tree: &Tree) -> Vec<String> {
        let mut lines = tree.render(0);
        lines.pop(); // drop the spinner + clock footer
        lines
    }

    #[test]
    fn renders_states_counts_and_totals() {
        // Single system: phase → commit. Colors live only on the label; the count
        // is plain, the ` / total` dim, nothing bold. A done side collapses to a
        // bare count; a running side shows `count / total`.
        let tree = Tree::new(0, false, true);
        tree.node("evaluate", 0);
        let base = tree.counter("master", 1, -1);
        let head = tree.counter("HEAD", 1, -1);
        base.set_running();
        base.add_count(114230);
        base.set_done();
        head.set_running();
        head.set_total(114231);
        head.add_count(107347);

        let lines = node_lines(&tree);
        assert_eq!(
            lines,
            vec![
                // rollup: a running child → the phase is yellow.
                "\x1b[33mevaluate\x1b[0m".to_string(),
                // done → green label, bare plain count, aligned in the 8-wide column.
                "\x1b[32m  master\x1b[0m  114230".to_string(),
                // running → yellow label, plain count, dim ` / total`.
                "\x1b[33m  HEAD  \x1b[0m  107347\x1b[90m / 114231\x1b[0m".to_string(),
            ]
        );
    }

    #[test]
    fn no_color_renders_no_sgr() {
        // With color off (NO_COLOR, or a non-terminal), the same tree emits zero
        // SGR — plain text, identical layout.
        let tree = Tree::new(0, false, false);
        tree.node("evaluate", 0);
        let head = tree.counter("HEAD", 1, 114231);
        head.set_running();
        head.add_count(107347);
        let lines = node_lines(&tree);
        assert!(
            lines.iter().all(|l| !l.contains('\x1b')),
            "no ANSI escapes: {lines:?}"
        );
        assert_eq!(
            lines,
            vec![
                "evaluate".to_string(),
                "  HEAD    107347 / 114231".to_string(),
            ]
        );
    }

    #[test]
    fn percent_node_smooths_and_keeps_pct_when_done() {
        // evaluate: a plain drv count (middle) PLUS a dim shard `NN%` (right). The
        // shard total is known up front, so just turning yellow with shards
        // running reads its true small percent — NOT 100%.
        let tree = Tree::new(0, false, true);
        tree.node("evaluate", 0);
        let head = tree.percent("HEAD", 1);
        head.set_shards_total(10);
        head.set_running();
        head.add_count(142001);
        head.shard_started();
        head.shard_started();
        // 0 done + 2 running of 10 → (0 + 2) / 2 = 1 → 10% (not 100%).
        assert_eq!(
            node_lines(&tree)[1],
            "\x1b[33m  HEAD  \x1b[0m  142001\x1b[90m       10%\x1b[0m"
        );
        // A running shard counts as half-done: 3 done + 2 running → (3 + 5)/2 = 4
        // → 40%.
        head.shard_progress(3);
        assert_eq!(
            node_lines(&tree)[1],
            "\x1b[33m  HEAD  \x1b[0m  142001\x1b[90m       40%\x1b[0m"
        );
        // Done keeps the percent (now 100%) beside the pinned count — not dropped.
        head.shard_finished();
        head.shard_finished();
        head.shard_progress(10);
        head.group_done(226117);
        assert_eq!(
            node_lines(&tree)[1],
            "\x1b[32m  HEAD  \x1b[0m  226117\x1b[90m      100%\x1b[0m"
        );
    }

    #[test]
    fn waiting_counter_populates_its_number() {
        // A counter populates its number immediately — even while blue (waiting)
        // it reads `0`, rather than blank until it turns yellow.
        let tree = Tree::new(0, false, true);
        tree.node("tests", 0);
        tree.counter("HEAD", 1, -1); // left in WAIT
        assert_eq!(
            node_lines(&tree),
            vec![
                "\x1b[34mtests\x1b[0m".to_string(),
                "\x1b[34m  HEAD\x1b[0m       0".to_string(),
            ]
        );
    }

    #[test]
    fn insert_sorted_keeps_phase_children_in_key_order() {
        // `tests` systems appear as each becomes ready, but a later-ready system
        // that sorts earlier splices ABOVE an already-present one — the section
        // stays in fixed system order regardless of completion order.
        let tree = Tree::new(0, true, true);
        let phase = tree.node("tests", 0);
        for (label, key) in [("sysB", 1), ("sysA", 0), ("sysC", 2)] {
            let sys = tree.detached_node(label, 1, key);
            let leaf = tree.detached_counter("HEAD", 2, -1, key);
            tree.insert_sorted(&phase, vec![sys, leaf]);
        }
        let lines = node_lines(&tree);
        // tests + three (system, leaf) pairs, all present.
        assert_eq!(lines.len(), 7);
        let out = lines.join("\n");
        let (a, b, c) = (
            out.find("sysA").unwrap(),
            out.find("sysB").unwrap(),
            out.find("sysC").unwrap(),
        );
        assert!(a < b && b < c, "must render in sort-key order: {out:?}");
    }

    #[test]
    fn emit_completed_prints_leaves_on_done_ancestors_first_once() {
        // The non-interactive append log: a leaf prints only once it's done, its
        // parent headers print once just before its first done child (ancestors
        // top-down), and nothing re-prints on a later call.
        let tree = Tree::new(0, true, false); // multi (system level), no color
        let phase = tree.node("evaluate", 0);
        let a1 = tree.detached_counter("base", 2, -1, 0);
        let a2 = tree.detached_counter("head", 2, -1, 0);
        tree.insert_sorted(
            &phase,
            vec![tree.detached_node("sysA", 1, 0), a1.clone(), a2.clone()],
        );

        assert!(tree.emit_completed().is_empty()); // nothing done yet

        a1.set_running();
        a1.add_count(5);
        a1.set_done();
        assert_eq!(
            tree.emit_completed(),
            vec![
                "evaluate".to_string(),
                "  sysA".to_string(),
                "    base       5".to_string(),
            ]
        );
        assert!(tree.emit_completed().is_empty()); // base emitted, head not done

        a2.set_running();
        a2.add_count(7);
        a2.set_done();
        // head only — its ancestors were already emitted.
        assert_eq!(tree.emit_completed(), vec!["    head       7".to_string()]);
    }

    #[test]
    fn rollup_all_done_is_green() {
        let tree = Tree::new(0, false, true);
        tree.node("enumerate", 0);
        for c in ["master", "HEAD"] {
            let n = tree.counter(c, 1, -1);
            n.set_running();
            n.add_count(100);
            n.set_done();
        }
        assert_eq!(node_lines(&tree)[0], "\x1b[32menumerate\x1b[0m");
    }

    #[test]
    fn empty_tree_draws_nothing() {
        let tree = Tree::new(11, false, true);
        assert!(tree.is_empty());
        assert!(tree.render(0).is_empty());
        assert!(tree.render_frozen().is_empty());
    }

    #[test]
    fn plan_width_clears_every_label() {
        // Single system: the longest phase name (`instantiate`, 11) is the floor.
        assert_eq!(plan_label_width(&["aarch64-linux".into()], None, None), 11);
        // Multi-system: a system name at depth 1 is widest (2 + 13).
        assert_eq!(
            plan_label_width(&["aarch64-linux".into(), "x86_64-linux".into()], None, None),
            15
        );
        // A PR fetch ref at depth 1 (2 + 19) beats them all.
        assert_eq!(
            plan_label_width(&["aarch64-linux".into()], Some(431), None),
            21
        );
    }
}
