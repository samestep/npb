//! Run `nix-eval-jobs` and schedule the runs: evaluate a nixpkgs revision into
//! an `attr -> drv` map — the first spine primitive (DESIGN.md §6, §9), a pure
//! fact keyed by `(commit, system)`, computed at most once and cached as one
//! flat file per eval (the file format and its diff live in [`crate::evalfile`]).
//!
//! The revision's source comes from `builtins.fetchGit`, so Nix fetches and
//! caches it in the store — npd manages no worktrees. `nix-eval-jobs` output is
//! parsed by streaming NDJSON straight off the child's stdout (never buffering
//! the whole, meta-heavy output).

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Deserialize;

use crate::evalfile::{eval_path, write_eval};
use crate::model::{AttrEval, TestJob};

/// The one nixpkgs config every eval runs under. npd owns the config
/// (DESIGN.md §6), which is what makes the eval cache key just
/// `(commit, system)` — changing this line changes the attr→drv map, so it is
/// by definition an [`crate::evalfile::EVAL_VERSION`] bump. The allow-flags are
/// on so meta-blocked packages still yield a drv + meta rather than throwing —
/// we want their drvpath and the option to build them anyway.
const EVAL_CONFIG: &str = "{ allowBroken = true; allowUnfree = true; \
                             allowUnsupportedSystem = true; allowInsecurePredicate = _: true; }";

// --- nix-eval-jobs output ---------------------------------------------------

/// The slice of `meta` we consume (from `--meta`): the availability bits
/// nixpkgs' check-meta computes. [`EVAL_CONFIG`]'s allow-flags let these packages
/// evaluate to a drv anyway; the bits say they shouldn't be *built* by default.
#[derive(Deserialize, Default)]
struct RawMeta {
    broken: Option<bool>,
    unsupported: Option<bool>,
    insecure: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawJob {
    attr: String,
    /// The attr path as an array of *unquoted* elements. Preferred over `attr`
    /// (which nix-eval-jobs quotes when an element contains a `.`) when a clean,
    /// dotted label is wanted — see the test eval.
    attr_path: Vec<String>,
    /// `None` when evaluation of the attr errored (the job line carries an
    /// `error` message instead, which we don't keep — re-evaluating reproduces it).
    drv_path: Option<String>,
    meta: Option<RawMeta>,
}

/// Fold `--meta`'s availability bits into npd's single "meta-blocked" bit: marked
/// broken *or* unsupported-on-this-system *or* insecure. A missing `meta` (an
/// errored attr carries none) reads as not-blocked. Shared by the full-set walk
/// and the targeted test eval so both classify meta the same way.
fn meta_broken(meta: &RawMeta) -> bool {
    meta.broken == Some(true) || meta.unsupported == Some(true) || meta.insecure == Some(true)
}

fn raw_to_attr_eval(raw: RawJob) -> AttrEval {
    AttrEval {
        attr: raw.attr,
        drv_path: raw.drv_path,
        broken: meta_broken(&raw.meta.unwrap_or_default()),
    }
}

// --- running the evaluator --------------------------------------------------

/// Escape a string for embedding inside a Nix `"..."` literal: backslashes,
/// double quotes, and the `${` interpolation opener. (Attr names and store
/// paths virtually never contain these, but the repo path and revision are
/// user input.)
fn nix_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace("${", "\\${")
}

/// Build the whole-package-set Nix expression `nix-eval-jobs` walks. The
/// revision's source is fetched by `builtins.fetchGit`. Interpolants are
/// escaped exactly as in [`build_tests_expr`] — the repo path in particular is
/// user input (`--nixpkgs`).
fn build_expr(repo: &Path, commit: &str, system: &str) -> String {
    format!(
        "import (builtins.fetchGit {{ url = \"{}\"; rev = \"{}\"; }}) \
         {{ system = \"{}\"; config = {EVAL_CONFIG}; }}",
        nix_escape(&repo.display().to_string()),
        nix_escape(commit),
        nix_escape(system),
    )
}

