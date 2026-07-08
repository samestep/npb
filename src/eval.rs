//! Evaluate a nixpkgs revision into an `attr -> drv` map via `nix-eval-jobs`,
//! cached in the SQLite store. This is the first spine primitive (DESIGN.md §6,
//! §9): a pure fact keyed by `(commit, system, profile)`, computed at most once.
//!
//! The revision's source comes from `builtins.fetchGit`, so Nix fetches and
//! caches it in the store — npd manages no worktrees. `nix-eval-jobs` output is
//! parsed by streaming NDJSON straight off the child's stdout (never buffering
//! the whole, meta-heavy output).

use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use indicatif::{MultiProgress, ProgressBar};
use serde::Deserialize;

use crate::model::{AttrEval, Existence};

/// Bumped if we change *how* we invoke `nix-eval-jobs` in a way that could alter
/// the attr->drv map; cache entries under a different version are ignored.
pub const EVAL_VERSION: u32 = 1;

/// The default (and, for now, only) eval profile. npd owns the config so the key
/// stays a short enumerable label rather than arbitrary Nix (DESIGN.md §6). The
/// allow-flags are on so meta-blocked packages still yield a drv + meta rather
/// than throwing — we want their drvpath and the option to build them anyway.
pub const DEFAULT_PROFILE: &str = "default";

fn profile_config(profile: &str) -> Result<&'static str> {
    match profile {
        "default" => Ok("{ allowBroken = true; allowUnfree = true; \
                          allowUnsupportedSystem = true; allowInsecurePredicate = _: true; }"),
        other => bail!("unknown eval profile: {other:?}"),
    }
}

// --- nix-eval-jobs output ---------------------------------------------------

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
    drv_path: Option<String>,
    error: Option<String>,
    meta: Option<RawMeta>,
}

fn raw_to_attr_eval(raw: RawJob) -> AttrEval {
    let RawJob {
        attr,
        drv_path,
        error,
        meta,
    } = raw;
    match drv_path {
        Some(drv) => {
            let meta = meta.unwrap_or_default();
            let blocked = matches!(meta.broken, Some(true))
                || matches!(meta.unsupported, Some(true))
                || matches!(meta.insecure, Some(true));
            AttrEval {
                attr,
                existence: if blocked {
                    Existence::Blocked
                } else {
                    Existence::Buildable
                },
                drv_path: Some(drv),
                broken: meta.broken,
                unsupported: meta.unsupported,
                insecure: meta.insecure,
                hydra_platforms_ok: None,
                error: None,
            }
        }
        None => AttrEval {
            attr,
            existence: Existence::Error,
            drv_path: None,
            broken: None,
            unsupported: None,
            insecure: None,
            hydra_platforms_ok: None,
            error,
        },
    }
}

/// Stream NDJSON values off `reader`, mapping each to an `AttrEval`. Memory stays
/// bounded to one value at a time rather than the whole (meta-heavy) output.
fn parse_jobs<R: std::io::Read>(reader: R) -> Result<Vec<AttrEval>> {
    let mut out = Vec::new();
    for item in serde_json::Deserializer::from_reader(reader).into_iter::<RawJob>() {
        let raw = item.context("parsing nix-eval-jobs output")?;
        out.push(raw_to_attr_eval(raw));
    }
    Ok(out)
}

// --- running the evaluator --------------------------------------------------

/// Build the whole-package-set Nix expression `nix-eval-jobs` walks. The
/// revision's source is fetched by `builtins.fetchGit`.
fn build_expr(repo: &Path, commit: &str, system: &str, profile: &str) -> Result<String> {
    let cfg = profile_config(profile)?;
    Ok(format!(
        "import (builtins.fetchGit {{ url = \"{}\"; rev = \"{commit}\"; }}) \
         {{ system = \"{system}\"; config = {cfg}; }}",
        repo.display()
    ))
}

