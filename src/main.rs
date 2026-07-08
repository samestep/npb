//! npd — a persistent fact store for iterating on nixpkgs changes.
//!
//! See DESIGN.md for the architecture. The pure data model lives in [`model`];
//! `npd` is a single command that evaluates a `base → head` change, builds
//! whatever the changed set needs, and renders a Markdown report.

// A few model/eval helpers are kept as clean public API even where the single
// command doesn't currently exercise them.
#![allow(dead_code)]

mod build;
mod cache;
mod eval;
mod model;
mod report;
mod store;

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command as Proc;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser};

use crate::model::BuildPolicy;

#[derive(Parser)]
#[command(name = "npd", version, about = "A persistent fact store for iterating on nixpkgs changes")]
struct Cli {
    /// Base revision (default: merge-base of head and `master`).
    base: Option<String>,
    /// Head revision (default: `HEAD`).
    head: Option<String>,
    /// nixpkgs clone to resolve the commits in (default: current directory).
    #[arg(long)]
    nixpkgs: Option<PathBuf>,
    /// Systems to report on (repeatable); defaults to the host system.
    #[arg(long)]
    system: Vec<String>,
    /// Don't build; render only from facts already in the log (may show `❓`).
    #[arg(long)]
    no_build: bool,
    /// Rebuild even a previously-succeeded drv (suspect a flaky success).
    #[arg(long)]
    recheck: bool,
    /// Re-attempt a previously-failed drv (expect it might pass now).
    #[arg(long)]
    retry: bool,
    /// Ignore a substitutable (cached) success; require a genuine local build.
    #[arg(long)]
    prefer_local: bool,
    #[command(flatten)]
    eval: EvalArgs,
}

/// Parallel-evaluation sizing knobs. Each unset flag is auto-sized from system
/// RAM (see `eval::eval_plan`).
#[derive(Args, Clone, Copy, Default)]
struct EvalArgs {
    /// RAM budget for parallel evaluation, MiB (default: 80% of *available* RAM).
    #[arg(long)]
    mem_budget_mb: Option<u64>,
    /// Per-`nix-eval-jobs`-worker heap cap, MiB (default: 4096).
    #[arg(long)]
    worker_mem_mb: Option<u64>,
    /// Number of evaluations to run at once (default: auto from the RAM budget).
    #[arg(long)]
    eval_concurrency: Option<u64>,
    /// `nix-eval-jobs` workers per evaluation (default: auto, clamped 1–8).
    #[arg(long)]
    eval_workers: Option<u64>,
}

impl EvalArgs {
    fn opts(self) -> eval::EvalOpts {
        eval::EvalOpts {
            mem_budget_mb: self.mem_budget_mb,
            worker_mem_mb: self.worker_mem_mb,
            concurrency: self.eval_concurrency,
            workers: self.eval_workers,
        }
    }
}

/// The host Nix system double, e.g. `aarch64-linux`.
fn host_system() -> String {
    let arch = std::env::consts::ARCH; // e.g. "aarch64", "x86_64"
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other, // "linux"
    };
    format!("{arch}-{os}")
}

/// The nixpkgs clone to operate on: `--nixpkgs` if given, else the current
/// directory (assumed to be the root of a nixpkgs checkout). Resolved to an
/// absolute path so `git -C` and `builtins.fetchGit` both accept it.
fn resolve_repo(nixpkgs: Option<PathBuf>) -> Result<PathBuf> {
    match nixpkgs {
        Some(p) => Ok(p),
        None => std::env::current_dir()
            .context("could not determine the current directory; pass --nixpkgs <path>"),
    }
}

fn resolve_systems(system: Vec<String>) -> Vec<String> {
    if system.is_empty() {
        vec![host_system()]
    } else {
        system
    }
}