/// Run one `nix-eval-jobs` invocation with `workers` worker processes, each
/// heap-capped at `per_worker_mb` (nix-eval-jobs restarts a worker that exceeds
/// it, so total memory ≈ `workers * per_worker_mb`). Progress is streamed onto
/// the caller-supplied `pb`, letting several evals share one MultiProgress.
fn run_eval_pb(
    repo: &Path,
    commit: &str,
    system: &str,
    workers: usize,
    per_worker_mb: u64,
    pb: &ProgressBar,
) -> Result<Vec<AttrEval>> {
    let expr = build_expr(repo, commit, system);
    let short: String = commit.chars().take(12).collect();
    let label = format!("{short} ({system}, {workers}w)");
    stream_jobs(&expr, workers, per_worker_mb, pb, &label, raw_to_attr_eval)
}

/// Run one `nix-eval-jobs --expr <expr>` (with `workers` workers each capped at
/// `per_worker_mb`), streaming its NDJSON stdout through `map_job` into
/// `AttrEval`s and rendering progress onto `pb`. `label` names the run in the
/// progress bar and the integrity-gate error. Shared by the cached full-set eval
/// (`map_job` → [`AttrEval`], keyed on `attr`) and the targeted test eval
/// (`map_job` → [`TestJob`], relabelled from `attrPath`) — both stream the same
/// job shape and want the same truncation gate, so it's generic over the output.
fn stream_jobs<T>(
    expr: &str,
    workers: usize,
    per_worker_mb: u64,
    pb: &ProgressBar,
    label: &str,
    map_job: impl Fn(RawJob) -> T,
) -> Result<Vec<T>> {
    // nix-eval-jobs prints a full Nix traceback per errored attr (megabytes over a
    // whole package set), and the actionable per-attr error is already in the
    // stdout JSON — so we neither inherit its stderr (terminal spam) nor persist
    // it to disk. A thread drains stderr into a bounded ring buffer, keeping only
    // the last few lines for the fatal-error diagnostic below; draining it (vs. an
    // undrained pipe) also can't deadlock while we stream stdout.

    // `--meta` costs ~15% (each package's meta attrset is forced and emitted),
    // but it's what carries `broken`/`unsupported`/`insecure` — the bits the
    // build policy needs to skip meta-blocked packages by default.

    // nix-eval-jobs compares `--max-memory-size` (MiB) against `ru_maxrss`
    // scaled by 1024, which is correct on Linux (KiB) but off by 1024× on
    // macOS, where `ru_maxrss` is in bytes: the effective cap becomes
    // `per_worker_mb` *KiB*, every worker trips it after its first job, and
    // each subsequent job pays a full worker restart + nixpkgs re-import
    // (~100× slower end-to-end). Compensate by passing the cap ×1024 on macOS.
    // Fixed upstream (https://github.com/NixOS/nix-eval-jobs/issues/425, via
    // https://github.com/NixOS/nix-eval-jobs/pull/426); remove once the
    // nix-eval-jobs on PATH carries that fix.
    let max_memory_size = if cfg!(target_os = "macos") {
        per_worker_mb * 1024
    } else {
        per_worker_mb
    };
    let mut child = Command::new("nix-eval-jobs")
        .args([
            "--meta",
            "--workers",
            &workers.to_string(),
            "--max-memory-size",
            &max_memory_size.to_string(),
            "--expr",
            expr,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning nix-eval-jobs (on PATH? use the flake dev shell)")?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stderr_tail = thread::spawn(move || {
        const KEEP: usize = 20;
        let mut ring: VecDeque<String> = VecDeque::with_capacity(KEEP + 1);
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if ring.len() == KEEP {
                ring.pop_front();
            }
            ring.push_back(line);
        }
        ring.into_iter().collect::<Vec<_>>().join("\n")
    });

    // A full-set eval takes minutes (and is pathologically slow on macOS — see
    // DESIGN); show a live elapsed timer next to the attr counter (like `nom`'s
    // build timer) so a slow eval reads as "still working", not "hung". The
    // `{elapsed}` field is re-rendered on every steady tick, so it ticks even
    // between attrs; the counter updates as attrs stream in.
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} ⏱ {elapsed} {msg}").expect("valid template"),
    );
    pb.reset_elapsed();
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_message(format!("evaluating {label}"));
    let mut attrs = Vec::new();
    for item in serde_json::Deserializer::from_reader(BufReader::new(stdout)).into_iter::<RawJob>()
    {
        match item.context("parsing nix-eval-jobs output") {
            Ok(raw) => attrs.push(map_job(raw)),
            Err(e) => {
                // A `Child` is not killed on drop: bail out without reaping and
                // a multi-GB nix-eval-jobs (plus its workers) keeps evaluating
                // into the void. Kill it (which also ends the stderr thread via
                // EOF) before surfacing the parse error.
                let _ = child.kill();
                let _ = child.wait();
                pb.abandon_with_message(format!("eval of {label} failed"));
                return Err(e);
            }
        }
        pb.set_message(format!("evaluating {label} — {} attrs", attrs.len()));
    }

    let status = child.wait().context("waiting for nix-eval-jobs")?;
    let stderr_tail = stderr_tail.join().unwrap_or_default();
    // Integrity gate. Per-attr eval errors are emitted *in band* as JSON
    // (`{"attr":…,"error":…}`) and do NOT affect the exit code — a complete
    // full-set eval exits 0 even with thousands of `throw`n attrs. A non-zero
    // exit means a *fatal* abort: a worker died mid-eval (most often an OOM
    // SIGKILL when the workers' memory caps oversubscribe RAM), in which case
    // the streamed output is silently TRUNCATED — we got some attrs but not
    // all. Caching that would poison every future diff/report with phantom
    // "removed" packages, so we refuse it outright rather than trust a partial.
    // The [`EvalAborted`] marker is what lets the scheduler retry narrower.
    if !status.success() {
        pb.abandon_with_message(format!("eval of {label} failed (truncated)"));
        return Err(anyhow::Error::new(EvalAborted).context(format!(
            "nix-eval-jobs did not finish evaluating {label}: it exited \
             {status} after streaming {} attr(s), so the result is truncated and \
             will NOT be cached. A worker most likely died — commonly out-of-memory. \
             Last stderr:\n{}",
            attrs.len(),
            stderr_tail,
        )));
    }
    // Declare success only after the integrity gate: a truncated eval must not
    // flash an "evaluated …" line before the error.
    pb.finish_with_message(format!("evaluated {label} — {} attrs", attrs.len()));
    Ok(attrs)
}

