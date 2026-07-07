//! The observation-backed build driver (DESIGN.md §5): evaluate the target
//! attrs, and for each buildable derivation consult [`BuildPolicy`] against the
//! stored observations to decide whether to build, skip a known success, or
//! skip a known failure. Every build we run is recorded as an `Observation`.
//!
//! This is the first writer to the observation log, and the reason it exists:
//! Nix remembers successful builds (the store), but *forgets failures* — so
//! without this, a known-failing derivation gets retried on every run.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result};

use crate::eval;
use crate::model::{BuildPolicy, Decision, Observation, Outcome, Source};
use crate::store::Store;

/// One derivation to consider building, with the attr/system it came from (for
/// reporting). Produced from either an explicit eval or a diff's changed set.
pub struct Target {
    pub attr: String,
    pub system: String,
    pub drv_path: String,
}

/// What happened to one target.
pub struct Built {
    pub attr: String,
    pub system: String,
    pub drv_path: String,
    pub decision: Decision,
    /// The build outcome, when `decision` was `Build` and this was not a dry run.
    pub outcome: Option<Outcome>,
}

/// The 32-char store-path hash component of a `/nix/store/<hash>-name[.drv]` path.
fn store_hash(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .and_then(|n| n.split('-').next())
        .unwrap_or(path)
}

fn hostname() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Realise `drv` (all outputs), rooting the result under the cache's gcroots so
/// it survives GC. `nix build`'s output is inherited so the user sees its native
/// progress; the full build log stays retrievable via `nix log <drv>`. Returns
/// whether it succeeded and how long it took.
fn run_build(drv: &str, cache: &Path) -> Result<(bool, f64)> {
    let gcroot = cache.join("gcroots").join(store_hash(drv));
    fs::create_dir_all(gcroot.parent().unwrap()).context("creating gcroots dir")?;

    let start = Instant::now();
    let status = Command::new("nix")
        .args(["build", &format!("{drv}^*"), "--out-link"])
        .arg(&gcroot)
        .args(["--extra-experimental-features", "nix-command"])
        .status()
        .context("running nix build")?;
    Ok((status.success(), start.elapsed().as_secs_f64()))
}

/// For each target, consult `policy` against the observation log and either
/// build it (recording the result and rooting the output) or skip it. With
/// `dry_run`, decisions are computed but nothing is built or recorded.
pub fn build_targets(
    targets: &[Target],
    policy: BuildPolicy,
    dry_run: bool,
) -> Result<Vec<Built>> {
    let mut store = Store::open(&eval::db_path()?)?;
    let cache = eval::cache_root()?;
    let host = hostname();

    let n = targets.len();
    let mut results = Vec::new();
    for (i, t) in targets.iter().enumerate() {
        let observations = store.load_observations(&t.drv_path)?;
        // `substitutable` is left false for now: on a drv's first encounter we
        // let `nix build` no-op if the output is already valid. The observation
        // log skips it thereafter. (A batch validity/narinfo pre-check to skip
        // even the first nix invocation is a later optimization.)
        let decision = policy.decide(&observations, false);
        let progress = format!("[{}/{n}] {} {}", i + 1, t.system, t.attr);
        let outcome = match decision {
            Decision::Build if !dry_run => {
                // Header before nix build inherits the terminal for its progress.
                println!("{progress}: building…");
                let now = chrono::Utc::now().timestamp();
                let (ok, secs) = run_build(&t.drv_path, &cache)?;
                let outcome = if ok { Outcome::Built } else { Outcome::Failed };
                store.add_observation(&Observation {
                    drv_path: t.drv_path.clone(),
                    source: Source::Local,
                    outcome,
                    when: now,
                    system: Some(t.system.clone()),
                    duration_s: Some(secs),
                    cached: None,
                    machine: Some(host.clone()),
                    log_ref: None,
                    build_id: None,
                })?;
                println!(
                    "{progress}: {} ({secs:.0}s)",
                    if ok { "built" } else { "FAILED" }
                );
                Some(outcome)
            }
            Decision::Build => {
                println!("{progress}: would build");
                None
            }
            Decision::SkipOk => {
                println!("{progress}: skip (known ok)");
                None
            }
            Decision::SkipFail => {
                println!("{progress}: skip (known failure)");
                None
            }
        };
        results.push(Built {
            attr: t.attr.clone(),
            system: t.system.clone(),
            drv_path: t.drv_path.clone(),
            decision,
            outcome,
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_hash_extracts_the_hash() {
        assert_eq!(
            store_hash("/nix/store/izk77azi9bcldnpdw4c62hc637q8xm27-hello-2.12.3.drv"),
            "izk77azi9bcldnpdw4c62hc637q8xm27"
        );
        assert_eq!(
            store_hash("/nix/store/qpp9968dpkv1c755nk13mrkrzpsvah18-hello-2.12.3"),
            "qpp9968dpkv1c755nk13mrkrzpsvah18"
        );
    }
}