/// Run one `nix-eval-jobs` invocation with `workers` worker processes, each
/// heap-capped at `per_worker_mb` (nix-eval-jobs restarts a worker that exceeds
/// it, so total memory ≈ `workers * per_worker_mb`). Progress is streamed onto
/// the caller-supplied `pb`, letting several evals share one MultiProgress.
fn run_eval_pb(
    repo: &Path,
    commit: &str,
    system: &str,
    profile: &str,
    workers: usize,
    per_worker_mb: u64,
    pb: &ProgressBar,
) -> Result<Vec<AttrEval>> {
    let expr = build_expr(repo, commit, system, profile)?;
    // Send stderr to a log file rather than inheriting it: nix-eval-jobs prints a
    // full Nix traceback per errored attr (megabytes over a whole package set),
    // and the actionable per-attr error is already in the stdout JSON. A file
    // (unlike an undrained pipe) also can't deadlock while we stream stdout.
    // One log per (commit, system) so concurrent evals don't clobber each other.
    let short: String = commit.chars().take(12).collect();
    let log_dir = cache_root()?.join("logs");
    fs::create_dir_all(&log_dir).context("creating log dir")?;
    let log_path = log_dir.join(format!("eval-{short}-{system}.log"));
    let log = File::create(&log_path).context("creating eval log")?;

    let mut child = Command::new("nix-eval-jobs")
        .args([
            "--meta",
            "--workers",
            &workers.to_string(),
            "--max-memory-size",
            &per_worker_mb.to_string(),
            "--expr",
            &expr,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log))
        .spawn()
        .context("spawning nix-eval-jobs (on PATH? use the flake dev shell)")?;
    let stdout = child.stdout.take().expect("stdout is piped");

    // A full-set eval takes minutes; a per-attr message on a steady-tick spinner
    // keeps it visibly alive (the tick throttles the repaint, like npc).
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_message(format!("evaluating {short} ({system}, {workers}w)…"));
    let mut attrs = Vec::new();
    for item in serde_json::Deserializer::from_reader(BufReader::new(stdout)).into_iter::<RawJob>() {
        attrs.push(raw_to_attr_eval(item.context("parsing nix-eval-jobs output")?));
        pb.set_message(format!("evaluating {short} ({system}, {workers}w)… {} attrs", attrs.len()));
    }
    pb.finish_with_message(format!("evaluated {short} ({system}): {} attrs", attrs.len()));

    let status = child.wait().context("waiting for nix-eval-jobs")?;
    // Integrity gate. Per-attr eval errors are emitted *in band* as JSON
    // (`{"attr":…,"error":…}`) and do NOT affect the exit code — a complete
    // full-set eval exits 0 even with thousands of `throw`n attrs. A non-zero
    // exit means a *fatal* abort: a worker died mid-eval (most often an OOM
    // SIGKILL when the workers' memory caps oversubscribe RAM), in which case
    // the streamed output is silently TRUNCATED — we got some attrs but not
    // all. Caching that would poison every future diff/report with phantom
    // "removed" packages, so we refuse it outright rather than trust a partial.
    if !status.success() {
        bail!(
            "nix-eval-jobs did not finish evaluating {commit} ({system}): it exited \
             {status} after streaming {} attr(s), so the result is truncated and \
             will NOT be cached. A worker most likely died — commonly out-of-memory: \
             reduce the worker count or --max-memory-size so their caps fit in RAM. \
             Last stderr from {}:\n{}",
            attrs.len(),
            log_path.display(),
            tail(&log_path, 20),
        );
    }
    Ok(attrs)
}

// --- concurrency: a memory-slot budget over parallel evals -------------------

/// Default per-worker heap cap (matches nix-eval-jobs' own 4 GiB default).
const DEFAULT_WORKER_MEM_MB: u64 = 4096;
/// A single eval sees diminishing returns past this many workers (each worker
/// redundantly re-evaluates the package-set spine), so cap the auto-derived
/// width here even when the RAM budget could afford more.
const MAX_WORKERS_PER_EVAL: u64 = 8;