// --- targeted test eval (passthru.tests of the changed set) ------------------
//
// The `--tests` feature (ported from nixpkgs-review#397): for the packages in a
// change's *changed set*, also build their `passthru.tests`. This is a small,
// targeted eval over the (few) changed attrs, distinct from the full-set eval —
// and it *is* cached, per package, in SQLite (see `store::Store` and `main`): a
// test's drv is a pure function of `(commit, system, package-attr)`, so
// `eval_tests` runs only over the packages a run hasn't cached yet (the misses),
// and a fully-cached re-run touches no `nix-eval-jobs` at all. It's a SQLite
// fact, not a flat eval file, because the access pattern is keyed/incremental
// (look up a package, append new ones) rather than the full-set eval's
// bulk/write-once/read-whole-and-diff (DESIGN §4).
//
// The full-set `nix-eval-jobs` walk never reaches these drvs: a package's
// `passthru.tests` is a plain attrset without `recurseForDerivations`, so it's
// not descended into. We surface them with a targeted expression: a job tree
// `<pkg>.tests.<name>` where each package's `.tests` is a *thunk* forced by
// `nix-eval-jobs` in its per-attr worker — so a package that fails to evaluate
// (even an uncatchable parse error `tryEval` can't trap) is isolated to its own
// attr, exactly as in the full-set walk, rather than aborting the whole eval.

