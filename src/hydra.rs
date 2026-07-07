//! Best-effort Hydra facts (DESIGN.md §7), recorded as observations.
//!
//! Two tiers, both keyed to *our exact* derivation:
//!   - **narinfo** on the drv's output paths — drv-precise, success-only. If the
//!     output is in the binary cache, Hydra built exactly this drv → a `Cache`
//!     Built observation.
//!   - **forward job** `/job/<jobset>/<attr>.<system>/latest` — the job's latest
//!     build, which may be a *different* drv (drift). Recorded as a `HydraJob`
//!     observation only when its drvpath matches ours; otherwise reported as
//!     drift and not recorded (that's the false-"regression" trap this avoids).

use std::process::Command;

use serde::Deserialize;

use crate::model::{Observation, Outcome, Source};

const CACHE: &str = "https://cache.nixos.org";
const HYDRA: &str = "https://hydra.nixos.org";
pub const DEFAULT_JOBSET: &str = "nixpkgs/trunk";

/// The 32-char store-path hash component of a `/nix/store/<hash>-name` path.
fn store_hash(path: &str) -> Option<&str> {
    path.rsplit('/').next().and_then(|n| n.split('-').next())
}

/// The realised output paths of a derivation (empty if the .drv isn't present).
fn store_outputs(drv: &str) -> Vec<String> {
    Command::new("nix-store")
        .args(["--query", "--outputs", drv])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Is any of `drv`'s outputs in the binary cache — i.e. substitutable without a
/// local build? Used by the build driver to avoid "building" (really fetching)
/// a cached path and mislabelling it as a local build.
pub fn in_cache(drv: &str) -> bool {
    store_outputs(drv).iter().any(|o| output_in_cache(o))
}

/// Is this exact output path in the binary cache? (narinfo HEAD -> 2xx.)
fn output_in_cache(out_path: &str) -> bool {
    let Some(hash) = store_hash(out_path) else {
        return false;
    };
    // ureq returns Err for 4xx/5xx and transport errors; only 2xx is Ok.
    ureq::head(&format!("{CACHE}/{hash}.narinfo")).call().is_ok()
}

#[derive(Deserialize)]
struct LatestBuild {
    drvpath: Option<String>,
    buildstatus: Option<i64>,
    finished: Option<i64>,
    id: Option<i64>,
}

struct JobStatus {
    drvpath: Option<String>,
    finished: bool,
    succeeded: bool,
    build_id: Option<u64>,
}

fn latest_build(jobset: &str, attr: &str, system: &str) -> Option<JobStatus> {
    let url = format!("{HYDRA}/job/{jobset}/{attr}.{system}/latest");
    let body = ureq::get(&url)
        .set("Accept", "application/json")
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let b: LatestBuild = serde_json::from_str(&body).ok()?;
    Some(JobStatus {
        drvpath: b.drvpath,
        finished: b.finished == Some(1),
        succeeded: b.buildstatus == Some(0),
        build_id: b.id.map(|i| i as u64),
    })
}

/// The Hydra facts gathered for one target, ready to record + report.
pub struct HydraResult {
    pub observations: Vec<Observation>,
    pub in_cache: bool,
    /// The forward job's latest build is a *different* drv than ours.
    pub drift: bool,
    /// A short human description of the forward-job verdict, for printing.
    pub job: Option<String>,
}

/// Query Hydra about one (attr, drv, system) and produce observations to record.
///
/// Always records one `Cache` and one `HydraJob` observation — `Built`/`Failed`
/// on a hit, `NotAttempted` on a miss — so the log remembers *that we probed*
/// this drv (letting a re-run skip it). `NotAttempted` doesn't affect verdicts.
pub fn observe(attr: &str, drv: &str, system: &str, jobset: &str, now: i64) -> HydraResult {
    let obs = |source, outcome, build_id| Observation {
        drv_path: drv.to_string(),
        source,
        outcome,
        when: now,
        system: Some(system.to_string()),
        duration_s: None,
        cached: None,
        machine: None,
        log_ref: None,
        build_id,
    };
    let mut observations = Vec::new();

    // Tier 1: narinfo — drv-precise "Hydra built exactly this output".
    let in_cache = store_outputs(drv).iter().any(|o| output_in_cache(o));
    observations.push(obs(
        Source::Cache,
        if in_cache { Outcome::Built } else { Outcome::NotAttempted },
        None,
    ));

    // Tier 2: forward job — a fact only if it's our exact drv (else it's drift).
    let mut drift = false;
    let (job, hydra) = match latest_build(jobset, attr, system) {
        Some(js) if js.finished && js.drvpath.as_deref() == Some(drv) => {
            let outcome = if js.succeeded { Outcome::Built } else { Outcome::Failed };
            (Some(format!("{outcome:?} (#{})", js.build_id.unwrap_or(0))), obs(Source::HydraJob, outcome, js.build_id))
        }
        Some(js) if js.finished => {
            drift = true;
            (Some("drift (different drv)".to_string()), obs(Source::HydraJob, Outcome::NotAttempted, None))
        }
        _ => (None, obs(Source::HydraJob, Outcome::NotAttempted, None)),
    };
    observations.push(hydra);

    HydraResult {
        observations,
        in_cache,
        drift,
        job,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_hash_of_output() {
        assert_eq!(
            store_hash("/nix/store/qpp9968dpkv1c755nk13mrkrzpsvah18-hello-2.12.3"),
            Some("qpp9968dpkv1c755nk13mrkrzpsvah18")
        );
    }
}