/// Optional overrides for the parallel-eval sizing (see [`eval_plan`]). Every
/// field `None` means "auto-size from system RAM"; the CLI surfaces each as a
/// global flag so the scheme can be tuned point-by-point without env vars.
#[derive(Debug, Clone, Copy, Default)]
pub struct EvalOpts {
    pub mem_budget_mb: Option<u64>,
    pub worker_mem_mb: Option<u64>,
    pub concurrency: Option<u64>,
    pub workers: Option<u64>,
}

/// Total system RAM in MiB (Linux `/proc/meminfo`); a conservative fallback
/// elsewhere (e.g. macOS) since we only auto-size on the Linux build boxes.
fn total_mem_mb() -> u64 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse::<u64>().ok())
        })
        .map(|kb| kb / 1024)
        .unwrap_or(8192)
}

/// How to run a batch of `n_jobs` evals: how many at once, how wide each, and
/// the per-worker heap cap. Derived from a RAM budget (default 80% of system
/// RAM) divided into `per_worker_mb` slots; each knob is overridable via
/// [`EvalOpts`] so the scheme can be benchmarked point-by-point.
struct EvalPlan {
    concurrency: usize,
    workers: usize,
    per_worker_mb: u64,
    budget_mb: u64,
    slots: u64,
}

fn eval_plan(n_jobs: usize, opts: EvalOpts) -> EvalPlan {
    let per_worker_mb = opts.worker_mem_mb.unwrap_or(DEFAULT_WORKER_MEM_MB);
    let budget_mb = opts.mem_budget_mb.unwrap_or_else(|| total_mem_mb() * 8 / 10);
    let slots = (budget_mb / per_worker_mb).max(1);
    // Run as many evals at once as fit in the budget (but no more than we have),
    // splitting the slots evenly across them; each override wins if set.
    let concurrency = opts
        .concurrency
        .unwrap_or(slots)
        .min(n_jobs.max(1) as u64)
        .max(1);
    let workers = opts
        .workers
        .unwrap_or_else(|| (slots / concurrency).clamp(1, MAX_WORKERS_PER_EVAL))
        .max(1);
    EvalPlan {
        concurrency: concurrency as usize,
        workers: workers as usize,
        per_worker_mb,
        budget_mb,
        slots,
    }
}

/// A counting semaphore (std has none): admits at most `permits` evals at once.
struct Semaphore {
    m: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new(permits: usize) -> Self {
        Semaphore {
            m: Mutex::new(permits),
            cv: Condvar::new(),
        }
    }
    fn acquire(&self) {
        let mut n = self.m.lock().unwrap();
        while *n == 0 {
            n = self.cv.wait(n).unwrap();
        }
        *n -= 1;
    }
    fn release(&self) {
        *self.m.lock().unwrap() += 1;
        self.cv.notify_one();
    }
}

/// The last `lines` lines of a (possibly large) file; empty if unreadable.
fn tail(path: &Path, lines: usize) -> String {
    match fs::read_to_string(path) {
        Ok(s) => {
            let all: Vec<&str> = s.lines().collect();
            all[all.len().saturating_sub(lines)..].join("\n")
        }
        Err(_) => String::new(),
    }
}

// --- cache ------------------------------------------------------------------

pub fn cache_root() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("could not determine cache directory")?
        .join("nix-npd"))
}

pub fn db_path() -> Result<PathBuf> {
    Ok(cache_root()?.join("npd.sqlite"))
}

// --- eval files -------------------------------------------------------------
//
// Each eval is a standalone file under `<cache>/evals/`, not SQLite rows. It's a
// bulk, write-once, read-as-a-whole artifact — the only thing we ever do with it
// is diff two of them — so a flat file is both smaller (no per-row / index
// overhead; ~11 MB vs ~22 MB in SQLite) and lets us evict by whole file (drop
// old commits' evals) without vacuuming a monolithic DB. The format is one
// `attr\tdrv` line per attr, sorted by attr (empty drv = no derivation), so the
// diff is a linear two-pointer merge.