/// Nix expression exposing the `passthru.tests` of `attrs` at one revision as a
/// `nix-eval-jobs` job tree. Each requested `<pkg>` becomes a recursable node
/// `{ recurseForDerivations = true; tests = <thunk>; }`; the `tests` thunk (which
/// is what forces the package) is evaluated per-attr in a worker, so a throwing
/// package errors only its own subtree. `tests` resolves to the package's
/// `passthru.tests` — a derivation (emitted as `<pkg>.tests`) or an attrset made
/// recursable (emitted as `<pkg>.tests.<name>`); anything else yields no jobs.
///
/// **Computed meta-blocked bit.** A `passthru.tests` entry is usually a
/// `nixosTest`/`vm-test-run` derivation, which does *not* pass through nixpkgs'
/// `check-meta` `commonMeta`, so — unlike a normal package — its raw `meta`
/// carries no computed `unsupported`/`insecure` field (only whatever the test
/// framework set, e.g. `platforms`). So `--meta` alone can't tell us a test is
/// meta-blocked. `mark` computes it here — platform support via
/// `lib.meta.availableOn`, insecurity via `knownVulnerabilities` — and injects
/// `unsupported`/`insecure` into each test derivation's `meta`, so the same fold
/// the full-set walk uses (`meta_broken`) also classifies tests, matching
/// nixpkgs-review's "marked broken and skipped" (which gets the same answer by
/// `tryEval`-ing the outPath under a strict config). `mark` stops at
/// derivations, so it never forces a derivation's internals, and each recursed
/// leaf is wrapped in `tryEval` so one throwing test errors only itself — the
/// per-leaf isolation nix-eval-jobs would otherwise give the untransformed tree.
fn build_tests_expr(repo: &Path, commit: &str, system: &str, attrs: &[String]) -> String {
    let list: String = attrs
        .iter()
        .map(|a| format!("\"{}\" ", nix_escape(a)))
        .collect();
    const TEMPLATE: &str = r#"
let
  pkgs = import (builtins.fetchGit { url = "@REPO@"; rev = "@COMMIT@"; }) { system = "@SYSTEM@"; config = @CFG@; };
  lib = pkgs.lib;
  host = pkgs.stdenv.hostPlatform;
  attrs = [ @ATTRS@];
  # Inject the *computed* meta-blocked bits (see build_tests_expr doc) into every
  # test derivation, recursing through `tests` sub-attrsets. Stops at derivations
  # (never forces their internals); each recursed leaf goes through `tryEval`, so
  # a test that throws when forced is passed through untouched to error on its own.
  mark = t:
    if lib.isDerivation t then
      t // {
        meta = (t.meta or { }) // {
          unsupported = !(lib.meta.availableOn host t);
          insecure = (t.meta.knownVulnerabilities or [ ]) != [ ];
        };
      }
    else if lib.isAttrs t then
      lib.mapAttrs (_: v: let r = builtins.tryEval (mark v); in if r.success then r.value else v) t
      // { recurseForDerivations = true; }
    else t;
  node = name: {
    recurseForDerivations = true;
    # Forced per-attr in a nix-eval-jobs worker: a package that fails to evaluate
    # errors only its own `<pkg>.tests`, never the whole run.
    tests =
      let
        pkg = lib.attrByPath (lib.splitString "." name) null pkgs;
        t = if pkg == null then null else (pkg.tests or null);
      in
        if lib.isDerivation t || lib.isAttrs t then mark t
        else { recurseForDerivations = true; };
  };
in
lib.listToAttrs (map (name: lib.nameValuePair name (node name)) attrs)
// { recurseForDerivations = true; }
"#;
    TEMPLATE
        .replace("@REPO@", &nix_escape(&repo.display().to_string()))
        .replace("@COMMIT@", &nix_escape(commit))
        .replace("@SYSTEM@", &nix_escape(system))
        .replace("@CFG@", EVAL_CONFIG)
        .replace("@ATTRS@", &list)
}

