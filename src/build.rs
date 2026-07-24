//! The observation-backed build driver (DESIGN.md §5): decide per target
//! (build / skip-known-ok / skip-cached / skip-known-failure), then build the
//! whole build set in ONE `nom build` invocation. Each drv's outcome is
//! recorded the moment its build activity stops — so an interrupted (^C) batch
//! keeps every fact observed so far — and drvs nix never attempted are
//! attributed from a post-batch output-validity check that records only
//! ground-truth outcomes (`Built`, or a `DepFailed` naming a verified culprit),
//! never an inference gated on nix's ambiguous exit status.
//!
//! The observation log exists because Nix remembers successful builds (the
//! store) but *forgets failures* — without it, a known-failing derivation
//! gets retried on every run. We record a failure for *any* drv we watch fail,
//! a transitive dependency included; a target whose build closure contains a
//! *still-failing* dependency is then skipped before building (it would only
//! cascade to `DepFailed`), which is how a re-run recovers a dependency failure
//! that a ^C dropped before the post-batch step could attribute its dependents.
//! A recorded failure is never trusted blindly, though: it is re-verified
//! against the store before it can block anything (`verify_failing`), and a
//! `DepFailed` carries its culprit's outputs (`Observation::blocker`) so a
//! since-healed dependency un-blocks its dependents automatically, no `--retry`.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::cache;
use crate::live;
use crate::model::{BuildPolicy, Decision, Observation, Outcome};
use crate::store::Store;

/// Store paths per `nix-store` invocation — keeps the argv well under `ARG_MAX`
/// on a huge changed set (a staging-next mass rebuild would otherwise fail with
/// `Argument list too long`), while still collapsing thousands of paths into a
/// handful of subprocess spawns. Mirrors `cache::DERIVATION_SHOW_CHUNK`.
const NIX_STORE_ARG_CHUNK: usize = 4096;

/// One derivation to consider building. Produced from either an explicit eval
/// or a diff's changed set. A meta-blocked package never becomes a target: it
/// threw during eval under the profile, so it has no drv (DESIGN §6).
pub struct Target {
    pub drv_path: String,
}

/// nix internal-json log event (only the fields we use).
#[derive(Deserialize)]
struct NixEvent {
    action: String,
    id: Option<u64>,
    #[serde(rename = "type")]
    typ: Option<i64>,
    #[serde(default)]
    fields: Vec<serde_json::Value>,
}

/// The `actBuild` activity type in nix's internal-json log.
const ACT_BUILD: i64 = 105;

/// Width the plain build log's event-kind column is left-padded to, so the store
/// paths that follow line up. The verbs are a fixed set — `building` (8, the
/// widest), `built`, `failed` — so a constant suffices; the log streams events
/// one at a time and can't compute a width from the batch.
const VERB_W: usize = 8;

