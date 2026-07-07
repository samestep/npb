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
use crate::hydra;
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
/// it survives GC. Uses `nom` (nix-output-monitor, a `nix build` drop-in) so the
/// user gets its live build tree; output is inherited and the full build log
/// stays retrievable via `nix log <drv>`. Returns success and elapsed time.
fn run_build(drv: &str, cache: &Path, force: bool) -> Result<(bool, f64)> {
    let gcroot = cache.join("gcroots").join(store_hash(drv));
    fs::create_dir_all(gcroot.parent().unwrap()).context("creating gcroots dir")?;

    let installable = format!("{drv}^*");
    let mut cmd = Command::new("nom");
    cmd.args(["build", &installable, "--out-link"])
        .arg(&gcroot)
        .args(["--extra-experimental-features", "nix-command"]);
    if force {
        // --recheck / --prefer-local: build from source even if the output is
        // already valid or substitutable, so the result is a genuine local build.
        cmd.arg("--rebuild");
    }
    let start = Instant::now();
    let status = cmd.status().context("running nom build (nix-output-monitor)")?;
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

    // --recheck / --prefer-local force a genuine local build; otherwise a
    // cached (substitutable) output means we needn't build at all.
    let force = policy.recheck || policy.prefer_local;
    let n = targets.len();
    let mut results = Vec::new();
    for (i, t) in targets.iter().enumerate() {
        let observations = store.load_observations(&t.drv_path)?;
        // Only probe the cache when it could change the decision (not when forcing).
        let substitutable = !force && hydra::in_cache(&t.drv_path);
        let decision = policy.decide(&observations, substitutable);
        let progress = format!("[{}/{n}] {} {}", i + 1, t.system, t.attr);
        let outcome = match decision {
            Decision::Build if !dry_run => {
                // Header before nom build inherits the terminal for its progress.
                println!("{progress}: building…");
                let now = chrono::Utc::now().timestamp();
                let (ok, secs) = run_build(&t.drv_path, &cache, force)?;
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
                let has_local_built = observations
                    .iter()
                    .any(|o| o.source == Source::Local && o.outcome == Outcome::Built);
                if substitutable && !has_local_built {
                    // It's in the cache, not built here — record that as a Cache
                    // fact (deduped) so the report shows `C`, never a bogus `L`.
                    let known_cache = observations
                        .iter()
                        .any(|o| o.source == Source::Cache && o.outcome == Outcome::Built);
                    if !known_cache {
                        store.add_observation(&Observation {
                            drv_path: t.drv_path.clone(),
                            source: Source::Cache,
                            outcome: Outcome::Built,
                            when: chrono::Utc::now().timestamp(),
                            system: Some(t.system.clone()),
                            duration_s: None,
                            cached: Some(true),
                            machine: None,
                            log_ref: None,
                            build_id: None,
                        })?;
                    }
                    println!("{progress}: skip (in binary cache)");
                } else {
                    println!("{progress}: skip (known ok)");
                }
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
