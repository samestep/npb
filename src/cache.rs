//! Substituter (binary cache) facts: is a derivation's output already built and
//! available from `cache.nixos.org`? This is the one remote source npb still
//! consults — a drv-precise, success-only signal (a narinfo either exists or it
//! doesn't). Recorded as `Cache` observations so a later run needn't re-probe.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const CACHE: &str = "https://cache.nixos.org";

/// How many narinfo probes to run at once. These are independent HTTP HEADs
/// against a CDN; with a pooled (keep-alive) agent, throughput climbs to ~64
/// concurrent connections and plateaus there (measured) — beyond that the extra
/// sockets cost more than they buy.
const PROBE_CONCURRENCY: usize = 64;

/// Drvs per `nix derivation show` invocation — keeps the argv well under
/// `ARG_MAX` on a huge changed set while still collapsing thousands of drvs
/// into a handful of subprocess spawns.
const DERIVATION_SHOW_CHUNK: usize = 4096;

/// The 32-char store-path hash component of a `/nix/store/<hash>-name` path.
fn store_hash(path: &str) -> Option<&str> {
    path.rsplit('/').next().and_then(|n| n.split('-').next())
}

/// The realised output paths of a derivation, via `nix-store --query
/// --outputs` (fails if the .drv isn't in the local store). The one such
/// helper — the build driver's validity checks use it too.
pub fn drv_outputs(drv: &str) -> Result<Vec<String>> {
    let out = Command::new("nix-store")
        .args(["--query", "--outputs", drv])
        .output()
        .context("running nix-store --query --outputs")?;
    if !out.status.success() {
        bail!("nix-store --query --outputs {drv} failed");
    }
    Ok(lines(&out.stdout))
}

/// Non-empty trimmed lines of a command's output.
pub fn lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Is this exact output path in the binary cache? (narinfo HEAD -> 2xx.) Uses a
/// shared, pooled `Agent` so the TCP+TLS connection is reused across the
/// thousands of probes rather than re-handshaken per request — the difference
/// (measured ~7×) between the probe taking a minute and taking seconds.
fn output_in_cache(agent: &ureq::Agent, out_path: &str) -> bool {
    let Some(hash) = store_hash(out_path) else {
        return false;
    };
    // ureq returns Err for 4xx/5xx and transport errors; only 2xx is Ok.
    agent
        .head(&format!("{CACHE}/{hash}.narinfo"))
        .call()
        .is_ok()
}

// --- `nix derivation show` JSON (only the output paths) ----------------------

#[derive(Deserialize)]
struct DrvShow {
    derivations: HashMap<String, DrvInfo>,
}
#[derive(Deserialize)]
struct DrvInfo {
    outputs: HashMap<String, OutInfo>,
}
#[derive(Deserialize)]
struct OutInfo {
    /// Absent for a floating content-addressed output whose path isn't yet
    /// known — such a drv can't be narinfo-probed, so we treat it as not
    /// substitutable (build it).
    path: Option<String>,
}

/// Resolve `drv -> its output store paths` for many drvs in a few subprocess
/// spawns via `nix derivation show`, instead of one `nix-store --query` per drv.
/// Only drvs whose `.drv` is present locally are queried: `nix derivation show`
/// fails the *whole* invocation on a missing path, and an un-instantiated drv
/// isn't substitutable anyway (the driver just builds it), so it's simply
/// omitted from the result. Batched this is ~10× the per-drv query (measured),
/// and concurrency can't close that gap — the spawns are process-bound, not
/// I/O-bound.
fn drv_outputs_many(drvs: &[String]) -> HashMap<String, Vec<String>> {
    // Filter to `.drv`s that actually exist (a cheap stat, no subprocess), and
    // index by basename so the JSON's keys (which drop the store prefix) map
    // back to the full drv path the caller keys on.
    let mut by_base: HashMap<&str, &String> = HashMap::new();
    let existing: Vec<&String> = drvs
        .iter()
        .filter(|d| Path::new(d).exists())
        .inspect(|d| {
            if let Some(b) = d.rsplit('/').next() {
                by_base.insert(b, d);
            }
        })
        .collect();

    let mut map = HashMap::new();
    for chunk in existing.chunks(DERIVATION_SHOW_CHUNK) {
        let out = Command::new("nix")
            .args([
                "derivation",
                "show",
                "--extra-experimental-features",
                "nix-command",
            ])
            .args(chunk.iter().map(|s| s.as_str()))
            .output();
        let Ok(out) = out else { continue };
        if !out.status.success() {
            continue;
        }
        let Ok(show) = serde_json::from_slice::<DrvShow>(&out.stdout) else {
            continue;
        };
        for (key, info) in show.derivations {
            // Tolerate the key being either the bare basename or a full path.
            let base = key.rsplit('/').next().unwrap_or(&key);
            let Some(&full) = by_base.get(base) else {
                continue;
            };
            // `None` if any output lacks a path (floating CA): can't probe it,
            // so omit the drv → not substitutable.
            let outs: Option<Vec<String>> = info.outputs.values().map(|o| o.path.clone()).collect();
            if let Some(outs) = outs {
                map.insert(full.clone(), outs);
            }
        }
    }
    map
}

/// Probe several drvs at once, returning `drv -> substitutable?`. First resolves
/// every drv's outputs in a couple of batched `nix derivation show` calls
/// ([`drv_outputs_many`]), then a shared cursor hands each of
/// [`PROBE_CONCURRENCY`] workers the next drv, all sharing one pooled agent so
/// the wall time is `ceil(n / workers)` *reused-connection* round-trips.
///
/// A drv is substitutable iff ALL its outputs are in the cache — the recorded
/// `Cache`/`Built` fact stands for the whole drv, and one missing output of a
/// multi-output drv (a partial upload, an evicting substituter) would still
/// force a local build. A drv absent from the resolved map (not in the store,
/// or floating CA) probes as not-substitutable: the safe direction.
///
/// `progress(1)` is called as each drv resolves, so a caller can render progress
/// for this otherwise-silent (and, on a first run over a big changed set,
/// minute-long) network phase.
pub fn in_cache_many(drvs: &[String], progress: &(dyn Fn(usize) + Sync)) -> HashMap<String, bool> {
    if drvs.is_empty() {
        return HashMap::new();
    }
    let outputs = drv_outputs_many(drvs);
    let agent = ureq::AgentBuilder::new()
        .max_idle_connections(PROBE_CONCURRENCY)
        .max_idle_connections_per_host(PROBE_CONCURRENCY)
        .timeout(Duration::from_secs(15))
        .build();
    let cursor = AtomicUsize::new(0);
    let workers = PROBE_CONCURRENCY.min(drvs.len());
    let results: Vec<(String, bool)> = thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let (agent, outputs, cursor) = (&agent, &outputs, &cursor);
                s.spawn(move || {
                    let mut local = Vec::new();
                    loop {
                        let i = cursor.fetch_add(1, Ordering::Relaxed);
                        let Some(drv) = drvs.get(i) else { break };
                        let sub = outputs.get(drv).is_some_and(|outs| {
                            !outs.is_empty() && outs.iter().all(|o| output_in_cache(agent, o))
                        });
                        local.push((drv.clone(), sub));
                        progress(1);
                    }
                    local
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    results.into_iter().collect()
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
