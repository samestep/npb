//! The observation-backed build driver (DESIGN.md §5): evaluate the target
//! attrs, and for each buildable derivation consult [`BuildPolicy`] against the
//! stored observations to decide whether to build, skip a known success, or
//! skip a known failure. Every build we run is recorded as an `Observation`.
//!
//! This is the first writer to the observation log, and the reason it exists:
//! Nix remembers successful builds (the store), but *forgets failures* — so
//! without this, a known-failing derivation gets retried on every run.

use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result};

use crate::eval::{self, DEFAULT_PROFILE};
use crate::model::{BuildPolicy, Decision, Observation, Outcome, Source};
use crate::store::Store;

/// What happened to one (attr, system) target.
pub struct Built {
    pub attr: String,
    pub system: String,
    pub drv_path: String,
    pub decision: Decision,
    /// The build outcome, when `decision` was `Build`.
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
/// it survives GC, and capturing build output to a per-drv log. Returns whether
/// it succeeded, how long it took, and the log path (relative to the cache root).
fn run_build(drv: &str, cache: &Path, when: i64) -> Result<(bool, f64, String)> {
    let hash = store_hash(drv);
    let log_rel = format!("logs/{hash}/build-{when}.log");
    let log_path = cache.join(&log_rel);
    fs::create_dir_all(log_path.parent().unwrap()).context("creating build log dir")?;
    let log = File::create(&log_path).context("creating build log")?;

    let gcroot = cache.join("gcroots").join(hash);
    fs::create_dir_all(gcroot.parent().unwrap()).context("creating gcroots dir")?;

    let start = Instant::now();
    let status = Command::new("nix")
        .args(["build", &format!("{drv}^*"), "--out-link"])
        .arg(&gcroot)
        .args(["--extra-experimental-features", "nix-command"])
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .status()
        .context("running nix build")?;
    Ok((status.success(), start.elapsed().as_secs_f64(), log_rel))
}

/// Evaluate `commit` for the given `scope`, then build each buildable derivation
/// subject to `policy` and the observation log.
pub fn build_commit(
    repo: &Path,
    commit: &str,
    systems: &[String],
    scope: &[String],
    policy: BuildPolicy,
) -> Result<Vec<Built>> {
    let mut store = Store::open(&eval::db_path()?)?;
    let cache = eval::cache_root()?;
    let host = hostname();

    let evals = eval::eval_commit(repo, commit, systems, DEFAULT_PROFILE, scope)?;
    let mut results = Vec::new();
    for e in &evals {
        for a in &e.attrs {
            // Anything with a drv can be built — including meta-blocked packages,
            // whose drv exists (we eval with the allow-flags on) and builds fine
            // since meta is only an eval-time gate. Skip eval errors (no drv).
            let Some(drv) = a.drv_path.clone() else {
                continue;
            };

            let observations = store.load_observations(&drv)?;
            let decision = policy.decide(&observations, false);
            let outcome = match decision {
                Decision::Build => {
                    let now = chrono::Utc::now().timestamp();
                    let (ok, secs, log_ref) = run_build(&drv, &cache, now)?;
                    let outcome = if ok { Outcome::Built } else { Outcome::Failed };
                    store.add_observation(&Observation {
                        drv_path: drv.clone(),
                        source: Source::Local,
                        outcome,
                        when: now,
                        system: Some(e.system.clone()),
                        duration_s: Some(secs),
                        cached: None,
                        machine: Some(host.clone()),
                        log_ref: Some(log_ref),
                        build_id: None,
                    })?;
                    Some(outcome)
                }
                Decision::SkipOk | Decision::SkipFail => None,
            };
            results.push(Built {
                attr: a.attr.clone(),
                system: e.system.clone(),
                drv_path: drv,
                decision,
                outcome,
            });
        }
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