/// Evaluate the `passthru.tests` of `attrs` at `commit`/`system` into [`TestJob`]s
/// (one per resolved `<pkg>.tests.<name>`). This is the *miss* path of the cache:
/// callers pass only the packages not already cached (see `main`). Returns an
/// empty vec for an empty `attrs`.
pub fn eval_tests(
    repo: &Path,
    commit: &str,
    system: &str,
    attrs: &[String],
) -> Result<Vec<TestJob>> {
    if attrs.is_empty() {
        return Ok(Vec::new());
    }
    let expr = build_tests_expr(repo, commit, system, attrs);
    // A targeted eval over a small changed set: a couple of workers is plenty,
    // and each still re-evaluates the nixpkgs spine, so more would only waste RAM.
    let workers = attrs.len().clamp(1, 4);
    let short: String = commit.chars().take(12).collect();
    let label = format!("tests {short} ({system})");
    let pb = ProgressBar::new_spinner();
    // Label and split from `attrPath` (unquoted elements) rather than `attr`
    // (which nix-eval-jobs quotes for the dotted package component, e.g.
    // `"python3Packages.requests".tests.foo`): element 0 is the package we asked
    // for (the job tree is keyed by it), and the whole path joined is the clean
    // `<pkg>.tests.<name>` label.
    let map = |raw: RawJob| {
        let pkg_attr = raw.attr_path.first().cloned().unwrap_or_default();
        let test_attr = raw.attr_path.join(".");
        let broken = meta_broken(&raw.meta.unwrap_or_default());
        TestJob {
            pkg_attr,
            test_attr,
            drv_path: raw.drv_path,
            broken,
        }
    };
    let r = stream_jobs(&expr, workers, DEFAULT_WORKER_MEM_MB, &pb, &label, map);
    pb.finish_and_clear();
    r
}

// --- concurrency: recover, don't predict (DESIGN §6) -------------------------

/// Default per-worker heap cap (matches nix-eval-jobs' own 4 GiB default).
const DEFAULT_WORKER_MEM_MB: u64 = 4096;
/// A single eval sees diminishing returns past this many workers (each worker
/// redundantly re-evaluates the package-set spine), so cap the auto-derived
/// width here even when the cores split could afford more.
const MAX_WORKERS_PER_EVAL: usize = 8;

/// Optional overrides for the parallel-eval sizing. `None` means "auto": the
/// width comes from splitting the machine's cores *and total RAM* across the
/// evals (see [`initial_workers`]); the dynamic part of RAM is not planned
/// for — an eval that dies is retried narrower instead ([`eval_pairs`]).
#[derive(Debug, Clone, Copy, Default, clap::Args)]
pub struct EvalOpts {
    /// `nix-eval-jobs` workers per evaluation (default: the machine's cores
    /// and total RAM split across the evals, clamped 1–8).
    #[arg(long = "eval-workers")]
    pub workers: Option<u64>,
    /// Per-`nix-eval-jobs`-worker heap cap, MiB (default: 4096).
    #[arg(long)]
    pub worker_mem_mb: Option<u64>,
}

/// A fatal `nix-eval-jobs` abort (non-zero exit): the streamed output was
/// truncated and discarded. A marker type so the scheduler can recognize it
/// through the anyhow chain and retry that eval with fewer workers.
#[derive(Debug)]
struct EvalAborted;

impl std::fmt::Display for EvalAborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "nix-eval-jobs aborted before finishing (output truncated)"
        )
    }
}

impl std::error::Error for EvalAborted {}

/// Total physical RAM in MiB. Unlike *available* RAM (which the old planner
/// used, and which lies — it moves while a minutes-long eval runs), this is an
/// invariant of the machine, so the width heuristic may plan from it. Linux:
/// `/proc/meminfo MemTotal`; macOS: `sysctl -n hw.memsize`; else 8 GiB.
fn total_mem_mb() -> u64 {
    if let Ok(s) = fs::read_to_string("/proc/meminfo")
        && let Some(kb) = s
            .lines()
            .find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|n| n.parse::<u64>().ok())
    {
        return kb / 1024;
    }
    Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|bytes| bytes / 1024 / 1024)
        .unwrap_or(8192)
}

