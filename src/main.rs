//! npd — a persistent fact store for iterating on nixpkgs changes.
//!
//! See DESIGN.md for the architecture. The pure data model lives in [`model`];
//! orchestration (eval / diff / build / hydra / report) is being built
//! spine-first, and unimplemented subcommands fail loudly rather than pretending.

// Scaffolding: the model types are defined ahead of the orchestration that will
// consume them (see DESIGN.md build order). Drop this once eval/build land.
#![allow(dead_code)]

mod model;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

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
        /// Systems to evaluate for (repeatable); defaults to the host system.
        #[arg(long)]
        system: Vec<String>,
    },
    /// Diff two revisions into a set of changed attrs (optionally three-way via merge base).
    Diff {
        base: String,
        head: String,
        /// Also evaluate the merge base to attribute each change (git-3-way style).
        #[arg(long)]
        three_way: bool,
    },
    /// Build derivations, consulting (and appending to) the observation log.
    Build {
        /// Attribute paths to build.
        attrs: Vec<String>,
        /// Rebuild even a previously-succeeded drv (suspect a flaky success).
        #[arg(long)]
        recheck: bool,
        /// Re-attempt a previously-failed drv (expect it might pass now).
        #[arg(long)]
        retry: bool,
        /// Ignore Cache/Hydra success; require a genuine local build.
        #[arg(long)]
        prefer_local: bool,
    },
    /// Fetch facts from Hydra on demand and record them as observations.
    Hydra {
        /// Attribute paths (or derivations) to look up.
        attrs: Vec<String>,
    },
    /// Render a Markdown report from stored facts.
    Report,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let name = match cli.command {
        Command::Eval { .. } => "eval",
        Command::Diff { .. } => "diff",
        Command::Build { .. } => "build",
        Command::Hydra { .. } => "hydra",
        Command::Report => "report",
    };
    bail!("npd {name}: not implemented yet (see DESIGN.md build order)")
}