fn eval_path(commit: &str, system: &str, profile: &str) -> Result<PathBuf> {
    Ok(cache_root()?
        .join("evals")
        .join(format!("{commit}-{system}-{profile}-v{EVAL_VERSION}.tsv")))
}

/// Write an eval to its file, sorted by attr, atomically (temp + rename) so a
/// crash can never leave a truncated file that would poison the cache.
fn write_eval(path: &Path, attrs: &[AttrEval]) -> Result<()> {
    let mut rows: Vec<(&str, &str)> = attrs
        .iter()
        .map(|a| (a.attr.as_str(), a.drv_path.as_deref().unwrap_or("")))
        .collect();
    rows.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut buf = String::with_capacity(rows.len() * 96);
    for (attr, drv) in rows {
        buf.push_str(attr);
        buf.push('\t');
        buf.push_str(drv);
        buf.push('\n');
    }
    fs::create_dir_all(path.parent().unwrap()).context("creating evals dir")?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, &buf).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).context("renaming eval into place")?;
    Ok(())
}

/// Parse an eval file's bytes into `(attr, Option<drv>)` pairs, borrowing from
/// `buf` (no per-attr allocation). Assumes the file is already sorted by attr.
fn parse_eval(buf: &str) -> Vec<(&str, Option<&str>)> {
    buf.lines()
        .map(|l| {
            let (attr, drv) = l.split_once('\t').unwrap_or((l, ""));
            (attr, if drv.is_empty() { None } else { Some(drv) })
        })
        .collect()
}

/// One changed attr between two evals: its path and its drv on each side (`None`
/// = absent/no derivation there).
pub type ChangedDrv = (String, Option<String>, Option<String>);

/// The changed set between two cached evals — one [`ChangedDrv`] for each attr
/// whose drv differs — via a linear two-pointer merge over the two sorted files.
/// Only the (few) changed rows are allocated.
pub fn changed_set(base: &str, head: &str, system: &str, profile: &str) -> Result<Vec<ChangedDrv>> {
    let bp = eval_path(base, system, profile)?;
    let hp = eval_path(head, system, profile)?;
    let bbuf = fs::read_to_string(&bp).with_context(|| format!("reading {}", bp.display()))?;
    let hbuf = fs::read_to_string(&hp).with_context(|| format!("reading {}", hp.display()))?;
    let b = parse_eval(&bbuf);
    let h = parse_eval(&hbuf);

    let own = |s: Option<&str>| s.map(str::to_string);
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < b.len() && j < h.len() {
        match b[i].0.cmp(h[j].0) {
            std::cmp::Ordering::Less => {
                if b[i].1.is_some() {
                    out.push((b[i].0.to_string(), own(b[i].1), None));
                }
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                if h[j].1.is_some() {
                    out.push((h[j].0.to_string(), None, own(h[j].1)));
                }
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                if b[i].1 != h[j].1 {
                    out.push((b[i].0.to_string(), own(b[i].1), own(h[j].1)));
                }
                i += 1;
                j += 1;
            }
        }
    }
    for k in &b[i..] {
        if k.1.is_some() {
            out.push((k.0.to_string(), own(k.1), None));
        }
    }
    for k in &h[j..] {
        if k.1.is_some() {
            out.push((k.0.to_string(), None, own(k.1)));
        }
    }
    Ok(out)
}