/// The starting width for each of `n_jobs` concurrent evals: the user's
/// `--eval-workers` if given, else the machine's cores split across the evals
/// (rounding up — a little CPU oversubscription beats idle cores), **bounded
/// so the batch's worst case fits in total RAM** (every worker at its heap
/// cap), clamped to [`MAX_WORKERS_PER_EVAL`].
///
/// The RAM bound plans only from invariants (total RAM, the cap) — the
/// recovery ladder in [`eval_pairs`] still covers the dynamic part. Without
/// it, a wide first attempt on a small-RAM machine starts a *global* OOM
/// hunt, and the kernel's victim is not always a worker (it can be npd, or an
/// unrelated process), in which case the ladder's detector never fires.
fn initial_workers(
    cores: usize,
    mem_mb: u64,
    per_worker_mb: u64,
    n_jobs: usize,
    user: Option<u64>,
) -> usize {
    if let Some(w) = user {
        return (w as usize).max(1);
    }
    let n = n_jobs.max(1);
    let by_cpu = cores.div_ceil(n);
    let slots = (mem_mb / per_worker_mb.max(1)).max(1) as usize;
    let by_ram = (slots / n).max(1);
    by_cpu.min(by_ram).clamp(1, MAX_WORKERS_PER_EVAL)
}

/// Ensure every `(commit, system)` pair has a cached eval file. The misses all
/// run concurrently, each starting at the cores-split width
/// ([`initial_workers`]); RAM is deliberately *not* planned for. Instead,
/// **recover**: an eval that aborts fatally (in practice a worker OOM-killed
/// when the widths oversubscribe RAM) is retried at half its width, halving
/// down to one worker — the integrity gate in [`stream_jobs`] is the detector,
/// and per-eval persistence means a retry re-pays only the eval that died,
/// never its finished siblings. See DESIGN §6 for why this replaced planning
/// from a measured RAM budget.
pub fn eval_pairs(repo: &Path, pairs: &[(String, String)], opts: EvalOpts) -> Result<()> {
    let mut todo: Vec<usize> = Vec::new();
    // Dedupe: `npd X X` (or repeated --system) would otherwise run the same
    // eval twice concurrently — harmless (the write is atomic) but 2× the work.
    let mut seen = std::collections::HashSet::new();
    for (i, (commit, system)) in pairs.iter().enumerate() {
        if !eval_path(commit, system)?.exists() && seen.insert((commit, system)) {
            todo.push(i);
        }
    }
    if todo.is_empty() {
        return Ok(());
    }

    let per_worker_mb = opts.worker_mem_mb.unwrap_or(DEFAULT_WORKER_MEM_MB);
    let cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let width = initial_workers(
        cores,
        total_mem_mb(),
        per_worker_mb,
        todo.len(),
        opts.workers,
    );
    let mp = MultiProgress::new();
    thread::scope(|s| -> Result<()> {
        let mut handles = Vec::new();
        for &i in &todo {
            let (commit, system) = (&pairs[i].0, &pairs[i].1);
            let mp = &mp;
            handles.push(s.spawn(move || -> Result<()> {
                let mut workers = width;
                loop {
                    // A fresh bar per attempt: the failed attempt's abandoned
                    // bar stays visible above as the record of what happened.
                    let pb = mp.add(ProgressBar::new_spinner());
                    match run_eval_pb(repo, commit, system, workers, per_worker_mb, &pb) {
                        // Persist immediately (the write is atomic): a sibling
                        // eval failing later must not discard finished work.
                        Ok(attrs) => return write_eval(&eval_path(commit, system)?, &attrs),
                        Err(e) if workers > 1 && e.downcast_ref::<EvalAborted>().is_some() => {
                            workers /= 2;
                            // Through the MultiProgress so the note doesn't
                            // tear the sibling evals' live bars.
                            let _ = mp.println(format!(
                                "  eval of {} ({system}) aborted — likely out of memory; \
                                 retrying with {workers} worker(s)",
                                &commit[..12.min(commit.len())],
                            ));
                        }
                        Err(e) => return Err(e),
                    }
                }
            }));
        }
        // Join everything before propagating the first error, so no result is
        // dropped mid-write and every progress bar reaches a final state.
        let mut result = Ok(());
        for h in handles {
            let r = h.join().expect("eval thread panicked");
            if result.is_ok() {
                result = r;
            }
        }
        result
    })
}

