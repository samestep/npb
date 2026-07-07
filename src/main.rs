//! npd — a persistent fact store for iterating on nixpkgs changes.
//!
//! See DESIGN.md for the architecture. The pure data model lives in [`model`];
//! orchestration (eval / diff / build / hydra / report) is being built
//! spine-first, and unimplemented subcommands fail loudly rather than pretending.

// Scaffolding: some model types are defined ahead of the orchestration that will
// consume them (see DESIGN.md build order). Drop this once build/report land.
#![allow(dead_code)]

mod diff;
mod eval;
mod model;

use std::path::PathBuf;
use std::process::Command as Proc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::diff::{Attribution, DiffKind};
use crate::model::Existence;

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
        /// nixpkgs repo to resolve the commit in (default: `$NPD_NIXPKGS`).
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
        /// nixpkgs repo to resolve the commits in (default: `$NPD_NIXPKGS`).
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
        attrs: Vec<String>,
        #[arg(long)]
        recheck: bool,
        #[arg(long)]
        retry: bool,
        #[arg(long)]
        prefer_local: bool,
    },
    /// Fetch facts from Hydra on demand and record them as observations.
    Hydra { attrs: Vec<String> },
    /// Render a Markdown report from stored facts.
    Report,
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

fn resolve_repo(nixpkgs: Option<PathBuf>) -> Result<PathBuf> {
    nixpkgs
        .or_else(|| std::env::var_os("NPD_NIXPKGS").map(PathBuf::from))
        .context("no nixpkgs repo: pass --nixpkgs <path> or set $NPD_NIXPKGS")
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

fn cmd_eval(
    commit: String,
    attrs: Vec<String>,
    nixpkgs: Option<PathBuf>,
    system: Vec<String>,
    profile: Option<String>,
) -> Result<()> {
    let repo = resolve_repo(nixpkgs)?;
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
    let systems = resolve_systems(system);
    let profile = profile.unwrap_or_else(|| eval::DEFAULT_PROFILE.to_string());

    let merge_base = if three_way {
        Some(git_merge_base(&repo, &base, &head)?)
    } else {
        None
    };

    let base_evals = eval::eval_commit(&repo, &base, &systems, &profile, &attrs)?;
    let head_evals = eval::eval_commit(&repo, &head, &systems, &profile, &attrs)?;
    let mb_evals = match &merge_base {
        Some(c) => Some(eval::eval_commit(&repo, c, &systems, &profile, &attrs)?),
        None => None,
    };
    let attrs_for = |evals: &[eval::Eval], sys: &str| -> Vec<model::AttrEval> {
        evals
            .iter()
            .find(|e| e.system == sys)
            .map(|e| e.attrs.clone())
            .unwrap_or_default()
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
        Command::Build { .. } => bail!("npd build: not implemented yet (see DESIGN.md build order)"),
        Command::Hydra { .. } => bail!("npd hydra: not implemented yet (see DESIGN.md build order)"),
        Command::Report => bail!("npd report: not implemented yet (see DESIGN.md build order)"),
    }
}
