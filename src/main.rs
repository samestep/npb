//! npd — a persistent fact store for iterating on nixpkgs changes.
//!
//! See DESIGN.md for the architecture. The pure data model lives in [`model`];
//! orchestration (eval / diff / build / report) is built spine-first.

// Scaffolding: some model types are defined ahead of the orchestration that will
// consume them (see DESIGN.md build order). Drop this once build/report land.
#![allow(dead_code)]

mod build;
mod cache;
mod diff;
mod eval;
mod model;
mod report;
mod store;

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command as Proc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::diff::{Attribution, DiffKind};
use crate::model::{BuildPolicy, Decision, Existence, Outcome};

#[derive(Parser)]
#[command(name = "npd", version, about = "A persistent fact store for iterating on nixpkgs changes")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Evaluate a revision into an attr->drv map (cached; pure).
    Eval {
        /// Git commit / revision to evaluate.
        commit: String,
        /// Attribute paths to scope to (dotted, e.g. `python3Packages.numpy`).
        /// Omit to evaluate the whole package set (that result is cached).
        attrs: Vec<String>,
        /// nixpkgs clone to resolve the commit in (default: current directory).
        #[arg(long)]
        nixpkgs: Option<PathBuf>,
        /// Systems to evaluate for (repeatable); defaults to the host system.
        #[arg(long)]
        system: Vec<String>,
        /// Eval profile (config npd owns); defaults to `default`.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Diff two revisions into a set of changed attrs (optionally three-way via merge base).
    Diff {
        base: String,
        head: String,
        /// Attribute paths to scope to (dotted). Omit for the whole package set.
        attrs: Vec<String>,
        /// Also evaluate the merge base to attribute each change (git-3-way style).
        #[arg(long)]
        three_way: bool,
        /// nixpkgs clone to resolve the commits in (default: current directory).
        #[arg(long)]
        nixpkgs: Option<PathBuf>,
        /// Systems to diff on (repeatable); defaults to the host system.
        #[arg(long)]
        system: Vec<String>,
        /// Eval profile (config npd owns); defaults to `default`.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Build derivations, consulting (and appending to) the observation log.
    Build {
        /// Git commit / revision to build at (the "head" for --changed).
        commit: String,
        /// Attribute paths to build (dotted). Provide these or --changed.
        attrs: Vec<String>,
        /// Build the reverse-closure changed between <base> and <commit>
        /// (i.e. diff base..commit, build the Changed+Added set) instead of
        /// explicit attrs.
        #[arg(long, value_name = "base")]
        changed: Option<String>,
        /// Show what would be built (decisions per target) without building.
        #[arg(long)]
        dry_run: bool,
        /// nixpkgs clone to resolve the commit in (default: current directory).
        #[arg(long)]
        nixpkgs: Option<PathBuf>,
        /// Systems to build for (repeatable); defaults to the host system.
        #[arg(long)]
        system: Vec<String>,
        /// Rebuild even a previously-succeeded drv (suspect a flaky success).
        #[arg(long)]
        recheck: bool,
        /// Re-attempt a previously-failed drv (expect it might pass now).
        #[arg(long)]
        retry: bool,
        /// Ignore a substitutable (cached) success; require a genuine local build.
        #[arg(long)]
        prefer_local: bool,
    },
    /// Render a Markdown report classifying the changed set between two revisions,
    /// building whatever the verdicts need first (so there are no `?`).
    ///
    /// With no arguments: head = `HEAD`, base = merge-base of `HEAD` and `master`.
    /// With one: it is the base, and head = `HEAD`.
    Report {
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
        /// Don't build; render only from facts already in the log (may show `?`).
        #[arg(long)]
        no_build: bool,
    },
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

/// Short, human-scannable form of a drv path: its store hash prefix.
fn short_drv(drv: &Option<String>) -> String {
    match drv {
        None => "∅".to_string(),
        Some(d) => d
            .rsplit('/')
            .next()
            .and_then(|n| n.split('-').next())
            .map(|h| h.chars().take(12).collect())
            .unwrap_or_else(|| d.clone()),
    }
}

/// The attrs an eval produced for `sys` (empty if that system wasn't evaluated).
fn attrs_for(evals: &[eval::Eval], sys: &str) -> Vec<model::AttrEval> {
    evals
        .iter()
        .find(|e| e.system == sys)
        .map(|e| e.attrs.clone())
        .unwrap_or_default()
}

fn cmd_eval(
    commit: String,
    attrs: Vec<String>,
    nixpkgs: Option<PathBuf>,
    system: Vec<String>,
    profile: Option<String>,
) -> Result<()> {
    let repo = resolve_repo(nixpkgs)?;
    let commit = resolve_commit(&repo, &commit)?;
    let systems = resolve_systems(system);
    let profile = profile.unwrap_or_else(|| eval::DEFAULT_PROFILE.to_string());

    for e in eval::eval_commit(&repo, &commit, &systems, &profile, &attrs)? {
        let (mut buildable, mut blocked, mut errored) = (0, 0, 0);
        for a in &e.attrs {
            match a.existence {
                Existence::Buildable => buildable += 1,
                Existence::Blocked => blocked += 1,
                Existence::Error => errored += 1,
                Existence::Absent => {}
            }
        }
        let origin = if e.from_cache { "cached" } else { "fresh" };
        println!(
            "{}: {} attrs (buildable={buildable} blocked={blocked} error={errored}) [{origin}]",
            e.system,
            e.attrs.len()
        );
        // For a scoped eval, show each attr's verdict — that's the whole point.
        if !attrs.is_empty() {
            for a in &e.attrs {
                match &a.drv_path {
                    Some(d) => println!("  {:?}  {}  {d}", a.existence, a.attr),
                    None => println!("  {:?}  {}", a.existence, a.attr),
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_diff(
    base: String,
    head: String,
    attrs: Vec<String>,
    three_way: bool,
    nixpkgs: Option<PathBuf>,
    system: Vec<String>,
    profile: Option<String>,
) -> Result<()> {
    let repo = resolve_repo(nixpkgs)?;
    let base = resolve_commit(&repo, &base)?;
    let head = resolve_commit(&repo, &head)?;
    let systems = resolve_systems(system);
    let profile = profile.unwrap_or_else(|| eval::DEFAULT_PROFILE.to_string());

    let merge_base = if three_way {
        Some(git_merge_base(&repo, &base, &head)?)
    } else {
        None
    };

    let (base_evals, head_evals) = eval::eval_two(&repo, &base, &head, &systems, &profile, &attrs)?;
    let mb_evals = match &merge_base {
        Some(c) => Some(eval::eval_commit(&repo, c, &systems, &profile, &attrs)?),
        None => None,
    };
    if let Some(c) = &merge_base {
        println!("merge base: {c}");
    }
    for sys in &systems {
        let b = attrs_for(&base_evals, sys);
        let h = attrs_for(&head_evals, sys);
        let m = mb_evals.as_ref().map(|e| attrs_for(e, sys));
        let d = diff::diff_evals(&b, &h, m.as_deref());

        let (mut changed, mut added, mut removed, mut unchanged) = (0, 0, 0, 0);
        let (mut by_head, mut by_base, mut by_both) = (0, 0, 0);
        for e in &d {
            match e.kind {
                DiffKind::Changed => changed += 1,
                DiffKind::Added => added += 1,
                DiffKind::Removed => removed += 1,
                DiffKind::Unchanged => unchanged += 1,
            }
            match e.attribution {
                Some(Attribution::ByHead) => by_head += 1,
                Some(Attribution::ByBase) => by_base += 1,
                Some(Attribution::ByBoth) => by_both += 1,
                None => {}
            }
        }
        print!("{sys}: {changed} changed, {added} added, {removed} removed ({unchanged} unchanged)");
        if three_way {
            print!(" [by-head={by_head} by-base={by_base} by-both={by_both}]");
        }
        println!();

        let mut shown = 0;
        for e in d.iter().filter(|e| e.kind != DiffKind::Unchanged) {
            if shown == 50 {
                println!("  ... and {} more", changed + added + removed - shown);
                break;
            }
            let tag = match e.attribution {
                Some(Attribution::ByHead) => " (by-head)",
                Some(Attribution::ByBase) => " (by-base)",
                Some(Attribution::ByBoth) => " (by-both)",
                None => "",
            };
            println!(
                "  {:?}  {}  {} -> {}{tag}",
                e.kind,
                e.attr,
                short_drv(&e.base_drv),
                short_drv(&e.head_drv)
            );
            shown += 1;
        }
    }
    Ok(())
}

/// Build targets from an explicit scoped eval at `commit`.
fn targets_from_attrs(
    repo: &std::path::Path,
    commit: &str,
    systems: &[String],
    attrs: &[String],
) -> Result<Vec<build::Target>> {
    let evals = eval::eval_commit(repo, commit, systems, eval::DEFAULT_PROFILE, attrs)?;
    let mut targets = Vec::new();
    for e in &evals {
        for a in &e.attrs {
            if let Some(drv) = &a.drv_path {
                targets.push(build::Target {
                    attr: a.attr.clone(),
                    system: e.system.clone(),
                    drv_path: drv.clone(),
                });
            }
        }
    }
    Ok(targets)
}

/// Build targets = the reverse-closure changed between `base` and `commit`
/// (the Changed + Added entries of the full-set diff, at the head drv).
fn targets_from_diff(
    repo: &std::path::Path,
    base: &str,
    commit: &str,
    systems: &[String],
) -> Result<Vec<build::Target>> {
    let (base_evals, head_evals) =
        eval::eval_two(repo, base, commit, systems, eval::DEFAULT_PROFILE, &[])?;
    let mut targets = Vec::new();
    for sys in systems {
        let b = attrs_for(&base_evals, sys);
        let h = attrs_for(&head_evals, sys);
        for e in diff::diff_evals(&b, &h, None) {
            if matches!(e.kind, DiffKind::Changed | DiffKind::Added)
                && let Some(drv) = e.head_drv
            {
                targets.push(build::Target {
                    attr: e.attr,
                    system: sys.clone(),
                    drv_path: drv,
                });
            }
        }
    }
    Ok(targets)
}

#[allow(clippy::too_many_arguments)]
fn cmd_build(
    commit: String,
    attrs: Vec<String>,
    changed: Option<String>,
    dry_run: bool,
    nixpkgs: Option<PathBuf>,
    system: Vec<String>,
    recheck: bool,
    retry: bool,
    prefer_local: bool,
) -> Result<()> {
    let repo = resolve_repo(nixpkgs)?;
    let commit = resolve_commit(&repo, &commit)?;
    let systems = resolve_systems(system);
    let policy = BuildPolicy {
        recheck,
        retry,
        prefer_local,
    };

    let targets = match (&changed, attrs.is_empty()) {
        (Some(_), false) => bail!("npd build: pass either attrs or --changed <base>, not both"),
        (Some(base), true) => {
            let base = resolve_commit(&repo, base)?;
            targets_from_diff(&repo, &base, &commit, &systems)?
        }
        (None, false) => targets_from_attrs(&repo, &commit, &systems, &attrs)?,
        (None, true) => bail!(
            "npd build: pass one or more attrs, or --changed <base> \
             (building the whole package set is almost never intended)"
        ),
    };

    if targets.is_empty() {
        println!("nothing to build (empty changed set)");
        return Ok(());
    }
    // build_targets streams per-target progress; we just tally the summary.
    let built = build::build_targets(&targets, policy, dry_run)?;

    let (mut ok, mut failed, mut dep_failed, mut would, mut skip_ok, mut skip_fail) =
        (0, 0, 0, 0, 0, 0);
    for r in &built {
        match (r.decision, r.outcome) {
            (Decision::Build, Some(Outcome::Built)) => ok += 1,
            (Decision::Build, Some(Outcome::Failed)) => failed += 1,
            (Decision::Build, Some(Outcome::DepFailed)) => dep_failed += 1,
            (Decision::Build, None) => would += 1,
            (Decision::SkipOk, _) => skip_ok += 1,
            (Decision::SkipFail, _) => skip_fail += 1,
            _ => {}
        }
    }
    if dry_run {
        println!("would-build={would} skipped-ok={skip_ok} skipped-fail={skip_fail} ({} targets)", built.len());
    } else {
        println!(
            "built={ok} failed={failed} dep-failed={dep_failed} \
             skipped-ok={skip_ok} skipped-fail={skip_fail}"
        );
        if failed + dep_failed > 0 {
            bail!("{failed} failed, {dep_failed} dep-failed");
        }
    }
    Ok(())
}

/// Resolve report/… revisions with ergonomic defaults: head defaults to `HEAD`,
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

fn cmd_report(
    base: Option<String>,
    head: Option<String>,
    nixpkgs: Option<PathBuf>,
    system: Vec<String>,
    no_build: bool,
) -> Result<()> {
    let repo = resolve_repo(nixpkgs)?;
    let (base, head) = resolve_base_head(&repo, base, head)?;
    let systems = resolve_systems(system);

    let (base_evals, head_evals) =
        eval::eval_two(&repo, &base, &head, &systems, eval::DEFAULT_PROFILE, &[])?;

    // The changed set per system, computed once and reused for both the build and
    // the render so they can never disagree.
    let mut per_system_diff: Vec<(String, Vec<diff::DiffEntry>)> = Vec::new();
    for sys in &systems {
        let b = attrs_for(&base_evals, sys);
        let h = attrs_for(&head_evals, sys);
        let changed: Vec<diff::DiffEntry> = diff::diff_evals(&b, &h, None)
            .into_iter()
            .filter(|e| e.kind != DiffKind::Unchanged)
            .collect();
        per_system_diff.push((sys.clone(), changed));
    }

    // Build both sides of the changed set (skipping anything already known or
    // substitutable) so the report has a real verdict for every row, not a `?`.
    if !no_build {
        let mut targets = Vec::new();
        let mut seen = HashSet::new();
        for (sys, changed) in &per_system_diff {
            for e in changed {
                for drv in [&e.base_drv, &e.head_drv].into_iter().flatten() {
                    if seen.insert((drv.clone(), sys.clone())) {
                        targets.push(build::Target {
                            attr: e.attr.clone(),
                            system: sys.clone(),
                            drv_path: drv.clone(),
                        });
                    }
                }
            }
        }
        if !targets.is_empty() {
            build::build_targets(&targets, BuildPolicy::default(), false)?;
        }
    }

    // Render from the (now-populated) log.
    let store = store::Store::open(&eval::db_path()?)?;
    let mut per_system = Vec::new();
    for (sys, changed) in &per_system_diff {
        let mut rows = Vec::new();
        for e in changed {
            let base_obs = match &e.base_drv {
                Some(d) => store.load_observations(d)?,
                None => Vec::new(),
            };
            let head_obs = match &e.head_drv {
                Some(d) => store.load_observations(d)?,
                None => Vec::new(),
            };
            rows.push(report::row_for(e, &base_obs, &head_obs));
        }
        per_system.push((sys.clone(), rows));
    }
    print!("{}", report::render(&base, &head, &per_system));
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Eval {
            commit,
            attrs,
            nixpkgs,
            system,
            profile,
        } => cmd_eval(commit, attrs, nixpkgs, system, profile),
        Command::Diff {
            base,
            head,
            attrs,
            three_way,
            nixpkgs,
            system,
            profile,
        } => cmd_diff(base, head, attrs, three_way, nixpkgs, system, profile),
        Command::Build {
            commit,
            attrs,
            changed,
            dry_run,
            nixpkgs,
            system,
            recheck,
            retry,
            prefer_local,
        } => cmd_build(
            commit,
            attrs,
            changed,
            dry_run,
            nixpkgs,
            system,
            recheck,
            retry,
            prefer_local,
        ),
        Command::Report {
            base,
            head,
            nixpkgs,
            system,
            no_build,
        } => cmd_report(base, head, nixpkgs, system, no_build),
    }
}