/// Ensure both commits are evaluated across all systems (they run concurrently).
pub fn eval_two(
    repo: &Path,
    base: &str,
    head: &str,
    systems: &[String],
    opts: EvalOpts,
) -> Result<()> {
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(systems.len() * 2);
    for s in systems {
        pairs.push((base.to_string(), s.clone()));
    }
    for s in systems {
        pairs.push((head.to_string(), s.clone()));
    }
    eval_pairs(repo, &pairs, opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stream NDJSON values off `reader`, mapping each to an `AttrEval` — the
    /// same job-parse the production streamer does inline, exercised here over a
    /// fixed buffer rather than a live `nix-eval-jobs` child.
    fn parse_jobs<R: std::io::Read>(reader: R) -> Result<Vec<AttrEval>> {
        let mut out = Vec::new();
        for item in serde_json::Deserializer::from_reader(reader).into_iter::<RawJob>() {
            let raw = item.context("parsing nix-eval-jobs output")?;
            out.push(raw_to_attr_eval(raw));
        }
        Ok(out)
    }

    #[test]
    fn parses_success_broken_and_error_lines() {
        // Any of meta.broken/unsupported/insecure folds into the one `broken`
        // bit; an errored attr has no drvPath (and no meta). Unknown fields
        // (system, fatal, …) are simply ignored.
        let stdout = concat!(
            r#"{"attr":"hello","attrPath":["hello"],"drvPath":"/nix/store/a-hello.drv","meta":{"broken":false,"unsupported":false},"system":"aarch64-linux"}"#,
            "\n",
            r#"{"attr":"br","attrPath":["br"],"drvPath":"/nix/store/b-br.drv","meta":{"broken":true}}"#,
            "\n",
            r#"{"attr":"unsup","attrPath":["unsup"],"drvPath":"/nix/store/c-unsup.drv","meta":{"unsupported":true}}"#,
            "\n",
            r#"{"attr":"bad","attrPath":["bad"],"error":"boom","fatal":false}"#,
            "\n",
        );
        let attrs = parse_jobs(stdout.as_bytes()).unwrap();
        assert_eq!(attrs.len(), 4);

        assert_eq!(attrs[0].attr, "hello");
        assert_eq!(attrs[0].drv_path.as_deref(), Some("/nix/store/a-hello.drv"));
        assert!(!attrs[0].broken);

        assert!(attrs[1].broken);
        assert!(attrs[1].drv_path.is_some());
        assert!(attrs[2].broken);

        assert_eq!(attrs[3].attr, "bad");
        assert_eq!(attrs[3].drv_path, None);
        assert!(!attrs[3].broken);
    }

    #[test]
    fn full_expr_fetches_and_imports() {
        let repo = Path::new("/repo");
        let full = build_expr(repo, "abc123", "aarch64-linux");
        assert!(full.contains(r#"builtins.fetchGit { url = "/repo"; rev = "abc123"; }"#));
        assert!(full.contains("allowBroken = true"));
    }

    #[test]
    fn initial_workers_splits_cores_and_ram() {
        const G: u64 = 1024;
        // Plenty of RAM: the cores split governs (rounded up, clamped 1..=8).
        assert_eq!(initial_workers(18, 256 * G, 4 * G, 2, None), 8); // ceil(18/2)=9→8
        assert_eq!(initial_workers(18, 256 * G, 4 * G, 6, None), 3);
        assert_eq!(initial_workers(4, 256 * G, 4 * G, 6, None), 1);
        // RAM-bound: total/(cap×jobs) governs — 31 GiB, 4 GiB caps.
        assert_eq!(initial_workers(18, 31 * G, 4 * G, 2, None), 3); // 7 slots / 2
        assert_eq!(initial_workers(18, 31 * G, 4 * G, 6, None), 1);
        // Never zero, even when one worker's cap exceeds total RAM.
        assert_eq!(initial_workers(18, 2 * G, 4 * G, 2, None), 1);
        // --eval-workers wins verbatim (floored at 1).
        assert_eq!(initial_workers(18, 31 * G, 4 * G, 2, Some(9)), 9);
        assert_eq!(initial_workers(18, 31 * G, 4 * G, 2, Some(0)), 1);
    }
}