/// Ensure every `(commit, system)` pair has a cached eval file, computing the
/// misses concurrently under a RAM-slot budget (see [`eval_plan`]) — no
/// oversubscription, no killing in-flight work.
pub fn eval_pairs(
    repo: &Path,
    pairs: &[(String, String)],
    profile: &str,
    opts: EvalOpts,
) -> Result<()> {
    let mut todo: Vec<usize> = Vec::new();
    for (i, (commit, system)) in pairs.iter().enumerate() {
        if !eval_path(commit, system, profile)?.exists() {
            todo.push(i);
        }
    }
    if todo.is_empty() {
        return Ok(());
    }

    let plan = eval_plan(todo.len(), opts);
    eprintln!(
        "  eval plan: {} job(s), budget {}MB / {}MB per worker = {} slot(s) \
         -> {} concurrent x {} worker(s)",
        todo.len(),
        plan.budget_mb,
        plan.per_worker_mb,
        plan.slots,
        plan.concurrency,
        plan.workers,
    );
    let sem = Semaphore::new(plan.concurrency);
    let mp = MultiProgress::new();
    let computed: Vec<(usize, Vec<AttrEval>)> =
        thread::scope(|s| -> Result<Vec<(usize, Vec<AttrEval>)>> {
            let mut handles = Vec::new();
            for &i in &todo {
                let (commit, system) = (&pairs[i].0, &pairs[i].1);
                let pb = mp.add(ProgressBar::new_spinner());
                let sem = &sem;
                handles.push(s.spawn(move || -> Result<(usize, Vec<AttrEval>)> {
                    sem.acquire();
                    let r =
                        run_eval_pb(repo, commit, system, profile, plan.workers, plan.per_worker_mb, &pb);
                    sem.release();
                    Ok((i, r?))
                }));
            }
            let mut out = Vec::new();
            for h in handles {
                out.push(h.join().expect("eval thread panicked")?);
            }
            Ok(out)
        })?;
    for (i, attrs) in &computed {
        let (commit, system) = &pairs[*i];
        write_eval(&eval_path(commit, system, profile)?, attrs)?;
    }
    Ok(())
}

/// Ensure both commits are evaluated across all systems (they run concurrently).
pub fn eval_two(
    repo: &Path,
    base: &str,
    head: &str,
    systems: &[String],
    profile: &str,
    opts: EvalOpts,
) -> Result<()> {
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(systems.len() * 2);
    for s in systems {
        pairs.push((base.to_string(), s.clone()));
    }
    for s in systems {
        pairs.push((head.to_string(), s.clone()));
    }
    eval_pairs(repo, &pairs, profile, opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success_blocked_and_error_lines() {
        let stdout = concat!(
            r#"{"attr":"hello","attrPath":["hello"],"drvPath":"/nix/store/a-hello.drv","meta":{"broken":false,"unsupported":false},"system":"aarch64-linux"}"#,
            "\n",
            r#"{"attr":"br","drvPath":"/nix/store/b-br.drv","meta":{"broken":true}}"#,
            "\n",
            r#"{"attr":"bad","attrPath":["bad"],"error":"boom","fatal":false}"#,
            "\n",
        );
        let attrs = parse_jobs(stdout.as_bytes()).unwrap();
        assert_eq!(attrs.len(), 3);

        assert_eq!(attrs[0].attr, "hello");
        assert_eq!(attrs[0].existence, Existence::Buildable);
        assert_eq!(attrs[0].drv_path.as_deref(), Some("/nix/store/a-hello.drv"));

        assert_eq!(attrs[1].existence, Existence::Blocked);
        assert_eq!(attrs[1].broken, Some(true));
        assert!(attrs[1].drv_path.is_some());

        assert_eq!(attrs[2].existence, Existence::Error);
        assert_eq!(attrs[2].drv_path, None);
        assert_eq!(attrs[2].error.as_deref(), Some("boom"));
    }

    #[test]
    fn full_expr_fetches_and_imports() {
        let repo = Path::new("/repo");
        let full = build_expr(repo, "abc123", "aarch64-linux", "default").unwrap();
        assert!(full.contains(r#"builtins.fetchGit { url = "/repo"; rev = "abc123"; }"#));
        assert!(full.contains("allowBroken = true"));
    }

    #[test]
    fn tail_returns_last_lines() {
        let dir = std::env::temp_dir().join(format!("npd-tail-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.log");
        fs::write(&path, "l1\nl2\nl3\nl4\n").unwrap();
        assert_eq!(tail(&path, 2), "l3\nl4");
        assert_eq!(tail(&path, 99), "l1\nl2\nl3\nl4");
        assert_eq!(tail(&dir.join("missing"), 5), "");
        let _ = fs::remove_dir_all(&dir);
    }
}