/// `git merge-base base head` in `repo`.
fn git_merge_base(repo: &std::path::Path, base: &str, head: &str) -> Result<String> {
    let out = Proc::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", base, head])
        .output()
        .context("running git merge-base")?;
    if !out.status.success() {
        bail!(
            "git merge-base {base} {head} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

/// Resolve a revision (ref, short/full sha, tag, `HEAD~1`, …) to a full commit
/// sha, so callers can use friendly names even though `fetchGit` needs a rev.
fn resolve_commit(repo: &std::path::Path, rev: &str) -> Result<String> {
    let out = Proc::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{rev}^{{commit}}"))
        .output()
        .context("running git rev-parse")?;
    if !out.status.success() {
        bail!("cannot resolve revision {rev:?} in {}", repo.display());
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

/// Resolve report revisions with ergonomic defaults: head defaults to `HEAD`,
/// base to the merge-base of head and `master` (the fork point of this branch).
fn resolve_base_head(
    repo: &std::path::Path,
    base: Option<String>,
    head: Option<String>,
) -> Result<(String, String)> {
    let head = resolve_commit(repo, &head.unwrap_or_else(|| "HEAD".to_string()))?;
    let base = match base {
        Some(b) => resolve_commit(repo, &b)?,
        None => git_merge_base(repo, "master", &head)
            .context("no base given and could not merge-base with `master`; pass one explicitly")?,
    };
    Ok((base, head))
}

fn run(cli: Cli) -> Result<()> {
    let policy = BuildPolicy {
        recheck: cli.recheck,
        retry: cli.retry,
        prefer_local: cli.prefer_local,
    };
    let opts = cli.eval.opts();
    let repo = resolve_repo(cli.nixpkgs)?;
    let (base, head) = resolve_base_head(&repo, cli.base, cli.head)?;
    let systems = resolve_systems(cli.system);

    eval::eval_two(&repo, &base, &head, &systems, eval::DEFAULT_PROFILE, opts)?;

    // The changed set per system — `(attr, base_drv, head_drv)` — from a linear
    // merge of the two sorted eval files. Computed once, reused for build+render.
    let mut per_system_changed: Vec<(String, Vec<eval::ChangedDrv>)> = Vec::new();
    for sys in &systems {
        let changed = eval::changed_set(&base, &head, sys, eval::DEFAULT_PROFILE)?;
        per_system_changed.push((sys.clone(), changed));
    }

    // Build both sides of the changed set (skipping anything already known or
    // substitutable) so the report has a real state for every row, not a `❓`.
    if !cli.no_build {
        let mut targets = Vec::new();
        let mut seen = HashSet::new();
        for (sys, changed) in &per_system_changed {
            for (attr, base_drv, head_drv) in changed {
                for drv in [base_drv, head_drv].into_iter().flatten() {
                    if seen.insert((drv.clone(), sys.clone())) {
                        targets.push(build::Target {
                            attr: attr.clone(),
                            system: sys.clone(),
                            drv_path: drv.clone(),
                        });
                    }
                }
            }
        }
        if !targets.is_empty() {
            build::build_targets(&targets, policy, false)?;
        }
    }

    // Render from the (now-populated) log: reduce each side to a state.
    let store = store::Store::open(&eval::db_path()?)?;
    let mut per_system = Vec::new();
    for (sys, changed) in &per_system_changed {
        let mut entries = Vec::new();
        for (attr, base_drv, head_drv) in changed {
            let base_obs = match base_drv {
                Some(d) => store.load_observations(d)?,
                None => Vec::new(),
            };
            let head_obs = match head_drv {
                Some(d) => store.load_observations(d)?,
                None => Vec::new(),
            };
            entries.push(report::Entry {
                attr: attr.clone(),
                base_drv: base_drv.clone(),
                head_drv: head_drv.clone(),
                base: report::side_state(base_drv, &base_obs),
                head: report::side_state(head_drv, &head_obs),
            });
        }
        per_system.push((sys.clone(), entries));
    }
    print!("{}", report::render(&base, &head, &per_system));
    Ok(())
}

fn main() -> Result<()> {
    run(Cli::parse())
}