/// Build all of `drvs` (all outputs) in ONE nix invocation — nix schedules them
/// together with its own parallelism — while acting as a middleman: nix emits
/// `--log-format internal-json`, which we always parse for build (`type:105`)
/// start/stop events, and — when colorizing (a TTY, `NO_COLOR` unset) — also
/// forward to `nom --json` for the live tree. Off a TTY or under `NO_COLOR`
/// (nom honors neither), nom is skipped and we render a plain append-only
/// `building`/`built`/`failed` log ourselves from the same events — two columns,
/// the event kind then the full `.drv` store path. `--keep-going` so every drv
/// is attempted.
///
/// `on_finish(drv)` fires as *every* build activity stops — the requested
/// drvs and their transitive dependencies alike (the caller records a
/// dependency only when it failed; DESIGN.md §5). Nix registers a successful
/// build's outputs *before* emitting the stop event (both the local and
/// build-hook goals `registerValidPaths` before destroying the `actBuild`
/// Activity — nix 2.34 `derivation-building-goal.cc`), so the callback can
/// attribute the outcome from output validity right away. (Nix keeps the build
/// log itself under `/nix/var/log/nix/drvs`; `nix log <drv>` retrieves it, so
/// npb doesn't duplicate it.)
///
/// nix's exit status is deliberately *not* returned: npb records only outcomes
/// it can ground in store validity — a drv's own stop event (here) or, for the
/// drvs that got no build activity, the post-batch output-validity check in
/// [`build_targets_at`] — never an inference gated on the ambiguous exit code
/// (DESIGN.md §5).
fn batch_build(drvs: &[&str], mut on_finish: impl FnMut(&str) -> Result<()>) -> Result<()> {
    let installables: Vec<String> = drvs.iter().map(|d| format!("{d}^*")).collect();
    let mut nix = Command::new("nix");
    nix.arg("build").args(&installables).args([
        "--keep-going",
        // No ./result* out-links: they'd litter the cwd (the user's nixpkgs
        // checkout) and pin every built output as a GC root — npb keeps no
        // gcroots by design (DESIGN §4); the *observation* is the durable fact.
        "--no-link",
        "--log-format",
        "internal-json",
        "-v",
        "--extra-experimental-features",
        "nix-command",
    ]);
    let mut nix = nix
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning nix build")?;
    // nom renders the colored, redrawn build tree — but only when we'd colorize
    // at all (it honors neither NO_COLOR (#129) nor a non-TTY). Otherwise we parse
    // the same internal-json stream (unchanged — that's what records outcomes and
    // keeps ^C-safety) and render a plain append-only log ourselves.
    let mut nom = if live::colors_enabled() {
        Some(
            Command::new("nom")
                .arg("--json")
                .stdin(Stdio::piped())
                .spawn()
                .context("spawning nom --json (nix-output-monitor)")?,
        )
    } else {
        None
    };
    let mut nom_in = nom
        .as_mut()
        .map(|n| n.stdin.take().expect("stdin is piped"));

    let log = BufReader::new(nix.stderr.take().expect("stderr is piped"));
    let mut starts: HashMap<u64, String> = HashMap::new();

    let streamed = (|| -> Result<()> {
        for line in log.lines() {
            let line = line.context("reading nix build log")?;
            if let Some(nom_in) = nom_in.as_mut() {
                // Forward the raw internal-json line to nom, which renders the tree.
                let _ = writeln!(nom_in, "{line}");
            }
            let Some(rest) = line.strip_prefix("@nix ") else {
                continue;
            };
            let Ok(ev) = serde_json::from_str::<NixEvent>(rest) else {
                continue;
            };
            match ev.action.as_str() {
                "start" if ev.typ == Some(ACT_BUILD) => {
                    if let (Some(id), Some(drv)) =
                        (ev.id, ev.fields.first().and_then(|v| v.as_str()))
                    {
                        if nom_in.is_none() {
                            eprintln!("{:<VERB_W$}  {drv}", "building");
                        }
                        starts.insert(id, drv.to_string());
                    }
                }
                "stop" => {
                    if let Some(id) = ev.id
                        && let Some(drv) = starts.remove(&id)
                    {
                        if nom_in.is_none() {
                            // Nix registers a success's outputs *before* this stop
                            // event, so output validity right now is the build's
                            // own result — the same signal `on_finish` records.
                            let verb = if drv_built(&drv).unwrap_or(false) {
                                "built"
                            } else {
                                "failed"
                            };
                            eprintln!("{verb:<VERB_W$}  {drv}");
                        }
                        on_finish(&drv)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();
    drop(nom_in); // EOF -> nom (if any) finishes rendering and exits
    if streamed.is_err() {
        // An on_finish (store) error abandons the stream mid-batch; a Child is
        // not killed on drop, so without this nix keeps building into a closed
        // pipe until its next stderr write EPIPEs it — potentially minutes.
        let _ = nix.kill();
    }
    // Reap nix (and nom) regardless. A build failing is normal (npb records the
    // per-drv outcome above), so the exit status is intentionally discarded.
    let _ = nix.wait().context("waiting for nix build")?;
    if let Some(mut nom) = nom {
        let _ = nom.wait();
    }
    streamed?;
    Ok(())
}

/// Which of `paths` are NOT valid in the local store (i.e. weren't built).
fn invalid_paths(paths: &[String]) -> Result<HashSet<String>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    // Prints the invalid subset; exits non-zero when some are invalid, which is
    // expected — parse stdout regardless. Chunked to stay under ARG_MAX; validity
    // is per-path independent, so the invalid set is the union over the chunks.
    let mut invalid = HashSet::new();
    for chunk in paths.chunks(NIX_STORE_ARG_CHUNK) {
        let out = Command::new("nix-store")
            .args(["--check-validity", "--print-invalid"])
            .args(chunk)
            .output()
            .context("running nix-store --check-validity")?;
        invalid.extend(cache::lines(&out.stdout));
    }
    Ok(invalid)
}

/// The build closure of `drvs` as a set of store paths — every input `.drv`
/// (and source) nix would need to realise them, transitively (`nix-store
/// --query --requisites`). Used to propagate a known failure forward
/// (DESIGN.md §5): if a target's closure contains a drv recorded as failing,
/// building the target would only `DepFail`, so it can be skipped.
fn drv_closure(drvs: &[&str]) -> Result<HashSet<String>> {
    if drvs.is_empty() {
        return Ok(HashSet::new());
    }
    // Chunked to stay under ARG_MAX; the requisites of a set of drvs is the
    // union of each drv's requisites, so unioning the per-chunk output is exact.
    let mut reqs = HashSet::new();
    for chunk in drvs.chunks(NIX_STORE_ARG_CHUNK) {
        let out = Command::new("nix-store")
            .args(["--query", "--requisites"])
            .args(chunk)
            .output()
            .context("running nix-store --query --requisites")?;
        if !out.status.success() {
            anyhow::bail!(
                "nix-store --query --requisites failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        reqs.extend(cache::lines(&out.stdout));
    }
    Ok(reqs)
}

/// Did this drv's build succeed — are all its outputs valid in the local
/// store? Sound at stop-event time; see `batch_build`.
fn drv_built(drv: &str) -> Result<bool> {
    let outs = cache::drv_outputs(drv)?;
    Ok(!outs.is_empty() && invalid_paths(&outs)?.is_empty())
}

/// Map each built drv to whether all its outputs are now valid (i.e. it built).
fn build_outcomes(drvs: &[&str]) -> Result<HashMap<String, bool>> {
    let mut per_drv: Vec<(String, Vec<String>)> = Vec::new();
    let mut all = Vec::new();
    for &d in drvs {
        let outs = cache::drv_outputs(d)?;
        all.extend(outs.iter().cloned());
        per_drv.push((d.to_string(), outs));
    }
    let invalid = invalid_paths(&all)?;
    Ok(per_drv
        .into_iter()
        .map(|(d, outs)| {
            let built = !outs.is_empty() && outs.iter().all(|o| !invalid.contains(o));
            (d, built)
        })
        .collect())
}

/// For each target with a `DepFailed`-only history, whether its recorded block is
/// now **stale** — the culprit dependency's outputs (`Observation::blocker`) are
/// all valid in the store, so it has built or been substituted since (DESIGN.md
/// §5). This is the self-healing check, and it is deliberately **offline**: one
/// `nix-store --check-validity` over the stored blocker paths, needing no `.drv`
/// and no closure walk, so a fully-cached run stays instant. A `DepFailed` with
/// no recorded blocker (its culprit unknown) is treated conservatively as *not*
/// stale — it keeps blocking until `--retry`. The store-validity signal mirrors,
/// for a dependency, what a recorded `Built` already does for a target
/// (`flaky_success_wins`): a later success outranks an earlier failure.
fn stale_dep_blocks(
    targets: &[Target],
    obs_by_drv: &HashMap<String, Vec<Observation>>,
) -> Result<HashMap<String, bool>> {
    let mut all_paths: Vec<String> = Vec::new();
    let mut per_target: Vec<(String, Vec<String>)> = Vec::new();
    for t in targets {
        let obs = obs_by_drv
            .get(&t.drv_path)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        // The most recent dep-block that recorded a culprit is the one in force.
        if let Some(blocker) = obs
            .iter()
            .rev()
            .find(|o| o.outcome == Outcome::DepFailed && !o.blocker.is_empty())
            .map(|o| o.blocker.clone())
        {
            all_paths.extend(blocker.iter().cloned());
            per_target.push((t.drv_path.clone(), blocker));
        }
    }
    if all_paths.is_empty() {
        return Ok(HashMap::new());
    }
    let invalid = invalid_paths(&all_paths)?;
    Ok(per_target
        .into_iter()
        .map(|(drv, blocker)| {
            let stale = blocker.iter().all(|p| !invalid.contains(p));
            (drv, stale)
        })
        .collect())
}

/// What re-checking direct failures against the store turned up (DESIGN §5),
/// both keyed on drvpath.
struct FailureRecheck {
    /// Failures whose outputs are now all valid — record a `Built`, so `decide`
    /// → `SkipOk` and the report shows a success.
    heal: HashSet<String>,
    /// Still-invalid failures whose outputs were freshly resolved from a `.drv`
    /// (they had none recorded): the resolved outputs, to record so the next run
    /// re-checks them offline rather than materializing again (self-limiting).
    record_outputs: Vec<(String, Vec<String>)>,
}

/// The direct-failure analogue of [`stale_dep_blocks`] (DESIGN §5): re-check
/// every direct failure's outputs against the store, so one that built out of
/// band since (a plain `nix build`, an unrelated realisation) heals instead of
/// staying sticky. Each failure's outputs come from its recorded `blocker`
/// (offline, no `.drv`) or — for one [`drvs_to_materialize`] pulled in because
/// it had none recorded — its now-present `.drv` via [`cache::drv_outputs`]. A
/// target with a recorded `Built` is already decided; a failure whose outputs
/// can't be resolved at all (no `blocker`, `.drv` absent) stays sticky,
/// overridden only by `--retry` or a later `Built`.
fn recheck_direct_failures(
    targets: &[Target],
    obs_by_drv: &HashMap<String, Vec<Observation>>,
) -> Result<FailureRecheck> {
    // (drvpath, outputs, from_record): whether the outputs came from the log.
    let mut items: Vec<(String, Vec<String>, bool)> = Vec::new();
    let mut all_paths: Vec<String> = Vec::new();
    for t in targets {
        let obs = obs_by_drv
            .get(&t.drv_path)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        // A recorded success already decides it; don't second-guess the log.
        if obs.iter().any(|o| o.outcome == Outcome::Built) {
            continue;
        }
        if !obs.iter().any(|o| o.outcome == Outcome::Failed) {
            continue;
        }
        let recorded = obs
            .iter()
            .rev()
            .find(|o| o.outcome == Outcome::Failed && !o.blocker.is_empty())
            .map(|o| o.blocker.clone());
        let (outs, from_record) = match recorded {
            Some(b) => (b, true),
            // No recorded outputs: resolve from the `.drv` `drvs_to_materialize`
            // instantiated for exactly this. If it's absent anyway (never
            // materialized, GC'd), `drv_outputs` yields none and we leave it sticky.
            None => (cache::drv_outputs(&t.drv_path).unwrap_or_default(), false),
        };
        if outs.is_empty() {
            continue;
        }
        all_paths.extend(outs.iter().cloned());
        items.push((t.drv_path.clone(), outs, from_record));
    }
    if all_paths.is_empty() {
        return Ok(FailureRecheck {
            heal: HashSet::new(),
            record_outputs: Vec::new(),
        });
    }
    let invalid = invalid_paths(&all_paths)?;
    let mut heal = HashSet::new();
    let mut record_outputs = Vec::new();
    for (drv, outs, from_record) in items {
        if outs.iter().all(|p| !invalid.contains(p)) {
            heal.insert(drv);
        } else if !from_record {
            record_outputs.push((drv, outs));
        }
    }
    Ok(FailureRecheck {
        heal,
        record_outputs,
    })
}

/// From a set of candidate failing drvs (drawn from the observation log), the
/// subset that is **actually still failing** — some output is invalid in the
/// store — mapped to that drv's output paths (its `blocker` if it culprits a
/// dependent). A candidate whose outputs are now all valid has healed since (a
/// rebuild, or an unrelated build substituted it) and is dropped, so a stale
/// failure fact can never re-block. Callers pass candidates drawn from an
/// instantiated target's closure, so each candidate's `.drv` is present and
/// `drv_outputs` resolves.
fn verify_failing(candidates: &HashSet<String>) -> Result<HashMap<String, Vec<String>>> {
    if candidates.is_empty() {
        return Ok(HashMap::new());
    }
    let mut per_drv: Vec<(String, Vec<String>)> = Vec::new();
    let mut all = Vec::new();
    for d in candidates {
        let outs = cache::drv_outputs(d)?;
        all.extend(outs.iter().cloned());
        per_drv.push((d.clone(), outs));
    }
    let invalid = invalid_paths(&all)?;
    Ok(per_drv
        .into_iter()
        .filter(|(_, outs)| outs.is_empty() || outs.iter().any(|o| invalid.contains(o)))
        .collect())
}

/// For each target, consult `policy` against the observation log; then build the
/// whole build set at once.
/// Build the changed set (skipping anything already known / substitutable /
/// meta-blocked). Returns whether it actually ran a build — i.e. produced stderr
/// output — so the caller only fences it with a separator when there's something
/// to fence (an all-cached set is silent).
pub fn build_targets(targets: &[Target], policy: BuildPolicy) -> Result<bool> {
    build_targets_at(&crate::paths::db_path()?, targets, policy)
}

/// The subset of `targets` whose `.drv` the build phase will actually need in
/// the store — the ones it will probe or build — decided from the observation
/// log alone (one query, no `.drv` required). Everything else is already
/// resolved from facts, so materializing it is pure waste; when this is empty (a
/// fully-cached changed set) the caller skips the instantiation eval entirely,
/// which is what keeps a warm re-run instant (DESIGN.md §5–§6).
///
/// This is the *pre-probe* build set: a drv with no fact is a probe candidate,
/// and probing (`nix derivation show`) needs the `.drv` present just as a build
/// does — and a never-observed drv already decides `Build` — so both fold into
/// one predicate here. A drv already known built / substitutable / failing
/// decides `Skip*` and is omitted. Kept in lockstep with `build_targets_at`'s
/// pass 1 by going through the same [`BuildPolicy::decide`].
pub fn drvs_to_materialize(targets: &[Target], policy: BuildPolicy) -> Result<HashSet<String>> {
    drvs_to_materialize_at(&crate::paths::db_path()?, targets, policy)
}

fn drvs_to_materialize_at(
    db: &std::path::Path,
    targets: &[Target],
    policy: BuildPolicy,
) -> Result<HashSet<String>> {
    let store = Store::open(db)?;
    let drv_refs: Vec<&str> = targets.iter().map(|t| t.drv_path.as_str()).collect();
    let obs_by_drv = store.load_observations_many(&drv_refs)?;
    // A stale dep-block re-opens its target (it will be re-attempted), so its
    // `.drv` is needed again — compute staleness the same offline way the build
    // driver does, keeping the two decisions in lockstep.
    let stale = stale_dep_blocks(targets, &obs_by_drv)?;
    let mut need = HashSet::new();
    for t in targets {
        let obs = obs_by_drv
            .get(&t.drv_path)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let dep_stale = stale.get(&t.drv_path).copied().unwrap_or(false);
        let decision = policy.decide(obs, dep_stale);
        if decision == Decision::Build {
            need.insert(t.drv_path.clone());
        } else if decision == Decision::SkipFail && needs_selfheal_instantiate(obs) {
            // A direct failure that recorded no outputs can't be self-heal-checked
            // offline (DESIGN §5), so materialize its `.drv` here and let the build
            // driver resolve the outputs and re-check store validity — the
            // cache-miss half of the self-heal, folded into the one instantiate
            // pass rather than a bespoke query. A failure that *did* record its
            // outputs is checked offline and stays out of the set, which is what
            // keeps a warm run whose failures are all recorded instant.
            need.insert(t.drv_path.clone());
        }
    }
    Ok(need)
}

/// A **direct** failure with no recorded outputs: it can't be self-heal-checked
/// offline (nothing to validate against the store), so its `.drv` must be
/// materialized to resolve them (DESIGN §5). Excludes a `DepFailed` (its block
/// self-heals via [`stale_dep_blocks`]) and a direct failure that already
/// recorded its outputs (checked offline, no `.drv` needed).
fn needs_selfheal_instantiate(obs: &[Observation]) -> bool {
    let direct_failed = obs.iter().any(|o| o.outcome == Outcome::Failed);
    let has_recorded_outputs = obs
        .iter()
        .any(|o| o.outcome == Outcome::Failed && !o.blocker.is_empty());
    direct_failed && !has_recorded_outputs
}

/// Narrow [`drvs_to_materialize`]'s set to the drvs whose `.drv` is **not** already
/// a valid store path — the recipes instantiation must still write. A `.drv` that
/// survived on disk from an earlier run needs no re-materialization: `nix build
/// <drv>^*` and the narinfo probe read it in place, so instantiating it again only
/// re-pays the nixpkgs import for a file already present. This is what lets a
/// re-run of a report with still-to-build rows — an `❔` unbuilt target nix
/// couldn't reach, a never-probed drv — skip that import while **still building**
/// the target: the goal is to drop the instantiate step, not the build (which a
/// present `.drv` lets nix do directly). Since the phase re-imports nixpkgs once
/// per `(commit, system)` (DESIGN §6), a side is skipped entirely only when *all*
/// its recipes are present; one absent drv still pays that side's import, but
/// instantiation is trimmed to just the absent attrs.
///
/// Sound because a drv path is content-addressed: a valid `.drv` at path `H` *is*
/// the recipe hashing to `H`, and the store's closure invariant (a valid path's
/// references are valid) guarantees its input `.drv`s are present too, so the
/// build and probe can walk it. npb keeps no gcroots (DESIGN §4), so a `.drv` may
/// have been reclaimed by `nix-collect-garbage` since — then it is simply
/// re-materialized here, no worse than before. Only called with a non-empty `need`
/// (a fully-cached run skips instantiation from the log alone, never reaching
/// here), so the one extra `nix-store --check-validity` never burdens a warm run.
pub fn drvs_needing_instantiation(need: HashSet<String>) -> Result<HashSet<String>> {
    if need.is_empty() {
        return Ok(need);
    }
    // `invalid_paths` returns the subset that is *not* valid in the store — for a
    // `.drv`, exactly the recipes still absent, which are the ones to instantiate.
    let paths: Vec<String> = need.iter().cloned().collect();
    let absent = invalid_paths(&paths)?;
    Ok(retain_absent(need, &absent))
}

/// Keep only the drvs whose `.drv` is absent (present in `absent`) — the recipes
/// still needing instantiation. Split out from [`drvs_needing_instantiation`] so
/// the keep-the-*absent* polarity is unit-testable without a Nix store.
fn retain_absent(need: HashSet<String>, absent: &HashSet<String>) -> HashSet<String> {
    need.into_iter().filter(|d| absent.contains(d)).collect()
}

/// [`build_targets`] against an explicit observation DB (separable for tests).
/// Probe the substituter for every target the log knows nothing about and
/// record a `Built` observation per hit (DESIGN §7) — the fact-gathering half
/// of the decision phase.
///
/// We only probe drvs with *no fact*: a probe can only change the decision
/// there. A drv with any observation is already decided (a success — its own
/// build or a prior probe's hit — → skip; failed/blocked → skip-fail). This is
/// what keeps a re-run of an unchanged report near-instant: we don't re-probe
/// (HTTP + `nix-store`) the failures every time. Probes run concurrently
/// (`cache::in_cache_many`). Idempotent: recorded facts make the next call a
/// no-op.
fn probe_candidates(store: &Store, targets: &[Target]) -> Result<Vec<String>> {
    let drv_refs: Vec<&str> = targets.iter().map(|t| t.drv_path.as_str()).collect();
    let obs_by_drv = store.load_observations_many(&drv_refs)?;
    let has_fact = |drv: &str| obs_by_drv.get(drv).is_some_and(|obs| !obs.is_empty());
    let mut to_probe: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for t in targets {
        if !has_fact(&t.drv_path) && seen.insert(t.drv_path.clone()) {
            to_probe.push(t.drv_path.clone());
        }
    }
    Ok(to_probe)
}

/// Record one coalesced batch of probe verdicts: its hits become `Built`
/// observations in a single transaction (cache presence and a local build are
/// the same success fact, DESIGN §7); misses record nothing (re-probing is cheap
/// and cache state can change). One transaction per batch — not one autocommit
/// per hit, which is fsync-bound at ~200 rows/s (`Store::record_cache_hits`) —
/// so the probe stays network-bound; and committed batch by batch as verdicts
/// land, not once at the end, so a ^C keeps every committed batch and a restart
/// re-probes only the rest (the probe analogue of the build phase's per-outcome
/// commit, §5). The first write error is captured into `result` because the
/// probe drain is a `FnMut` that can't return one; later batches are then skipped
/// and the error surfaces after the drain.
fn record_hit_batch(store: &mut Store, batch: &[(String, bool)], result: &mut Result<()>) {
    if result.is_err() {
        return;
    }
    let hits: Vec<&str> = batch
        .iter()
        .filter(|(_, hit)| *hit)
        .map(|(drv, _)| drv.as_str())
        .collect();
    if !hits.is_empty() {
        *result = store.record_cache_hits(&hits);
    }
}

/// Gather any missing cache facts for `targets`, reporting each probed drv via
/// `progress` and recording its hits as they land. Idempotent: a fully-known set
/// probes nothing. The display-less form used by [`build_targets_at`] (facts are
/// normally already probed by the tree phase below, so this is a no-op there).
fn probe_and_record(store: &mut Store, targets: &[Target], progress: &dyn Fn(usize)) -> Result<()> {
    let to_probe = probe_candidates(store, targets)?;
    if to_probe.is_empty() {
        return Ok(());
    }
    let mut result = Ok(());
    cache::in_cache_many(&to_probe, |batch| {
        progress(batch.len());
        record_hit_batch(store, batch, &mut result);
    });
    result
}

/// A prepared narinfo probe: its (blue) tree node, the drvs to probe, and the
/// store to record hits into — from [`probe_prepare`], run by [`probe_execute`].
pub struct Probe {
    store: Store,
    to_probe: Vec<String>,
    node: std::sync::Arc<live::Node>,
}

/// Reveal the narinfo `probe` leaf (DESIGN §7) — a cross-cutting phase whose
/// count climbs over the union of drvs with no fact yet — as a blue node, with
/// its total, *before* it runs. The candidate set comes from the observation log
/// alone (no `.drv` needed), so this can appear alongside `instantiate` while
/// both are still blue; the actual HTTP HEADs ([`probe_execute`]) read each
/// drv's output paths from its `.drv`, so they run only after `instantiate` has
/// written them. A fully-known build set adds no node and returns `None` — which
/// is what keeps a re-run of an unchanged report near-instant (no HTTP).
pub fn probe_prepare(targets: &[Target], tree: &live::Tree) -> Result<Option<Probe>> {
    let store = Store::open(&crate::paths::db_path()?)?;
    let to_probe = probe_candidates(&store, targets)?;
    if to_probe.is_empty() {
        return Ok(None);
    }
    let node = tree.counter("probe", 0, to_probe.len() as i64);
    Ok(Some(Probe {
        store,
        to_probe,
        node,
    }))
}

/// Run a prepared probe: HEAD each drv's outputs on `cache.nixos.org` and record
/// the hits, driving its node (blue → yellow → green).
pub fn probe_execute(probe: Probe) -> Result<()> {
    let Probe {
        mut store,
        to_probe,
        node,
    } = probe;
    node.set_running();
    // Record each batch of hits as it lands, so a ^C keeps the hits probed so far
    // and a restart skips them (DESIGN §5, §7); the node climbs over every verdict
    // in the batch, hit or miss.
    let mut result = Ok(());
    cache::in_cache_many(&to_probe, |batch| {
        node.add_count(batch.len() as i64);
        record_hit_batch(&mut store, batch, &mut result);
    });
    result?;
    node.set_done();
    Ok(())
}

fn build_targets_at(db: &std::path::Path, targets: &[Target], policy: BuildPolicy) -> Result<bool> {
    let mut store = Store::open(db)?;

    // Gather any missing cache facts, then load every target's history in one
    // SQLite round-trip — an all-known set costs a single query, no network.
    // (The tree's probe phase normally recorded these already, so this is a
    // no-op; it stays for the test path that drives the build directly.)
    probe_and_record(&mut store, targets, &|_| {})?;
    let drv_refs: Vec<&str> = targets.iter().map(|t| t.drv_path.as_str()).collect();
    let mut obs_by_drv = store.load_observations_many(&drv_refs)?;

    // Self-heal direct failures that built out of band (§5): re-check each
    // failure's outputs against the store — from its recorded `blocker`
    // (offline) or the `.drv` `drvs_to_materialize` just instantiated for a
    // failure that had none — and a failure whose outputs are now valid becomes
    // a `Built`, so `decide()` returns `SkipOk` and the report shows a success
    // with no `--retry`. This is the direct-failure analogue of the
    // dependency-block self-heal (`stale_dep_blocks`): a later success read from
    // the store outranks an earlier recorded failure (`flaky_success_wins`). A
    // failure resolved from its `.drv` that is still invalid gets its outputs
    // recorded, so the next run re-checks it offline instead of materializing
    // again — keeping a warm run whose failures are all recorded instant (§6).
    let FailureRecheck {
        heal: healed,
        record_outputs,
    } = recheck_direct_failures(targets, &obs_by_drv)?;
    if !healed.is_empty() || !record_outputs.is_empty() {
        for drv in &healed {
            let ob = Observation {
                drv_path: drv.clone(),
                outcome: Outcome::Built,
                blocker: Vec::new(),
            };
            store.add_observation(&ob)?;
            obs_by_drv.entry(drv.clone()).or_default().push(ob);
        }
        for (drv, outs) in record_outputs {
            store.add_observation(&Observation {
                drv_path: drv,
                outcome: Outcome::Failed,
                blocker: outs,
            })?;
        }
    }

    let obs_of = |drv: &str| obs_by_drv.get(drv).map(Vec::as_slice).unwrap_or(&[]);

    // Pass 1: decide per target, purely from the (just-refreshed) log, plus the
    // one store-backed input the pure predicate can't compute — whether a
    // recorded dependency-block has gone stale (its culprit built or was
    // substituted since; `stale_dep_blocks`, offline). Skips are silent — a
    // fully-cached run must print nothing.
    let stale = stale_dep_blocks(targets, &obs_by_drv)?;
    let dep_stale = |drv: &str| stale.get(drv).copied().unwrap_or(false);
    let mut to_build: Vec<usize> = Vec::new();
    for (i, t) in targets.iter().enumerate() {
        let observations = obs_of(&t.drv_path);
        let decision = policy.decide(observations, dep_stale(&t.drv_path));
        if decision == Decision::Build {
            to_build.push(i);
        }
    }

    // Pass 1b: forward-propagate *still-failing* dependencies (DESIGN.md §5). A
    // target whose build closure contains a dependency that only-fails locally
    // would just cascade to `DepFailed`, so drop it from the build set and record
    // that block now (committed immediately, so ^C-safe; the next run then skips
    // it from its own history). Two guards keep it sound and self-healing: the
    // log's failing candidates are re-verified against the store
    // (`verify_failing`), so a since-rebuilt/substituted dependency no longer
    // blocks; and the recorded `DepFailed` carries the culprit's output paths as
    // its `blocker`, so a later run can re-check the block offline and re-attempt
    // the dependent the moment the culprit heals. `--retry` disables propagation.
    let failing = if policy.retry {
        HashSet::new()
    } else {
        store.failing_drvs()?
    };
    if !failing.is_empty() && !to_build.is_empty() {
        // One union query first: only the drvs whose closure actually reaches the
        // log's failing set are worth the per-drv verification (drv_outputs + a
        // validity probe), and a since-healed candidate drops out here.
        let cand: Vec<&str> = to_build
            .iter()
            .map(|&i| targets[i].drv_path.as_str())
            .collect();
        let reachable: HashSet<String> = drv_closure(&cand)?
            .into_iter()
            .filter(|d| failing.contains(d))
            .collect();
        let verified = verify_failing(&reachable)?;
        if !verified.is_empty() {
            let mut still_build = Vec::new();
            let mut blocked_seen: HashSet<&str> = HashSet::new();
            for &i in &to_build {
                let drv = targets[i].drv_path.as_str();
                // A still-failing dependency in this target's closure is the
                // culprit; its outputs become the block's `blocker`. Exclude the
                // target itself: `--requisites` lists a drv among its own inputs,
                // and a re-opened target still carries its own failure in the log,
                // so without this a target would block *itself* forever.
                let culprit = drv_closure(&[drv])?
                    .into_iter()
                    .filter(|d| d != drv)
                    .find_map(|d| verified.get(&d).cloned());
                match culprit {
                    None => still_build.push(i),
                    // Aliased attrs share a drv — record the block once.
                    Some(blocker) => {
                        if blocked_seen.insert(drv) {
                            store.add_observation(&Observation {
                                drv_path: drv.to_string(),
                                outcome: Outcome::DepFailed,
                                blocker,
                            })?;
                        }
                    }
                }
            }
            to_build = still_build;
        }
    }

    // Pass 2: one nom build for the whole set, recording each drv's outcome the
    // moment its build activity stops — its outputs' validity at that instant is
    // the build's own result (see `batch_build`). Both requested targets and
    // their transitive dependencies fire the callback; a target records its
    // outcome either way, a dependency only when it *failed* (that failure fact
    // is what pass 1b propagates forward on a later run). Recording incrementally
    // is what makes ^C mid-batch safe: every fact observed so far is already
    // committed, so only in-flight and never-started builds cost anything on
    // the next run. (Nix keeps the build log itself; `nix log <drv>` gets it.)
    // Whether we run the nom build below — i.e. whether this call produces any
    // stderr output. The caller uses it to fence the build off with a separator
    // only when there's actually a build to fence (an all-cached set is silent).
    let ran = !to_build.is_empty();
    if ran {
        let drvs: Vec<&str> = to_build
            .iter()
            .map(|&i| targets[i].drv_path.as_str())
            .collect();
        // Several targets can share a drv (aliased attrs); record it once.
        let requested: HashSet<&str> = drvs.iter().copied().collect();
        let mut recorded: HashMap<String, Outcome> = HashMap::new();
        batch_build(&drvs, |drv| {
            let built = drv_built(drv)?;
            if requested.contains(drv) {
                // A requested target: record its own outcome, success or failure.
                // A direct failure records the drv's *own* output paths as its
                // `blocker` (the .drv is present here, having just been built),
                // so a later run can re-check their validity offline and
                // self-heal if it built out of band since — the direct-failure
                // analogue of a DepFailed's culprit blocker (DESIGN §5).
                let (outcome, blocker) = if built {
                    (Outcome::Built, Vec::new())
                } else {
                    (Outcome::Failed, cache::drv_outputs(drv).unwrap_or_default())
                };
                store.add_observation(&Observation {
                    drv_path: drv.to_string(),
                    outcome,
                    blocker,
                })?;
                recorded.insert(drv.to_string(), outcome);
            } else if !built {
                // A transitive *dependency* that failed on its own. Record it
                // (keyed on its drvpath, like every fact) so a later run
                // propagates the failure forward — skipping any target that would
                // re-pull it (pass 1b) — and so ^C keeps it. A dependency
                // *success* needs no row: nix's own store validity already
                // remembers it, and `verify_failing` consults exactly that so a
                // healed dependency stops blocking. Its own outputs are recorded
                // as the `blocker` too, so a dependency that later builds out of
                // band self-heals just like a requested target (DESIGN §5).
                store.add_observation(&Observation {
                    drv_path: drv.to_string(),
                    outcome: Outcome::Failed,
                    blocker: cache::drv_outputs(drv).unwrap_or_default(),
                })?;
            }
            Ok(())
        })?;

        // Post-batch attribution of requested targets that got *no* build
        // activity (blocked by a failed dependency, or already valid). Only two
        // outcomes are recorded here, and both are ground truth — never an
        // inference gated on nix's ambiguous exit status (the old "Pass 3", which
        // could mis-attribute never-started drvs as failures when a batch aborted
        // with a normal error code; DESIGN §5):
        //   - outputs valid -> `Built` (another target realized it as a
        //     dependency, or it was already valid);
        //   - a still-failing dependency in its closure -> `DepFailed`, tagged
        //     with that culprit's outputs so the block self-heals next run.
        // A leftover with neither (nix never reached it, and nothing in its
        // closure is actually failing) is left unrecorded and re-attempted next
        // run — sound, since we never assert a failure we didn't observe.
        let leftover: Vec<&str> = drvs
            .iter()
            .copied()
            .filter(|d| !recorded.contains_key(*d))
            .collect::<HashSet<&str>>()
            .into_iter()
            .collect();
        if !leftover.is_empty() {
            let built_map = build_outcomes(&leftover)?;
            // Pass 2 may have just added this batch's own dependency failures.
            let failing_now = if policy.retry {
                HashSet::new()
            } else {
                store.failing_drvs()?
            };
            for &drv in &leftover {
                if built_map.get(drv).copied().unwrap_or(false) {
                    store.add_observation(&Observation {
                        drv_path: drv.to_string(),
                        outcome: Outcome::Built,
                        blocker: Vec::new(),
                    })?;
                    continue;
                }
                if failing_now.is_empty() {
                    continue;
                }
                // Exclude the target itself (see pass 1b): a leftover target that
                // failed to build isn't blocked *by itself*, and its own drv is in
                // its `--requisites`.
                let reachable: HashSet<String> = drv_closure(&[drv])?
                    .into_iter()
                    .filter(|d| d != drv && failing_now.contains(d))
                    .collect();
                if let Some(blocker) = verify_failing(&reachable)?.into_values().next() {
                    store.add_observation(&Observation {
                        drv_path: drv.to_string(),
                        outcome: Outcome::DepFailed,
                        blocker,
                    })?;
                }
            }
        }
    }

    Ok(ran)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn target(drv: &str) -> Target {
        Target {
            drv_path: drv.into(),
        }
    }

    fn planted(drv: &str, outcome: Outcome) -> Observation {
        Observation {
            drv_path: drv.into(),
            outcome,
            blocker: Vec::new(),
        }
    }

    /// Like `planted`, but for a `DepFailed` whose culprit outputs are recorded —
    /// used to exercise the offline staleness re-check (`stale_dep_blocks`).
    fn planted_block(drv: &str, blocker: &[&str]) -> Observation {
        Observation {
            drv_path: drv.into(),
            outcome: Outcome::DepFailed,
            blocker: blocker.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A batch of probe verdicts records only its hits (as `Built`), skips its
    /// misses, and surfaces no error — so a ^C mid-probe keeps every committed
    /// batch, and a restart no longer re-probes the hits (they drop out of
    /// `probe_candidates`).
    #[test]
    fn record_hit_batch_commits_hits_so_a_restart_skips_them() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("npb.sqlite");
        let mut s = Store::open(&db).unwrap();
        let mut result = Ok(());

        record_hit_batch(
            &mut s,
            &[
                ("/nix/store/hit.drv".to_string(), true),
                ("/nix/store/miss.drv".to_string(), false),
            ],
            &mut result,
        );
        result.unwrap();

        // The hit is a durable `Built` fact; the miss recorded nothing.
        let hit = s.load_observations("/nix/store/hit.drv").unwrap();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].outcome, Outcome::Built);
        assert!(
            s.load_observations("/nix/store/miss.drv")
                .unwrap()
                .is_empty()
        );

        // A drv with a recorded hit is no longer a probe candidate — exactly the
        // restart-skip this fix is about; the miss still is.
        let cands = probe_candidates(
            &s,
            &[target("/nix/store/hit.drv"), target("/nix/store/miss.drv")],
        )
        .unwrap();
        assert_eq!(cands, vec!["/nix/store/miss.drv".to_string()]);
    }

    /// `drvs_to_materialize` must mirror pass 1's `Decision::Build`, from the log
    /// alone: only drvs the build phase will still probe or build get a `.drv`.
    /// This is what lets a fully-cached run skip instantiation (DESIGN §5–§6).
    #[test]
    fn drvs_to_materialize_matches_the_build_decision() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("npb.sqlite");
        {
            let mut s = Store::open(&db).unwrap();
            // Built locally, and a recorded cache hit (the same plain `Built`
            // fact, §7): decided as successes.
            s.add_observation(&planted("/nix/store/built.drv", Outcome::Built))
                .unwrap();
            s.add_observation(&planted("/nix/store/cached.drv", Outcome::Built))
                .unwrap();
            // A direct failure with NO recorded outputs: can't be self-heal-checked
            // offline, so it must be materialized to resolve them (§5).
            s.add_observation(&planted("/nix/store/failed.drv", Outcome::Failed))
                .unwrap();
            // A direct failure that DID record its outputs: re-checked offline,
            // needs no `.drv`.
            s.add_observation(&Observation {
                drv_path: "/nix/store/failed-recorded.drv".into(),
                outcome: Outcome::Failed,
                blocker: vec!["/nix/store/out-of-failed-recorded".into()],
            })
            .unwrap();
        }
        // "/nix/store/new.drv" has no fact.
        let targets = vec![
            target("/nix/store/built.drv"),
            target("/nix/store/cached.drv"),
            target("/nix/store/failed.drv"),
            target("/nix/store/failed-recorded.drv"),
            target("/nix/store/new.drv"),
        ];

        // Default policy: the never-observed drv needs a `.drv` to build, and the
        // failure with no recorded outputs needs one to self-heal-check (§5). The
        // failure that recorded its outputs is checked offline and stays out.
        let need = drvs_to_materialize_at(&db, &targets, BuildPolicy::default()).unwrap();
        assert_eq!(
            need,
            HashSet::from([
                "/nix/store/new.drv".to_string(),
                "/nix/store/failed.drv".to_string(),
            ])
        );

        // A set that's fully decided offline (successes plus a failure that
        // recorded its outputs) needs nothing — instantiation is skipped entirely.
        let cached_only = vec![
            target("/nix/store/built.drv"),
            target("/nix/store/cached.drv"),
            target("/nix/store/failed-recorded.drv"),
        ];
        assert!(
            drvs_to_materialize_at(&db, &cached_only, BuildPolicy::default())
                .unwrap()
                .is_empty()
        );

        // --retry re-opens a failed target, so it needs its `.drv` again.
        let retry = BuildPolicy { retry: true };
        assert!(
            drvs_to_materialize_at(&db, &targets, retry)
                .unwrap()
                .contains("/nix/store/failed.drv")
        );
    }

    /// `drvs_needing_instantiation` keeps the recipes still **absent** from the
    /// store and drops those already present — a present `.drv` needs no
    /// re-materialization (§6). This pins the keep-the-absent polarity (the easy
    /// bug: keeping the *valid* ones would re-instantiate exactly what's on disk).
    #[test]
    fn drvs_needing_instantiation_keeps_only_the_absent() {
        let need = HashSet::from([
            "/nix/store/present.drv".to_string(),
            "/nix/store/absent.drv".to_string(),
        ]);
        // `invalid_paths` would return the absent subset; simulate it here so the
        // filter is exercised without a Nix store.
        let absent = HashSet::from(["/nix/store/absent.drv".to_string()]);
        assert_eq!(
            retain_absent(need, &absent),
            HashSet::from(["/nix/store/absent.drv".to_string()])
        );
    }

    /// An empty set short-circuits (no `nix-store` call on a warm run, which never
    /// reaches here anyway since instantiation is skipped from the log alone, §6).
    #[test]
    fn drvs_needing_instantiation_empty_is_noop() {
        assert!(
            drvs_needing_instantiation(HashSet::new())
                .unwrap()
                .is_empty()
        );
    }

    /// Instantiate a nix expression, returning its .drv path.
    fn instantiate(expr: &str, attr: &str) -> String {
        let out = Command::new("nix-instantiate")
            .args(["--expr", expr, "-A", attr])
            .output()
            .expect("running nix-instantiate");
        assert!(
            out.status.success(),
            "nix-instantiate -A {attr} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// End-to-end against real nix (hence ignored; `cargo test -- --ignored`):
    /// build a set with a fast failure, a slow success, and a drv blocked by the
    /// failure. Asserts the attribution of all three outcomes AND the property
    /// that makes ^C safe: the failure's observation is committed to SQLite
    /// while the batch is still building, not after it finishes.
    #[test]
    #[ignore = "builds real derivations via nix; needs nix, nom, and ~10s"]
    fn records_outcomes_while_batch_still_building() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("npb-build-test-{nonce}"));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("npb.sqlite");

        // Nonce'd names so nothing is valid in the store from a previous run.
        let expr = format!(
            r#"let
                 mk = name: cmd: derivation {{
                   name = name; system = builtins.currentSystem;
                   builder = "/bin/sh"; args = ["-c" cmd];
                 }};
                 fail = mk "npb-test-fail-{nonce}" "exit 1";
                 # Spin on shell builtins (~10s): the sandbox has no `sleep`
                 # (PATH is /path-not-set), and the delay must outlast the poll
                 # below that watches for the failure's row.
                 slow = mk "npb-test-slow-{nonce}"
                   "i=0; while [ $i -lt 15000000 ]; do i=$((i+1)); done; echo ok > $out";
                 blocked = mk "npb-test-blocked-{nonce}" "cat ${{fail}} > $out";
               in {{ inherit fail slow blocked; }}"#
        );
        let fail = instantiate(&expr, "fail");
        let slow = instantiate(&expr, "slow");
        let blocked = instantiate(&expr, "blocked");

        let targets: Vec<Target> = [&fail, &slow, &blocked]
            .into_iter()
            .map(|drv| Target {
                drv_path: drv.clone(),
            })
            .collect();
        let db2 = db.clone();
        let builder =
            std::thread::spawn(move || build_targets_at(&db2, &targets, BuildPolicy::default()));

        // The failure is near-instant, the success sleeps 8s; its Failed row
        // must land while the batch (and the thread driving it) still runs.
        let mut seen_mid_batch = false;
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !builder.is_finished() {
            // Concurrent open can transiently fail (writer holds the lock).
            if let Ok(s) = Store::open(&db)
                && let Ok(obs) = s.load_observations(&fail)
                && obs.iter().any(|o| o.outcome == Outcome::Failed)
            {
                seen_mid_batch = !builder.is_finished();
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(
            seen_mid_batch,
            "failure was not recorded while the batch was still building"
        );

        builder.join().unwrap().unwrap();

        // Every outcome is recovered from the observation log — the same ground
        // truth the production path renders from. Each drv is observed exactly
        // once: the failure and the slow success from their own build activity,
        // the blocked drv from the post-batch output-validity sweep.
        let s = Store::open(&db).unwrap();
        let obs_of = |drv: &str| {
            let obs = s.load_observations(drv).unwrap();
            assert_eq!(obs.len(), 1, "exactly one local observation per drv");
            obs.into_iter().next().unwrap()
        };
        assert_eq!(obs_of(&fail).outcome, Outcome::Failed);
        assert_eq!(obs_of(&slow).outcome, Outcome::Built);
        assert_eq!(obs_of(&blocked).outcome, Outcome::DepFailed);

        let _ = fs::remove_dir_all(&dir);
    }

    /// A dependency that fails is recorded even though it was never a requested
    /// target, and its dependent is swept to `DepFailed` — the raw material pass
    /// 1b later propagates forward (DESIGN.md §5).
    #[test]
    #[ignore = "builds real derivations via nix; needs nix, nom"]
    fn dependency_failure_is_recorded() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("npb-depfail-test-{nonce}"));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("npb.sqlite");

        // `top` depends on `dep`, which fails. Only `top` is a requested target.
        let expr = format!(
            r#"let
                 mk = name: args: derivation ({{
                   name = name; system = builtins.currentSystem;
                   builder = "/bin/sh";
                 }} // args);
                 dep = mk "npb-dep-fail-{nonce}" {{ args = ["-c" "exit 1"]; }};
                 top = mk "npb-top-{nonce}" {{ args = ["-c" "cat ${{dep}} > $out"]; }};
               in {{ inherit dep top; }}"#
        );
        let dep = instantiate(&expr, "dep");
        let top = instantiate(&expr, "top");

        let targets = [Target {
            drv_path: top.clone(),
        }];
        build_targets_at(&db, &targets, BuildPolicy::default()).unwrap();

        let s = Store::open(&db).unwrap();
        // The dependency's failure is recorded even though it was never a target
        // — keyed on its own drvpath, from the incremental (dep-branch) record.
        let dep_obs = s.load_observations(&dep).unwrap();
        assert_eq!(dep_obs.len(), 1, "the failing dependency is recorded once");
        assert_eq!(dep_obs[0].outcome, Outcome::Failed);
        // The requested target is blocked by that dependency.
        let top_obs = s.load_observations(&top).unwrap();
        assert_eq!(top_obs.len(), 1);
        assert_eq!(top_obs[0].outcome, Outcome::DepFailed);

        let _ = fs::remove_dir_all(&dir);
    }

    /// The ^C-recovery path (DESIGN.md §5): with a dependency's failure already
    /// on record (as an interrupted run would have left it) but the dependent
    /// *un*recorded (its post-batch sweep was skipped by the ^C), a re-run skips
    /// the dependent from its closure — recording it `DepFailed` — and never
    /// re-attempts the failing dependency.
    #[test]
    #[ignore = "instantiates real derivations via nix; needs nix"]
    fn known_dependency_failure_skips_dependent_without_rebuilding() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("npb-propagate-test-{nonce}"));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("npb.sqlite");

        let expr = format!(
            r#"let
                 mk = name: args: derivation ({{
                   name = name; system = builtins.currentSystem;
                   builder = "/bin/sh";
                 }} // args);
                 dep = mk "npb-pdep-fail-{nonce}" {{ args = ["-c" "exit 1"]; }};
                 top = mk "npb-ptop-{nonce}" {{ args = ["-c" "cat ${{dep}} > $out"]; }};
               in {{ inherit dep top; }}"#
        );
        let dep = instantiate(&expr, "dep");
        let top = instantiate(&expr, "top");

        // Simulate the state a ^C leaves: the dependency's failure is recorded,
        // but the dependent has no observation at all.
        {
            let mut s = Store::open(&db).unwrap();
            s.add_observation(&Observation {
                drv_path: dep.clone(),
                outcome: Outcome::Failed,
                blocker: Vec::new(),
            })
            .unwrap();
        }

        let targets = [Target {
            drv_path: top.clone(),
        }];
        build_targets_at(&db, &targets, BuildPolicy::default()).unwrap();

        let s = Store::open(&db).unwrap();
        // The dependent was skipped without building and recorded blocked, tagged
        // with the culprit dependency's outputs (the self-healing `blocker`).
        let top_obs = s.load_observations(&top).unwrap();
        assert_eq!(top_obs.len(), 1);
        assert_eq!(top_obs[0].outcome, Outcome::DepFailed);
        assert!(
            !top_obs[0].blocker.is_empty(),
            "a propagated block records its culprit's outputs"
        );
        // The failing dependency was NOT re-attempted: still exactly the one
        // planted observation (a rebuild would have appended a second).
        assert_eq!(
            s.load_observations(&dep).unwrap().len(),
            1,
            "the known-failing dependency must not be rebuilt"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The direct-failure analogue of the dependency self-heal (DESIGN.md §5): a
    /// `Failed` records the drv's own outputs, and if they are valid in the store
    /// on a later run — the drv built out of band — the sticky failure is stale
    /// and heals. Uses `nix-store --add` for a guaranteed-valid path (cheap, no
    /// build); a bogus path stands in for a still-missing output.
    #[test]
    #[ignore = "queries the real store via nix-store; needs nix"]
    fn direct_failure_self_heals_when_own_outputs_are_valid() {
        let valid = String::from_utf8(
            Command::new("nix-store")
                .args(["--add", "/etc/hostname"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let invalid =
            "/nix/store/00000000000000000000000000000000-npb-self-heal-absent".to_string();

        let failed = |drv: &str, blocker: Vec<String>| Observation {
            drv_path: drv.into(),
            outcome: Outcome::Failed,
            blocker,
        };
        let mut obs: HashMap<String, Vec<Observation>> = HashMap::new();
        // Failed, own outputs now valid -> heals.
        obs.insert("/a.drv".into(), vec![failed("/a.drv", vec![valid.clone()])]);
        // Failed, own outputs still missing -> stays sticky.
        obs.insert(
            "/b.drv".into(),
            vec![failed("/b.drv", vec![invalid.clone()])],
        );
        // Failed with no recorded outputs -> can't re-check, stays sticky.
        obs.insert("/c.drv".into(), vec![failed("/c.drv", vec![])]);
        // A recorded success already decides it -> not re-derived from the store.
        obs.insert(
            "/d.drv".into(),
            vec![
                failed("/d.drv", vec![valid.clone()]),
                planted("/d.drv", Outcome::Built),
            ],
        );

        let targets: Vec<Target> = ["/a.drv", "/b.drv", "/c.drv", "/d.drv"]
            .iter()
            .map(|d| target(d))
            .collect();
        let FailureRecheck {
            heal,
            record_outputs,
        } = recheck_direct_failures(&targets, &obs).unwrap();

        assert!(heal.contains("/a.drv"), "valid own outputs must heal");
        assert!(!heal.contains("/b.drv"), "missing outputs stay sticky");
        assert!(!heal.contains("/c.drv"), "no recorded outputs -> no heal");
        assert!(
            !heal.contains("/d.drv"),
            "a recorded Built already decides it"
        );
        // `/b` was resolved from the log (recorded blocker), so a still-invalid
        // check doesn't re-record it; only outputs freshly resolved from a `.drv`
        // are backfilled, and none here came from a `.drv`.
        assert!(
            record_outputs.is_empty(),
            "already-recorded failures aren't re-recorded"
        );
    }

    /// The self-healing path (DESIGN.md §5): a dependent recorded `DepFailed`,
    /// whose culprit dependency has since built, is re-attempted on the next run
    /// *without* `--retry` — because the block's recorded `blocker` outputs are
    /// now valid in the store, so `stale_dep_blocks` reports it stale. The
    /// dependent then builds and its success outranks the stale block.
    #[test]
    #[ignore = "builds real derivations via nix; needs nix, nom"]
    fn stale_dependency_block_reattempts_dependent() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("npb-heal-test-{nonce}"));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("npb.sqlite");

        // `dep` *succeeds* here (a flaky dependency that has since been fixed);
        // `top` depends on it. Only `top` is a requested target.
        let expr = format!(
            r#"let
                 mk = name: args: derivation ({{
                   name = name; system = builtins.currentSystem;
                   builder = "/bin/sh";
                 }} // args);
                 dep = mk "npb-heal-dep-{nonce}" {{ args = ["-c" "echo ok > $out"]; }};
                 # `: ${{dep}}` references dep (making it a build input) without
                 # any external command — the sandbox PATH is empty, so `cat` &c.
                 # aren't available; only shell builtins (`:`, `echo`, `>`) are.
                 top = mk "npb-heal-top-{nonce}" {{ args = ["-c" ": ${{dep}}; echo ok > $out"]; }};
               in {{ inherit dep top; }}"#
        );
        let dep = instantiate(&expr, "dep");
        let top = instantiate(&expr, "top");
        let dep_outs = cache::drv_outputs(&dep).unwrap();

        // Realize `dep` so its outputs are valid in the store — the culprit has
        // healed. (Build it directly; it isn't a requested target of this run.)
        let ok = Command::new("nix")
            .args(["build", "--no-link", &format!("{dep}^*")])
            .args(["--extra-experimental-features", "nix-command"])
            .status()
            .expect("building the dependency");
        assert!(ok.success(), "the healed dependency should build");

        // Plant the state a prior run left: `dep` failed once, and `top` was
        // blocked by it — tagged with `dep`'s (now-valid) outputs.
        {
            let mut s = Store::open(&db).unwrap();
            s.add_observation(&planted(&dep, Outcome::Failed)).unwrap();
            let blocker: Vec<&str> = dep_outs.iter().map(String::as_str).collect();
            s.add_observation(&planted_block(&top, &blocker)).unwrap();
        }

        let targets = [Target {
            drv_path: top.clone(),
        }];
        // Default policy — no --retry. The stale block alone must re-open `top`.
        build_targets_at(&db, &targets, BuildPolicy::default()).unwrap();

        let s = Store::open(&db).unwrap();
        let top_obs = s.load_observations(&top).unwrap();
        assert!(
            top_obs.iter().any(|o| o.outcome == Outcome::Built),
            "a stale dep-block must re-attempt the dependent, which then builds: {top_obs:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
