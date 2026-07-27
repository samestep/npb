//! Run `nix-eval-jobs` and schedule the runs: evaluate a nixpkgs revision into
//! an `attr -> drv` map — the first spine primitive (DESIGN.md §6, §9), a pure
//! fact keyed by `(tree, system)` (the git *tree*, not the commit — see
//! [`crate::model::Rev`]), computed at most once and cached as one flat file per
//! eval (the file format and its diff live in [`crate::evalfile`]).
//!
//! The revision's source comes from `builtins.fetchGit` on its [`Rev::commit`],
//! so Nix fetches and caches it in the store — npb manages no worktrees.
//! `nix-eval-jobs` output is parsed by streaming NDJSON straight off the child's
//! stdout (never buffering the whole, meta-heavy output).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::evalfile::{eval_path, write_eval};
use crate::live;
use crate::model::{AttrEval, Profile, Rev, TestJob};

/// The nixpkgs `config` attrset for a [`Profile`] (DESIGN.md §6). npb evaluates
/// under the profile the user means — strict by default — so a
/// broken/unsupported/insecure package (and anything forcing such a dependency)
/// throws and falls out, precisely and for free; the throw *is* the signal, so
/// there's no `--meta` and no post-hoc reclassification. `allowUnfree` is always
/// on and `allowAliases` always off (both matching nixpkgs-review): aliases like
/// `claude-code-bin` resolve to the same drv as their target, so evaluating them
/// only adds a redundant attr to the changed set (collapsed onto its target's
/// line in the report, but still noise); `allowAliases = false` makes them throw
/// and fall out at eval, exactly as ofborg/nixpkgs-review see the package set.
/// The three allow-flags are added only when the profile permits them. The
/// profile is part of the eval cache key ([`Profile::qualify`]), so different
/// profiles never share an eval file.
fn profile_config(profile: Profile) -> String {
    let mut s = String::from("{ allowUnfree = true; allowAliases = false;");
    if profile.broken {
        s.push_str(" allowBroken = true;");
    }
    if profile.unsupported {
        s.push_str(" allowUnsupportedSystem = true;");
    }
    if profile.insecure {
        s.push_str(" allowInsecurePredicate = _: true;");
    }
    s.push_str(" }");
    s
}

// --- nix-eval-jobs output ---------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawJob {
    attr: String,
    /// The attr path as an array of *unquoted* elements. Preferred over `attr`
    /// (which nix-eval-jobs quotes when an element contains a `.`) when a clean,
    /// dotted label is wanted — see the test eval.
    attr_path: Vec<String>,
    /// `None` when evaluation of the attr **threw** (the job line carries an
    /// `error` message instead, which we don't keep — re-evaluating reproduces
    /// it). Under the run's profile, that's exactly a broken/unsupported/insecure
    /// attr, or one forcing such a dependency (DESIGN §6).
    drv_path: Option<String>,
}

fn raw_to_attr_eval(raw: RawJob) -> AttrEval {
    AttrEval {
        attr: raw.attr,
        drv_path: raw.drv_path,
    }
}

/// Map a `--tests` job to a [`TestJob`]. Label from `attrPath` (unquoted
/// elements) rather than `attr` (which nix-eval-jobs quotes for the dotted
/// package component, e.g. `"python3Packages.requests".tests.foo`): element 0
/// is the package we asked for, and the whole path joined is the clean
/// `<pkg>.tests.<name>` label.
fn raw_to_test_job(raw: RawJob) -> TestJob {
    TestJob {
        pkg_attr: raw.attr_path.first().cloned().unwrap_or_default(),
        test_attr: raw.attr_path.join("."),
        drv_path: raw.drv_path,
    }
}

// --- running the evaluator --------------------------------------------------

/// Escape a string for embedding inside a Nix `"..."` literal: backslashes,
/// double quotes, and the `${` interpolation opener. (Attr names and store
/// paths virtually never contain these, but the repo path and revision are
/// user input.)
fn nix_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace("${", "\\${")
}

/// A space-separated Nix string-list body — `"a" "b" ` — each element escaped
/// for a Nix `"..."` literal. Shared by every expression builder that
/// interpolates a list of attr names/paths ([`shard_expr`], [`select_expr`],
/// [`build_tests_expr`]), so the escaping lives in one place.
fn nix_string_list(items: &[String]) -> String {
    items
        .iter()
        .map(|s| format!("\"{}\" ", nix_escape(s)))
        .collect()
}

/// Build the whole-package-set Nix expression `nix-eval-jobs` walks, under the
/// nixpkgs `config` of the run's [`Profile`] ([`profile_config`]). The revision's
/// source is fetched by `builtins.fetchGit` at `rev` (a commit — real or the
/// synthetic one minted for the working tree; the eval depends only on the tree
/// it resolves to, which is why the cache keys on the tree, not this commit — see
/// [`Rev`]). Interpolants are escaped via [`nix_escape`] (the repo path in
/// particular is user input, `--nixpkgs`).
fn build_expr(repo: &Path, rev: &str, system: &str, config: &str) -> String {
    format!(
        "import (builtins.fetchGit {{ url = \"{}\"; rev = \"{}\"; }}) \
         {{ system = \"{}\"; config = {config}; }}",
        nix_escape(&repo.display().to_string()),
        nix_escape(rev),
        nix_escape(system),
    )
}

/// Scrub the evaluator's environment of the variables nixpkgs is known to
/// leak into derivations via `builtins.getEnv` (drbd bakes `$SHELL` into its
/// Makefile patch), so cached evals don't depend on the shell npb was launched
/// from — `getEnv` then returns `""`, matching a hermetic evaluation.
fn scrub_env(cmd: &mut Command) -> &mut Command {
    cmd.env_remove("SHELL");
    cmd
}

/// Run one `nix-eval-jobs --expr <expr>` (with `workers` workers each capped at
/// `per_worker_mb`), streaming its NDJSON stdout through `map_job` into a vec.
/// `on_item` fires per streamed job so callers can render progress however they
/// like; `label` names the run in the integrity-gate error. Shared by the
/// sharded full-set eval (`map_job` → [`AttrEval`]) and the targeted test eval
/// (`map_job` → [`TestJob`], relabelled from `attrPath`) — both stream the same
/// job shape and want the same truncation gate, so it's generic over the output.
fn stream_jobs<T>(
    expr: &str,
    workers: usize,
    per_worker_mb: u64,
    instantiate: bool,
    label: &str,
    map_job: impl Fn(RawJob) -> T,
    mut on_item: impl FnMut(),
) -> Result<Vec<T>> {
    // nix-eval-jobs prints a full Nix traceback per errored attr (megabytes over a
    // whole package set), and the actionable per-attr error is already in the
    // stdout JSON — so we neither inherit its stderr (terminal spam) nor persist
    // it to disk. A thread drains stderr into a bounded ring buffer, keeping only
    // the last few lines for the fatal-error diagnostic below; draining it (vs. an
    // undrained pipe) also can't deadlock while we stream stdout.

    // nix-eval-jobs takes the expression inline (`--expr E`) or as a file-path
    // positional. The `--tests` expression lists every changed package, so on a
    // big changed set an inline `--expr` blows past ARG_MAX (E2BIG on spawn);
    // writing it to a temp file and passing the path works for any size (and the
    // small shard/full-set exprs don't care). The evaluated expression is
    // byte-identical either way — same drvs — so this doesn't affect the cached
    // evals. Kept alive until the child exits (nix-eval-jobs reads it at start).
    let mut expr_file = tempfile::Builder::new()
        .prefix("npb-eval-")
        .suffix(".nix")
        .tempfile()
        .context("creating nix-eval-jobs expr file")?;
    expr_file
        .write_all(expr.as_bytes())
        .and_then(|()| expr_file.flush())
        .context("writing nix-eval-jobs expr file")?;
    let workers_s = workers.to_string();
    let max_s = per_worker_mb.to_string();
    let mut cmd = Command::new("nix-eval-jobs");
    cmd.args(["--workers", &workers_s, "--max-memory-size", &max_s]);
    // `--no-instantiate` evaluates without writing the `.drv` files. The full-set
    // walk only needs the drvPath + outputs (both emitted regardless), so skipping
    // the writes is ~40% faster and avoids instantiating the ~114k attrs it never
    // builds; the changed set is materialized later, by [`instantiate_execute`].
    // The two targeted evals do want the writes ([`eval_tests`] as it goes,
    // [`instantiate_execute`] by definition) — a `drvPath` is the same either way,
    // so this only decides whether the recipe lands in the store.
    if !instantiate {
        cmd.arg("--no-instantiate");
    }
    let mut child = scrub_env(&mut cmd)
        .arg(expr_file.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning nix-eval-jobs (on PATH? use the flake dev shell)")?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stderr_tail = thread::spawn(move || {
        const KEEP: usize = 20;
        let mut ring: VecDeque<String> = VecDeque::with_capacity(KEEP + 1);
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if ring.len() == KEEP {
                ring.pop_front();
            }
            ring.push_back(line);
        }
        ring.into_iter().collect::<Vec<_>>().join("\n")
    });

    let mut attrs = Vec::new();
    for item in serde_json::Deserializer::from_reader(BufReader::new(stdout)).into_iter::<RawJob>()
    {
        match item.context("parsing nix-eval-jobs output") {
            Ok(raw) => attrs.push(map_job(raw)),
            Err(e) => {
                // A `Child` is not killed on drop: bail out without reaping and
                // a multi-GB nix-eval-jobs (plus its workers) keeps evaluating
                // into the void. Kill it (which also ends the stderr thread via
                // EOF) before surfacing the parse error.
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
        on_item();
    }

    let status = child.wait().context("waiting for nix-eval-jobs")?;
    let stderr_tail = stderr_tail.join().unwrap_or_default();
    // Integrity gate. Per-attr eval errors are emitted *in band* as JSON
    // (`{"attr":…,"error":…}`) and do NOT affect the exit code — a complete
    // full-set eval exits 0 even with thousands of `throw`n attrs. A non-zero
    // exit means a *fatal* abort: a worker died mid-eval, in which case the
    // streamed output is silently TRUNCATED — we got some attrs but not all.
    // Caching that would poison every future diff/report with phantom "removed"
    // packages, so we refuse it outright rather than trust a partial. The
    // [`EvalAborted`] payload is what lets the scheduler recover (its ladder in
    // [`run_shards`]) and, when it can't, say what actually happened.
    if !status.success() {
        return Err(anyhow::Error::new(EvalAborted {
            status: status.to_string(),
            streamed: attrs.len(),
            stderr_tail,
        })
        .context(format!("nix-eval-jobs did not finish evaluating {label}")));
    }
    Ok(attrs)
}

// --- targeted test eval (passthru.tests of the changed set) ------------------
//
// The `--tests` feature (ported from nixpkgs-review#397): for the packages in a
// change's *changed set*, also build their `passthru.tests`. This is a small,
// targeted eval over the (few) changed attrs, distinct from the full-set eval —
// and it *is* cached, per package, in SQLite (see `store::Store` and `main`): a
// test's drv is a pure function of `(commit, system, package-attr)`, so
// `eval_tests` runs only over the packages a run hasn't cached yet (the misses),
// and a fully-cached re-run touches no `nix-eval-jobs` at all. It's a SQLite
// fact, not a flat eval file, because the access pattern is keyed/incremental
// (look up a package, append new ones) rather than the full-set eval's
// bulk/write-once/read-whole-and-diff (DESIGN §4).
//
// The full-set `nix-eval-jobs` walk never reaches these drvs: a package's
// `passthru.tests` is a plain attrset without `recurseForDerivations`, so it's
// not descended into. We surface them with a targeted expression: a job tree
// `<pkg>.tests.<name>` where each package's `.tests` is a *thunk* forced by
// `nix-eval-jobs` in its per-attr worker — so a package that fails to evaluate
// (even an uncatchable parse error `tryEval` can't trap) is isolated to its own
// attr, exactly as in the full-set walk, rather than aborting the whole eval.

/// Nix expression exposing the `passthru.tests` of `attrs` at one revision as a
/// `nix-eval-jobs` job tree. Each requested `<pkg>` becomes a recursable node
/// `{ recurseForDerivations = true; tests = <thunk>; }`; the `tests` thunk (which
/// is what forces the package) is evaluated per-attr in a worker, so a throwing
/// package errors only its own subtree. `tests` resolves to the package's
/// `passthru.tests` — a derivation (emitted as `<pkg>.tests`) or an attrset made
/// recursable (emitted as `<pkg>.tests.<name>`); anything else yields no jobs.
///
/// **Profile-aware availability.** A `passthru.tests` entry is usually a
/// `nixosTest`/`vm-test-run` derivation, which does *not* pass through nixpkgs'
/// `check-meta` `commonMeta` — so, unlike a normal package, evaluating it under
/// the profile config does *not* make it throw when it's unsupported on this
/// system or insecure. `mark` reintroduces that check per test: platform support
/// via `lib.meta.availableOn`, insecurity via `knownVulnerabilities`. A test
/// unavailable under the run's profile is **dropped** — replaced by `{ }`, so
/// nix-eval-jobs emits no job for it and it renders ➖ absent (rather than a
/// phantom build). Under `--allow-unsupported`/`--allow-insecure` the respective
/// check is disabled and the test is kept. `mark` stops at derivations (never
/// forces their internals), and each recursed leaf is wrapped in `tryEval` so one
/// test that throws when forced is passed through untouched to error on its own —
/// the per-leaf isolation nix-eval-jobs would otherwise give the untransformed
/// tree. The package itself still evaluates under the profile config, so a
/// meta-blocked *package* throws when forced here and drops all its tests for free.
fn build_tests_expr(
    repo: &Path,
    rev: &str,
    system: &str,
    attrs: &[String],
    profile: Profile,
) -> String {
    let list = nix_string_list(attrs);
    let nixbool = |b| if b { "true" } else { "false" };
    const TEMPLATE: &str = r#"
let
  pkgs = @PKGS@;
  lib = pkgs.lib;
  host = pkgs.stdenv.hostPlatform;
  attrs = [ @ATTRS@];
  # Drop tests unavailable under the run's profile (see build_tests_expr doc),
  # recursing through `tests` sub-attrsets. Stops at derivations (never forces
  # their internals); each recursed leaf goes through `tryEval`, so a test that
  # throws when forced is passed through untouched to error on its own.
  mark = t:
    if lib.isDerivation t then
      (if (!(lib.meta.availableOn host t) && !@ALLOW_UNSUP@)
          || ((t.meta.knownVulnerabilities or [ ]) != [ ] && !@ALLOW_INSEC@)
       then { }
       else t)
    else if lib.isAttrs t then
      lib.mapAttrs (_: v: let r = builtins.tryEval (mark v); in if r.success then r.value else v) t
      // { recurseForDerivations = true; }
    else t;
  node = name: {
    recurseForDerivations = true;
    # Forced per-attr in a nix-eval-jobs worker: a package that fails to evaluate
    # errors only its own `<pkg>.tests`, never the whole run.
    tests =
      let
        pkg = lib.attrByPath (lib.splitString "." name) null pkgs;
        t = if pkg == null then null else (pkg.tests or null);
      in
        if lib.isDerivation t || lib.isAttrs t then mark t
        else { recurseForDerivations = true; };
  };
in
lib.listToAttrs (map (name: lib.nameValuePair name (node name)) attrs)
// { recurseForDerivations = true; }
"#;
    TEMPLATE
        .replace(
            "@PKGS@",
            &build_expr(repo, rev, system, &profile_config(profile)),
        )
        .replace("@ATTRS@", &list)
        .replace("@ALLOW_UNSUP@", nixbool(profile.unsupported))
        .replace("@ALLOW_INSEC@", nixbool(profile.insecure))
}

/// Evaluate the `passthru.tests` of several `(commit, system, packages)`
/// requests **together**, through one shard scheduler — one shared queue,
/// cross-key load balancing — after all eval finishes. `nodes` are the `tests`
/// leaves the caller already created per-system as each platform's eval landed
/// (parallel to `requests`; DESIGN §9), which this drives to running/done.
/// Returns the resolved [`TestJob`]s per request, parallel to `requests` (one
/// `<pkg>.tests.<name>` per job). Callers pass only the packages not already
/// cached (see `main`); an empty/all-empty `requests` does no work.
///
/// **The scheduling atom is the `(commit, system)` key — one shard per request,
/// never sub-sliced *up front* — exactly like [`instantiate_execute`] and for the
/// same reason (DESIGN §6).** Both phases share the cost structure: the dominant
/// cost is the per-key nixpkgs-spine re-import, and the changed set is only a
/// handful of packages, so splitting a key's packages across shards would just
/// re-pay that import per shard while multiplying the concurrent heavy workers.
/// And here each worker is *heavy* — a `nixosTest` ≈ a whole NixOS system — so
/// that oversubscribes RAM: the old `total/(2·slots)` sub-slicing started
/// `2·slots` workers and cascaded into OOM, then requeued a fat shard forever
/// once slots bottomed out at 1 (the shard, not the concurrency, was the
/// memory-bearing unit AIMD couldn't shrink). With the key as the atom, backing
/// off the slot count directly backs off concurrent heavy workers, and each key's
/// single worker recycles its heap once it passes the restart cap. Concurrency is
/// across keys (only ~2 per system), started at the heavy-worker budget
/// ([`TESTS_SLOT_MEM_MB`]) — which is also why splitting is what the *recovery*
/// ladder reaches for at one slot ([`run_shards`]): with so few keys, shedding
/// slots runs out of room long before the memory does. Splitting up front was the
/// mistake; splitting in response to a death, with no concurrency left to
/// multiply, is the only lever with range.
///
/// It also **instantiates as it goes** (below): these are the same drvs the
/// changed test rows will need materialized, so writing each `.drv` here costs
/// store I/O on an evaluation just performed, where deferring it to
/// [`instantiate_execute`] costs that whole `nixosTest` evaluation a second
/// time.
pub fn eval_tests(
    repo: &Path,
    requests: &[(Rev, String, Vec<String>)],
    nodes: Vec<Arc<live::Node>>,
    profile: Profile,
    handle: live::LiveHandle<'_>,
) -> Result<Vec<Vec<TestJob>>> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let slots = default_slots(TESTS_SLOT_MEM_MB);
    // One shard per key: sizing at the largest request makes every group exactly
    // one shard (no split), so each key re-imports nixpkgs just once.
    let shard_size = requests
        .iter()
        .map(|(_, _, p)| p.len())
        .max()
        .unwrap_or(1)
        .max(1);

    // The `tests` leaves were created per-system as each platform's eval landed
    // (DESIGN §9), so `nodes` is parallel to `requests`; execution is still one
    // grouped scheduler run, after all eval.
    let labels: Vec<String> = requests
        .iter()
        .map(|(rev, system, _)| format!("{} {system}", rev.display))
        .collect();
    let items: Vec<Vec<String>> = requests.iter().map(|(_, _, p)| p.clone()).collect();
    let meta: Vec<(&Rev, &str)> = requests.iter().map(|(r, s, _)| (r, s.as_str())).collect();
    let results: Vec<Mutex<Vec<TestJob>>> = (0..requests.len())
        .map(|_| Mutex::new(Vec::new()))
        .collect();

    run_shards(
        "tests",
        nodes,
        labels,
        items,
        shard_size,
        slots,
        // The count is streamed test *jobs*, not the package count, so `items`
        // is not a meaningful denominator — show a bare count.
        false,
        handle,
        |gi, label, pkgs, on_item| {
            let (rev, system) = meta[gi];
            let expr = build_tests_expr(repo, &rev.commit, system, pkgs, profile);
            stream_jobs(
                &expr,
                1,
                DEFAULT_WORKER_MEM_MB,
                // Instantiate as we go: these are the drvs the changed test rows
                // will need materialized, and writing each `.drv` now costs the
                // store I/O on a `nixosTest` evaluation just performed, where
                // leaving it to the `instantiate` phase costs that whole
                // evaluation again (DESIGN §6). No cached fact changes —
                // `--no-instantiate` never altered a `drvPath`.
                true,
                label,
                raw_to_test_job,
                || on_item(1),
            )
        },
        |gi, rows| {
            results[gi].lock().unwrap().extend(rows);
            Ok(())
        },
        // No salvage: a `test_pkg` marker written for a package whose test list
        // only half streamed would cache a truncated set at that key forever, and
        // telling the complete packages from the frontier one means leaning on
        // nix-eval-jobs' dispatch order (DESIGN §6). The key is re-run whole.
        None,
    )?;

    Ok(results
        .into_iter()
        .map(|m| m.into_inner().unwrap())
        .collect())
}

// --- scheduling: one queue of shards (DESIGN §6) ------------------------------

/// Default per-worker heap *cap* — the `--max-memory-size` a worker may reach
/// before nix-eval-jobs restarts it. Kept at nix-eval-jobs' 4 GiB default so a
/// giant subtree (haskellPackages ≈ 3–4 GiB) doesn't trip a restart mid-shard
/// and thrash on re-imports. Distinct from the slot-count budget below.
const DEFAULT_WORKER_MEM_MB: u64 = 4096;

/// RAM budget per slot for the **full-set eval**, used only to *count* the
/// starting slots (see [`eval_slots`]) — not a memory cap. A typical shard's
/// worker holds only ~1–1.5 GiB; just the few giant subtrees spike toward the
/// 4 GiB cap. So counting slots at the cap badly under-parallelizes (a 31 GiB box
/// got 7 workers when it had 18 cores); ~2 GiB matches the measured best worker
/// counts across 62/31/16 GiB machines, and AIMD backs off if a run overshoots
/// RAM. The two targeted phases are heavier per job and use
/// [`TESTS_SLOT_MEM_MB`].
const SLOT_MEM_MB: u64 = 2048;

/// Per-slot RAM budget for the two targeted evals — `--tests` and
/// [`instantiate_execute`], which instantiates what `--tests` did not — distinct
/// from [`SLOT_MEM_MB`] because their workers are far heavier: each `nixosTest`
/// pulls in a whole NixOS system, so a worker genuinely reaches the
/// [`DEFAULT_WORKER_MEM_MB`] restart cap instead of sitting well under it like a
/// full-set worker. Counting such a slot at the *typical* full-set footprint
/// (2 GiB) is what started 15 heavy workers on a 31 GiB box and cascaded into
/// OOM. Budgeting at the cap starts a memory-safe count (~7 on that box) — and is
/// still optimistic, since the cap is a recycle *threshold*: a measured lone
/// worker peaked at ~5.3 GiB against it (DESIGN §6). In these phases the atom
/// count (≤2 shards per system) usually binds before either figure does.
const TESTS_SLOT_MEM_MB: u64 = DEFAULT_WORKER_MEM_MB;

/// Top-level attr names per shard. Larger shards amortize the per-job nixpkgs
/// import (a few seconds each); smaller ones requeue more cheaply and balance
/// better. Measured across all three RAM sizes, ~800–1600 is a flat best (fewer
/// redundant imports, peak still bounded by the RAM ceiling); the old 400 left
/// 20–30% on the table.
const NAMES_PER_SHARD: usize = 1024;

/// A fatal `nix-eval-jobs` abort (non-zero exit): the streamed output was
/// truncated and discarded. The scheduler recognizes it through the anyhow chain
/// to drive its recovery ladder ([`run_shards`]), and it carries the *diagnostic*
/// rather than a marker, because an abort is **not** always an out-of-memory kill
/// — `nix-eval-jobs` also exits non-zero on a fatal eval error (a
/// `StackOverflowError` job is emitted and then kills the run). So a note
/// summarizes what the child actually reported ([`EvalAborted::summary`]) and the
/// give-up path surfaces the whole tail, instead of either one guessing.
#[derive(Debug)]
struct EvalAborted {
    /// The child's exit status, rendered (`exit status: 1`, `signal: 9 (SIGKILL)`).
    status: String,
    /// How many jobs had streamed before the output was cut off.
    streamed: usize,
    /// The tail of the child's stderr (its ring buffer, `stream_jobs`).
    stderr_tail: String,
}

impl EvalAborted {
    /// A one-line cause for a progress note: the exit status plus the last
    /// non-blank line of stderr, which for a worker death is `nix-eval-jobs`' own
    /// verdict ("evaluation worker got killed by SIGKILL, maybe memory limit
    /// reached?", "possible infinite recursion", …).
    fn summary(&self) -> String {
        match self
            .stderr_tail
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
        {
            Some(last) => format!("{} — {}", self.status, last.trim()),
            None => self.status.clone(),
        }
    }
}

impl std::fmt::Display for EvalAborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} after streaming {} job(s), so the result is truncated and will \
             NOT be cached. A worker died; the exit status above and this stderr \
             tail are what it reported, not a guess:\n{}",
            self.status, self.streamed, self.stderr_tail
        )
    }
}

impl std::error::Error for EvalAborted {}

/// The memory ceiling npb plans slots from: total physical RAM, further capped
/// by any cgroup limit the process runs under (a container, or a systemd
/// `MemoryMax=` scope). Unlike *available* RAM (which the old planner used,
/// and which lies — it moves while a minutes-long eval runs), both are
/// **configured promises about the execution environment**, not measurements
/// of a race: physical RAM via `/proc/meminfo MemTotal` (Linux) or
/// `sysctl hw.memsize` (macOS, where cgroups don't exist and the cap is a
/// no-op); the cgroup ceiling via [`cgroup_mem_limit_mb`]. If an admin edits a
/// limit mid-run, the requeue feedback covers it like any other dynamic
/// effect. Fallback 8 GiB.
fn total_mem_mb() -> u64 {
    let physical = physical_mem_mb();
    match cgroup_mem_limit_mb() {
        Some(limit) => physical.min(limit),
        None => physical,
    }
}

/// The tightest cgroup-v2 memory ceiling over this process's ancestry, in MiB
/// — both `memory.max` (the OOM kill line) and `memory.high` (the reclaim
/// throttle, just as bad for throughput). `None` when unlimited or off-Linux.
fn cgroup_mem_limit_mb() -> Option<u64> {
    let cg = fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = cg.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    let root = Path::new("/sys/fs/cgroup");
    let mut dir = PathBuf::from(format!("/sys/fs/cgroup{rel}"));
    let mut min: Option<u64> = None;
    while dir.starts_with(root) && dir != root {
        for f in ["memory.max", "memory.high"] {
            // The unlimited value is the literal string "max": parse fails, skip.
            if let Ok(s) = fs::read_to_string(dir.join(f))
                && let Ok(bytes) = s.trim().parse::<u64>()
            {
                min = Some(min.map_or(bytes, |m| m.min(bytes)));
            }
        }
        if !dir.pop() {
            break;
        }
    }
    min.map(|b| b / 1024 / 1024)
}

fn physical_mem_mb() -> u64 {
    if let Ok(s) = fs::read_to_string("/proc/meminfo")
        && let Some(kb) = s
            .lines()
            .find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|n| n.parse::<u64>().ok())
    {
        return kb / 1024;
    }
    Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|bytes| bytes / 1024 / 1024)
        .unwrap_or(8192)
}

/// The number of concurrent shard jobs to start with, bounded by the machine's
/// *invariants* — one worker per slot, so cores, and total RAM divided by a
/// per-slot budget ([`SLOT_MEM_MB`], ~2 GiB — the *typical* worker footprint,
/// deliberately below the 4 GiB restart cap since only the few giant subtrees
/// approach it). The dynamic part of RAM is handled by feedback, not planning:
/// the queue sheds slots when a shard is OOM-killed ([`eval_pairs`]).
fn eval_slots(cores: usize, mem_mb: u64, per_slot_mb: u64) -> usize {
    cores.min((mem_mb / per_slot_mb.max(1)).max(1) as usize)
}

/// [`eval_slots`] wired to this machine's invariants — the starting slot count
/// every scheduler run uses. Callers pass the per-slot budget for their
/// workload: [`SLOT_MEM_MB`] for the light full-set walk, [`TESTS_SLOT_MEM_MB`]
/// for the two heavy targeted evals. `eval_slots` stays a standalone pure fn so
/// its unit test can pin the arithmetic.
fn default_slots(per_slot_mb: u64) -> usize {
    let cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    eval_slots(cores, total_mem_mb(), per_slot_mb)
}

/// What a phase's commit leaves show in the number column: nothing (a state
/// color only, e.g. `enumerate`, which has no meaningful count to tick), a plain
/// count (`tests`; or `count / total` when [`run_shards`] is told the total, e.g.
/// `instantiate`), or a dim `NN%` shard-progress readout (`evaluate`).
#[derive(Clone, Copy)]
enum Leaf {
    None,
    Count,
    Percent,
}

/// Build a phase subtree in `tree`: the phase node, then a system level, then the
/// per-side commit `display` leaves — returning the leaf handles in `groups` order
/// (parallel to the scheduler's groups, one `(system, display)` each). The system
/// level is always present, one system or many (DESIGN §6). `leaf` picks the
/// leaves' number-column kind.
fn add_phase(
    tree: &live::Tree,
    phase: &str,
    groups: &[(String, String)],
    leaf: Leaf,
) -> Vec<Arc<live::Node>> {
    tree.node(phase, 0);
    let make = |disp: String, depth: usize| match leaf {
        Leaf::None => tree.node(disp, depth),
        Leaf::Count => tree.counter(disp, depth, -1),
        Leaf::Percent => tree.percent(disp, depth),
    };
    let mut handles: Vec<Option<Arc<live::Node>>> = vec![None; groups.len()];
    // Distinct systems in first-seen order; each side's commit nests under it.
    let mut order: Vec<&str> = Vec::new();
    for (s, _) in groups {
        if !order.contains(&s.as_str()) {
            order.push(s);
        }
    }
    for s in order {
        tree.node(s.to_string(), 1);
        for (gi, (gs, disp)) in groups.iter().enumerate() {
            if gs == s {
                handles[gi] = Some(make(disp.clone(), 2));
            }
        }
    }
    handles.into_iter().map(Option::unwrap).collect()
}

/// The top-level attr names of the package set at `(commit, system)` — the
/// space the shards partition. Cheap (well under a second warm): forcing
/// `attrNames` touches no derivations. The literal `recurseForDerivations`
/// key is dropped to mirror how `nix-eval-jobs` skips it when walking a set.
fn enumerate_names(repo: &Path, rev: &str, system: &str, config: &str) -> Result<Vec<String>> {
    let expr = format!(
        "builtins.attrNames ({})",
        build_expr(repo, rev, system, config)
    );
    let out = scrub_env(
        Command::new("nix-instantiate").args(["--eval", "--strict", "--json", "-E", &expr]),
    )
    .output()
    .context("running nix-instantiate (attr names)")?;
    if !out.status.success() {
        bail!(
            "enumerating top-level attrs of {rev} ({system}) failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let mut names: Vec<String> =
        serde_json::from_slice(&out.stdout).context("parsing attr names")?;
    names.retain(|n| n != "recurseForDerivations");
    Ok(names)
}

/// The eval expression for one shard: the same import as the whole-set walk,
/// narrowed to `names` via `listToAttrs`. Each value stays a thunk forced
/// per-attr in the worker, so walk semantics and error isolation match the
/// monolithic root exactly — validated byte-for-byte against a whole-set eval
/// (DESIGN §6).
fn shard_expr(repo: &Path, rev: &str, system: &str, names: &[String], config: &str) -> String {
    let list = nix_string_list(names);
    format!(
        "let pkgs = {}; in builtins.listToAttrs \
         (map (n: {{ name = n; value = pkgs.${{n}}; }}) [ {list}])",
        build_expr(repo, rev, system, config)
    )
}

/// A job expression selecting exactly `paths` out of the package set — each an
/// attr path, possibly dotted/nested (`python3Packages.foo`, or a test path like
/// `grafana.tests.grafana.basic`). One job per path, forced per-attr in the
/// worker, so a path that no longer resolves errors only itself.
///
/// TODO(nix-eval-jobs#412): this hand-rolled selector — and the identical
/// `splitString "."` trick in [`build_tests_expr`] — is the wrapper-expr
/// workaround that a native `--select <attrpath>` (emitting the literal selector
/// as `attr`) would replace. `splitString "."` also mis-splits a quoted path
/// element like `haskell.compiler."ghc94"`, which `--select` would handle
/// correctly. Adopt it (in both spots) once it lands upstream.
fn select_expr(repo: &Path, rev: &str, system: &str, paths: &[String], config: &str) -> String {
    let list = nix_string_list(paths);
    format!(
        "let pkgs = {}; lib = pkgs.lib; in builtins.listToAttrs \
         (map (p: {{ name = p; value = lib.attrByPath (lib.splitString \".\" p) null pkgs; }}) [ {list}])",
        build_expr(repo, rev, system, config)
    )
}

/// A prepared instantiate: its (blue) tree nodes and the non-empty requests they
/// pair with, from [`instantiate_prepare`], ready for [`instantiate_execute`].
pub struct Instantiate {
    requests: Vec<InstRequest>,
    nodes: Vec<Arc<live::Node>>,
}

/// One side to instantiate: which attr paths, and the drv each one evaluates to.
/// The drvs are what make an aborted pass recoverable — they are the phase's
/// durable record, so `run_shards` can ask the store which recipes a dead worker
/// had already written and retry only the rest (DESIGN §6).
pub struct InstRequest {
    pub rev: Rev,
    pub system: String,
    /// `(attr path, its drv path)`, in the order the phase will request them.
    pub attrs: Vec<(String, String)>,
}

/// Create the `instantiate` phase's (blue) nodes for the requests with something
/// to do, returning them paired with those requests. Split from execution so the
/// caller can reveal the `probe` node — which sorts *below* `instantiate` — while
/// this phase is still only blue, so both appear at once (DESIGN §9). `None` when
/// there's nothing to instantiate.
pub fn instantiate_prepare(tree: &live::Tree, requests: Vec<InstRequest>) -> Option<Instantiate> {
    // Drop the sides with nothing to instantiate (a diff side can have no
    // buildable changed attrs) so they don't clutter the display.
    let requests: Vec<InstRequest> = requests
        .into_iter()
        .filter(|r| !r.attrs.is_empty())
        .collect();
    if requests.is_empty() {
        return None;
    }
    let groups: Vec<(String, String)> = requests
        .iter()
        .map(|r| (r.system.clone(), r.rev.display.clone()))
        .collect();
    let nodes = add_phase(tree, "instantiate", &groups, Leaf::Count);
    Some(Instantiate { requests, nodes })
}

/// Write the changed set's `.drv` files to the store (DESIGN §6). npb's evals run
/// with `--no-instantiate` (drvPath + outputs only — no `.drv` writes for the
/// ~114k attrs it never builds), so the drvs the build and the narinfo probe
/// actually touch — the small changed set — are materialized here, one
/// `nix-eval-jobs` run per `(commit, system)` with instantiation on. Streamed
/// rows are discarded; the store write is the point. Runs only when about to
/// build.
///
/// The per-pair runs go through the **same shard scheduler** as the two eval
/// paths (`run_shards`) so they run concurrently and get the identical live
/// display — a fresh multi-system run would otherwise sit silent through six
/// serial nixpkgs re-imports (base+head × the systems). Each request is **one
/// shard** (no sub-slicing): the whole cost here is the per-run nixpkgs import,
/// so splitting a request's handful of changed attrs across shards would only
/// re-pay that import for no gain. Concurrency is what wins — the phase's
/// wall-time drops from the *sum* of the imports to (roughly) the *slowest*
/// one, up to `slots` at a time, at no extra total work.
pub fn instantiate_execute(
    repo: &Path,
    inst: Instantiate,
    profile: Profile,
    handle: live::LiveHandle<'_>,
    absent: &(dyn Fn(&[String]) -> Result<HashSet<String>> + Sync),
) -> Result<()> {
    let Instantiate { requests, nodes } = inst;
    // Budgeted at the heavy-worker figure like `--tests`, not the light full-set
    // one: a residual pass (a package whose tests another review cached at this
    // tree, its `.drv` since collected) still evaluates `nixosTest`s. In practice
    // the atom count — one shard per side, so ≤2 per system — binds first.
    let slots = default_slots(TESTS_SLOT_MEM_MB);
    let labels: Vec<String> = requests
        .iter()
        .map(|r| format!("{} {}", r.rev.display, r.system))
        .collect();
    let items: Vec<Vec<String>> = requests
        .iter()
        .map(|r| r.attrs.iter().map(|(attr, _)| attr.clone()).collect())
        .collect();
    // A shard per request: sizing at the largest request makes every group
    // exactly one shard, so each pair re-imports nixpkgs just once — until an
    // abort splits one (`run_shards`' ladder), which is the whole point of paying
    // that import per shard rather than looping on a slice that doesn't fit.
    let shard_size = items.iter().map(Vec::len).max().unwrap_or(1).max(1);
    // Per group, `attr -> drv`, for the abort re-check below.
    let drvs: Vec<HashMap<&str, &str>> = requests
        .iter()
        .map(|r| {
            r.attrs
                .iter()
                .map(|(a, d)| (a.as_str(), d.as_str()))
                .collect()
        })
        .collect();

    run_shards(
        "instantiate",
        nodes,
        labels,
        items,
        shard_size,
        slots,
        // One streamed job per requested drv, so `items` is the total.
        true,
        handle,
        |gi, label, paths, on_item| {
            let r = &requests[gi];
            let expr = select_expr(
                repo,
                &r.rev.commit,
                &r.system,
                paths,
                &profile_config(profile),
            );
            // Streamed rows are discarded (mapped to `()`); the `.drv` writes are
            // the point. The per-job callback drives the live count.
            stream_jobs(
                &expr,
                1,
                DEFAULT_WORKER_MEM_MB,
                true,
                label,
                |_| (),
                || on_item(1),
            )
        },
        |_, _| Ok(()),
        // The durable record: a `.drv` a dead worker had already written is in the
        // store and content-addressed, hence as good as one this pass wrote (§6),
        // so a retry owes only the attrs whose recipe is still missing.
        Some(&|gi: usize, paths: &[String]| {
            let wanted: Vec<String> = paths
                .iter()
                .filter_map(|a| drvs[gi].get(a.as_str()).map(|d| (*d).to_string()))
                .collect();
            let missing = absent(&wanted)?;
            Ok(paths
                .iter()
                .filter(|a| {
                    drvs[gi]
                        .get(a.as_str())
                        .is_none_or(|d| missing.contains(*d))
                })
                .cloned()
                .collect())
        }),
    )
}

// --- the shard scheduler (shared by the full-set eval and the --tests eval) ---

/// One group of shards run together: one leaf node in the progress tree, one
/// assembled result. Its items (top-level names for the full eval, changed
/// packages for `--tests`) are sliced into shards, which own them from then on;
/// the shard counters drive the AIMD scheduler, while progress is reflected onto
/// `node` for the display.
struct ShardGroup<T> {
    node: Arc<live::Node>,
    items_total: usize,
    /// Shards this group is split into — *mutable*, because an abort at one slot
    /// splits a shard in two ([`run_shards`]), and the completion check (and a
    /// percent node's denominator) must follow.
    shards_total: AtomicUsize,
    shards_done: AtomicUsize,
    /// Set when a shard completed by re-derivation rather than by streaming its
    /// rows (`instantiate`'s salvage, DESIGN §6): the streamed tally then
    /// understates the work actually done, so the node's count is pinned to the
    /// item total instead of to the assembled rows.
    salvaged: AtomicBool,
    rows: Mutex<Vec<T>>,
}

/// A queued unit of work: some of one group's items. Owned (not a range into the
/// group) so a retry can carry an arbitrary *subset* — the still-absent recipes
/// after an abort, or one half of a shard being split.
struct Shard {
    group: usize,
    items: Vec<String>,
}

/// What an abort does next — the recovery ladder of [`run_shards`] as a pure
/// decision over three numbers: how many of the shard's items a retry still owes
/// (`left`), how many it had (`items`), and the current slot count. Kept
/// standalone so the rung *ordering* and — the property that matters — the
/// ladder's **bottom** are unit-testable without a scheduler or a Nix store.
#[derive(Debug, PartialEq, Eq)]
enum Recovery {
    /// Rung 0, best case: the dead pass had already produced everything this
    /// shard owed, so book it done (`instantiate`'s salvage).
    Done,
    /// Rung 0: it produced *some* of it. Retry just the remainder — real progress,
    /// so nothing backs off. Strictly smaller each time, hence not a cycle.
    Retry,
    /// Rung 1: halve the slot count (from, to) and retry the shard as-is.
    Slots(usize, usize),
    /// Rung 2: out of concurrency to shed — split the shard at this index.
    Split(usize),
    /// Rung 3: one item, one slot, still dead. Report it.
    GiveUp,
}

fn recovery(left: usize, items: usize, slots: usize) -> Recovery {
    if left == 0 {
        Recovery::Done
    } else if left < items {
        Recovery::Retry
    } else if slots > 1 {
        Recovery::Slots(slots, (slots / 2).max(1))
    } else if items > 1 {
        Recovery::Split(items / 2)
    } else {
        Recovery::GiveUp
    }
}

/// A phase's **durable-record** hook: given a group and a shard's items, which of
/// them a retry still owes after an abort (DESIGN §6). Only `instantiate` has one
/// — it re-queries store validity — so [`run_shards`] takes it as an `Option`,
/// where `None` reads as "nothing a partial pass leaves behind is salvageable".
type Remaining<'a> = dyn Fn(usize, &[String]) -> Result<Vec<String>> + Sync + 'a;

/// Run a set of shard groups through one bounded, AIMD-controlled worker pool,
/// reflecting progress onto each group's [`live::Node`] in the shared tree
/// (DESIGN §6). Shared by the full-set, tests, and instantiate paths. `nodes`
/// gives the leaf node per group (parallel to `items`); `labels` names each
/// group for error messages; `known_total` sets the node's denominator to
/// `items.len()` — true when one streamed row == one item (evaluate,
/// instantiate), false for enumerate (which discovers its count) and tests
/// (whose count is streamed jobs, not packages). Persistence is the caller's job
/// via the closures (the full eval assembles a flat file, `--tests` returns
/// rows; DESIGN §4); this owns only the scheduling and the node updates. The
/// outer `with_live` in `run` owns the refresher that redraws the tree.
///
/// `eval_shard(group, label, items, on_item)` evaluates one shard's items to its
/// rows, calling `on_item(n)` as items surface (bumps the node count); it may
/// return an [`EvalAborted`] error to put the shard on the recovery ladder
/// below, or any other error to fail the whole run. `on_group_complete` fires the
/// moment a group's last shard lands, with the group's assembled rows.
///
/// **The recovery ladder (DESIGN §6).** An abort means a `nix-eval-jobs` worker
/// died, so its stream was truncated and the shard's rows are gone. What to do
/// about it is a finite sequence, tried in order — finite so the loop terminates
/// by construction, and ordered so each step makes the retry *different work*
/// rather than a re-roll of the same dice:
///
/// 0. **Re-derive what is still outstanding** through `remaining`, the phase's
///    durable-record hook. `None` (`evaluate`, `--tests`) means nothing a partial
///    pass leaves behind is salvageable, so every item is still owed.
///    `instantiate` re-queries store validity, where an aborted pass's `.drv`
///    writes survive: an empty answer completes the shard outright, and a smaller
///    one is requeued as-is — real progress, so no back-off at all.
/// 1. **Halve the slot count** (AIMD multiplicative decrease), while > 1.
/// 2. **Halve the shard**, once slots are at 1 and there is more than one item.
///    The reasons not to sub-slice a key are about the happy path (re-paid
///    imports, multiplied heavy workers); neither applies to one serial worker
///    responding to a failure, where a smaller slice can only lower the peak.
/// 3. **Fail, naming the item**, when a single item still aborts — surfacing
///    `nix-eval-jobs`' own diagnostic, since a machine that cannot evaluate it
///    will not evaluate it on the next attempt either.
#[allow(clippy::too_many_arguments)]
fn run_shards<T: Send>(
    phase: &str,
    nodes: Vec<Arc<live::Node>>,
    labels: Vec<String>,
    items: Vec<Vec<String>>,
    shard_size: usize,
    slots: usize,
    known_total: bool,
    handle: live::LiveHandle<'_>,
    eval_shard: impl Fn(usize, &str, &[String], &(dyn Fn(usize) + Sync)) -> Result<Vec<T>> + Sync,
    on_group_complete: impl Fn(usize, Vec<T>) -> Result<()> + Sync,
    remaining: Option<&Remaining<'_>>,
) -> Result<()> {
    let shard_size = shard_size.max(1);
    let mut queue: VecDeque<Shard> = VecDeque::new();
    let groups: Vec<ShardGroup<T>> = nodes
        .into_iter()
        .zip(items)
        .enumerate()
        .map(|(gi, (node, items))| {
            let items_total = items.len();
            if known_total {
                node.set_total(items_total as i64);
            }
            let mut shards_total = 0;
            for chunk in items.chunks(shard_size) {
                queue.push_back(Shard {
                    group: gi,
                    items: chunk.to_vec(),
                });
                shards_total += 1;
            }
            // Set the shard denominator up front so a percent node's `NN%` is
            // correct from the first frame (not 100% until the first shard lands).
            node.set_shards_total(shards_total);
            ShardGroup {
                node,
                items_total,
                shards_total: AtomicUsize::new(shards_total),
                shards_done: AtomicUsize::new(0),
                salvaged: AtomicBool::new(false),
                rows: Mutex::new(Vec::new()),
            }
        })
        .collect();

    if queue.is_empty() {
        return Ok(());
    }
    // No point in more workers than shards.
    let slots = slots.min(queue.len());

    struct Q {
        queue: VecDeque<Shard>,
        /// Shards not yet completed (queued or running); requeues don't count
        /// down, so workers only exit when everything truly finished (or fatal).
        outstanding: usize,
        fatal: Option<anyhow::Error>,
    }
    let q = Mutex::new(Q {
        outstanding: queue.len(),
        queue,
        fatal: None,
    });
    // AIMD over the slot count: halve on an abort (multiplicative decrease), +1
    // back toward the starting value per few clean shards (additive increase).
    let target = AtomicUsize::new(slots);
    let successes = AtomicUsize::new(0);

    // The worker pool. Workers only bump atomics / node state and, on a requeue,
    // emit a note above the tree via `handle`; the refresher (owned by the outer
    // `with_live` in `run`) redraws the tree off the node atomics every 100 ms.
    thread::scope(|s| {
        for w in 0..slots {
            let (q, target, successes, groups, labels, eval_shard, on_group_complete) = (
                &q,
                &target,
                &successes,
                &groups,
                &labels,
                &eval_shard,
                &on_group_complete,
            );
            s.spawn(move || {
                loop {
                    let shard = {
                        let mut g = q.lock().unwrap();
                        if g.fatal.is_some() || g.outstanding == 0 {
                            return;
                        }
                        // Parked slots (w >= target) and an empty-but-not-done
                        // queue both just wait: an aborted shard may requeue.
                        if w < target.load(Ordering::Relaxed) {
                            g.queue.pop_front()
                        } else {
                            None
                        }
                    };
                    let Some(shard) = shard else {
                        thread::sleep(Duration::from_millis(200));
                        continue;
                    };
                    let g = &groups[shard.group];
                    let label = &labels[shard.group];
                    let set_fatal = |err: anyhow::Error| {
                        let mut lock = q.lock().unwrap();
                        if lock.fatal.is_none() {
                            lock.fatal = Some(err);
                        }
                    };

                    g.node.set_running();
                    g.node.shard_started();
                    let outcome = (|| -> Result<()> {
                        let on_item = |n: usize| g.node.stream(n as i64);
                        let rows = eval_shard(shard.group, label, &shard.items, &on_item)?;
                        finish_shard(g, shard.group, rows, &on_group_complete)
                    })();
                    g.node.shard_finished();

                    match outcome {
                        Ok(()) => {
                            q.lock().unwrap().outstanding -= 1;
                            let n = successes.fetch_add(1, Ordering::Relaxed) + 1;
                            let t = target.load(Ordering::Relaxed);
                            if n % 4 == 0 && t < slots {
                                target.store(t + 1, Ordering::Relaxed);
                            }
                        }
                        // Any error that isn't an abort fails the whole run.
                        Err(e) if e.downcast_ref::<EvalAborted>().is_none() => set_fatal(e),
                        // An abort walks the recovery ladder (see this fn's docs).
                        Err(e) => {
                            let ab = e
                                .downcast_ref::<EvalAborted>()
                                .expect("the arm above matched every non-abort error");
                            successes.store(0, Ordering::Relaxed);
                            let n = shard.items.len();
                            // Rung 0: what does this shard still owe? A phase with a
                            // durable record answers from it; the others owe it all.
                            // Each shrinking answer requeues a strictly smaller
                            // shard, so this can't cycle: it reaches empty (done) or
                            // unchanged (the ladder proper, below).
                            let left = match remaining {
                                Some(f) => match f(shard.group, &shard.items) {
                                    Ok(left) => left,
                                    Err(err) => {
                                        set_fatal(err.context(
                                            "re-checking what an aborted shard still owed",
                                        ));
                                        continue;
                                    }
                                },
                                None => shard.items.clone(),
                            };
                            let cause = ab.summary();
                            match recovery(left.len(), n, target.load(Ordering::Relaxed)) {
                                Recovery::Done => {
                                    handle.note(&format!(
                                        "  {phase}: {label} aborted ({cause}) — but all \
                                         {n} had already landed, so there is nothing \
                                         left to redo",
                                    ));
                                    g.salvaged.store(true, Ordering::Relaxed);
                                    if let Err(err) =
                                        finish_shard(g, shard.group, Vec::new(), &on_group_complete)
                                    {
                                        set_fatal(err);
                                        continue;
                                    }
                                    q.lock().unwrap().outstanding -= 1;
                                }
                                Recovery::Retry => {
                                    handle.note(&format!(
                                        "  {phase}: {label} aborted ({cause}); {} of {n} \
                                         had landed, retrying the remaining {}",
                                        n - left.len(),
                                        left.len(),
                                    ));
                                    q.lock().unwrap().queue.push_back(Shard {
                                        group: shard.group,
                                        items: left,
                                    });
                                }
                                Recovery::Slots(from, to) => {
                                    target.store(to, Ordering::Relaxed);
                                    handle.note(&format!(
                                        "  {phase}: {label} aborted ({cause}); requeued, \
                                         slots {from} -> {to}",
                                    ));
                                    q.lock().unwrap().queue.push_back(shard);
                                }
                                Recovery::Split(at) => {
                                    // The two halves replace this shard, so the group
                                    // gains one shard (and a percent node its
                                    // denominator); the queue mutex publishes the new
                                    // count and the pushes together, so a worker that
                                    // picks up a half sees both.
                                    let mut items = shard.items;
                                    let tail = items.split_off(at);
                                    handle.note(&format!(
                                        "  {phase}: {label} aborted ({cause}) at one \
                                         slot; splitting {n} items into {} + {}",
                                        items.len(),
                                        tail.len(),
                                    ));
                                    let total = g.shards_total.fetch_add(1, Ordering::Relaxed) + 1;
                                    g.node.set_shards_total(total);
                                    let mut lock = q.lock().unwrap();
                                    lock.outstanding += 1;
                                    for items in [items, tail] {
                                        lock.queue.push_back(Shard {
                                            group: shard.group,
                                            items,
                                        });
                                    }
                                }
                                Recovery::GiveUp => set_fatal(e.context(format!(
                                    "{phase}: {label} could not evaluate `{}` — it \
                                     aborted alone, at one slot, with nothing left to \
                                     split or salvage",
                                    shard.items[0],
                                ))),
                            }
                        }
                    }
                }
            });
        }
    });

    match q.into_inner().unwrap().fatal {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Book one shard as done: fold its rows into the group and advance the group's
/// shard progress; on the *last* shard, hand the assembled rows to
/// `on_group_complete` and mark the node done. Reading `shards_total` here is safe
/// under `Relaxed` because a split publishes it before pushing the halves under
/// the queue mutex, so any worker that ran a half acquired that write with it.
///
/// The node's final count is the assembled row count — the streamed tally can
/// drift — *unless* a shard completed by re-derivation (`instantiate`'s salvage),
/// whose work produced no rows here: then the item total is the honest number.
fn finish_shard<T>(
    g: &ShardGroup<T>,
    gi: usize,
    rows: Vec<T>,
    on_group_complete: &(impl Fn(usize, Vec<T>) -> Result<()> + Sync),
) -> Result<()> {
    g.rows.lock().unwrap().extend(rows);
    let done = g.shards_done.fetch_add(1, Ordering::Relaxed) + 1;
    // Advance a percent node's shard-progress readout (a no-op for count-less /
    // plain-count nodes).
    g.node.shard_progress(done);
    if done == g.shards_total.load(Ordering::Relaxed) {
        let rows = std::mem::take(&mut *g.rows.lock().unwrap());
        let count = if g.salvaged.load(Ordering::Relaxed) {
            g.items_total as i64
        } else {
            rows.len() as i64
        };
        on_group_complete(gi, rows)?;
        g.node.group_done(count);
    }
    Ok(())
}

/// Ensure every `(commit, system)` pair has a cached eval file, via **one
/// global queue of shards** (DESIGN §6): each eval's top-level names are
/// split into [`NAMES_PER_SHARD`] slices, and every shard is an independent
/// one-worker `nix-eval-jobs` job. [`eval_slots`] jobs run at once; a shard
/// that aborts (in practice a worker OOM-kill) is simply requeued while the
/// slot count backs off multiplicatively — and creeps back up on sustained
/// success (AIMD). An aborted shard requeues in memory (completed shards' rows
/// are held there too), so an interrupted eval re-runs from scratch rather than
/// resuming; when an eval's last shard lands, its rows are assembled and
/// written as the one cached file.
#[allow(clippy::too_many_arguments)]
pub fn eval_pairs(
    repo: &Path,
    pairs: &[(Rev, String)],
    profile: Profile,
    tree: &live::Tree,
    handle: live::LiveHandle<'_>,
    // Called with a bare `system` the moment one of its eval files lands, so the
    // caller can compute that system's diff and show its `tests` early (DESIGN §9).
    on_eval_done: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    // The nixpkgs config every shard evaluates under, and the profile token that
    // prefixes each eval file's system key on disk (`Profile::qualify`).
    let config = profile_config(profile);
    let mut todo: Vec<usize> = Vec::new();
    // Systems with a cache hit this run — signalled to `on_eval_done` once the
    // eval nodes exist (below), so their `tests` can appear early while the cold
    // systems evaluate, yet still sort under `evaluate` (DESIGN §9).
    let mut cached: Vec<&str> = Vec::new();
    // Dedupe on the eval key `(tree, system)`: `npb X X`, repeated --system, or
    // two revisions sharing a tree would otherwise run the same eval twice
    // concurrently — harmless (the write is atomic) but 2× the work.
    let mut seen = std::collections::HashSet::new();
    for (i, (rev, system)) in pairs.iter().enumerate() {
        let path = eval_path(&rev.tree, &profile.qualify(system))?;
        if path.exists() {
            // A cache hit: mark the file used now so LRU eviction (`--clean`,
            // DESIGN.md §4) keeps a frequently-reused eval (e.g. a shared base)
            // warm rather than judging it by its first-write time.
            crate::evalfile::touch_eval(&path);
            cached.push(system);
        } else if seen.insert((&rev.tree, system)) {
            todo.push(i);
        }
    }
    if todo.is_empty() {
        // Nothing to evaluate — the caller sweeps the (fully-cached) systems
        // itself; we've created no nodes and fire nothing.
        return Ok(());
    }

    // Count slots at the ~2 GiB typical footprint, but each worker keeps the
    // 4 GiB restart cap ([`DEFAULT_WORKER_MEM_MB`]) — the two are deliberately decoupled.
    let slots = default_slots(SLOT_MEM_MB);

    // One shard group per `(tree, system)` for both phases below; `meta` keeps
    // the identifying `(rev, system)` per group (the rev supplies fetchGit's
    // commit; the tree is its cache key), `labels` the error-message name.
    let meta: Vec<(&Rev, &str)> = todo
        .iter()
        .map(|&i| (&pairs[i].0, pairs[i].1.as_str()))
        .collect();
    let labels: Vec<String> = meta
        .iter()
        .map(|(rev, system)| format!("{} {system}", rev.display))
        .collect();
    let groups: Vec<(String, String)> = meta
        .iter()
        .map(|(rev, system)| (system.to_string(), rev.display.clone()))
        .collect();
    // Both phase subtrees are created up front, so `evaluate` shows as waiting
    // (blue) under the same commit displays while `enumerate` runs (DESIGN §6).
    let enum_nodes = add_phase(tree, "enumerate", &groups, Leaf::None);
    let eval_nodes = add_phase(tree, "evaluate", &groups, Leaf::Percent);

    // Now that the eval nodes exist, signal systems already cached this run so
    // their `tests` appear immediately (a side whose other side is still cold is
    // a no-op until that lands — the caller re-checks both files). A cold group
    // signals when it completes (below).
    for system in cached {
        on_eval_done(system);
    }

    // Phase 1: enumerate each pair's top-level attr names — the space phase 2
    // shards. This runs through the *same scheduler*, one shard per pair (the
    // work is a single `builtins.attrNames` call — not a fannable set — so there
    // is nothing to sub-slice): the pairs enumerate concurrently behind the
    // shared live display instead of a bespoke pool. Enumerating a cold commit
    // reads and hashes its whole source tree (a few seconds — even on Nix ≥2.35,
    // where the tree is no longer *copied* into the store, the content-addressed
    // hash still forces a full read); running the pairs concurrently overlaps
    // those hashes instead of summing them (each distinct commit is independent —
    // measured ~2×; Nix's fetcher locks serialize same-commit races, so a warm
    // pair still returns cheaply). Reusing the eval slot count can't oversubscribe
    // RAM — a lone `nix-instantiate` is ~0.5 GB, well under a shard worker's cap.
    let enumerated: Vec<Mutex<Vec<String>>> =
        (0..meta.len()).map(|_| Mutex::new(Vec::new())).collect();
    run_shards(
        "enumerate",
        enum_nodes,
        labels.clone(),
        // One placeholder item per group ⇒ exactly one shard per pair.
        meta.iter().map(|_| vec![String::new()]).collect(),
        1,
        slots,
        // Enumerate discovers its attr count, so there is no denominator to show.
        false,
        handle,
        |gi, _label, _slice, on_item| {
            let (rev, system) = meta[gi];
            let names = enumerate_names(repo, &rev.commit, system, &config)?;
            on_item(names.len());
            Ok(names)
        },
        |gi, names| {
            *enumerated[gi].lock().unwrap() = names;
            Ok(())
        },
        // One `builtins.attrNames` call per pair: nothing partial to salvage, and
        // nothing to split either (its single item is the whole call).
        None,
    )?;
    let items: Vec<Vec<String>> = enumerated
        .into_iter()
        .map(|m| m.into_inner().unwrap())
        .collect();

    // Phase 2: shard-evaluate every pair's enumerated names into its cached file.
    run_shards(
        "evaluate",
        eval_nodes,
        labels,
        items,
        NAMES_PER_SHARD,
        slots,
        // No denominator: `nix-eval-jobs` descends into `recurseForDerivations`
        // sets (haskellPackages, the python sets, …), so it streams far more drvs
        // than there are enumerated top-level names — the enumerated count is not
        // a valid total. Like `enumerate`, show a bare climbing count.
        false,
        handle,
        // Evaluate one shard by streaming its own one-worker `nix-eval-jobs`.
        |gi, label, names, on_item| {
            let (rev, system) = meta[gi];
            let expr = shard_expr(repo, &rev.commit, system, names, &config);
            stream_jobs(
                &expr,
                1,
                DEFAULT_WORKER_MEM_MB,
                false,
                label,
                raw_to_attr_eval,
                || on_item(1),
            )
        },
        // Assemble the eval into its one cached file, keyed on the tree, then
        // signal that this `(tree, system)` is now available — the last side of a
        // system to land lets the caller diff it and reveal its `tests` (§9).
        |gi, rows| {
            let (rev, system) = meta[gi];
            write_eval(&eval_path(&rev.tree, &profile.qualify(system))?, &rows)?;
            on_eval_done(system);
            Ok(())
        },
        // No salvage: a shard's rows live in memory until its eval file is written
        // whole (nothing transient on disk, above), and one top-level name fans
        // out into an unknown number of them, so a truncated stream can't be
        // attributed per item at all. The shard is re-run, split if it must be.
        None,
    )
}

/// Ensure both revisions are evaluated across all systems (they run
/// concurrently). Deduped by their eval key `(tree, system)` in [`eval_pairs`],
/// so a `base`/`head` that share a tree pay for one eval, not two.
#[allow(clippy::too_many_arguments)]
pub fn eval_two(
    repo: &Path,
    base: &Rev,
    head: &Rev,
    systems: &[String],
    profile: Profile,
    tree: &live::Tree,
    handle: live::LiveHandle<'_>,
    on_eval_done: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    // System-major, base-then-head — the order the tree displays them (grouped
    // by system, base above head), so the shard scheduler works through them in
    // that same order rather than all bases first (DESIGN §6).
    let mut pairs: Vec<(Rev, String)> = Vec::with_capacity(systems.len() * 2);
    for s in systems {
        pairs.push((base.clone(), s.clone()));
        pairs.push((head.clone(), s.clone()));
    }
    eval_pairs(repo, &pairs, profile, tree, handle, on_eval_done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stream NDJSON values off `reader`, mapping each to an `AttrEval` — the
    /// same job-parse the production streamer does inline, exercised here over a
    /// fixed buffer rather than a live `nix-eval-jobs` child.
    fn parse_jobs<R: std::io::Read>(reader: R) -> Result<Vec<AttrEval>> {
        let mut out = Vec::new();
        for item in serde_json::Deserializer::from_reader(reader).into_iter::<RawJob>() {
            let raw = item.context("parsing nix-eval-jobs output")?;
            out.push(raw_to_attr_eval(raw));
        }
        Ok(out)
    }

    #[test]
    fn parses_success_and_threw_lines() {
        // Under the profile config, an attr either resolves to a `drvPath` or
        // **threw** — the job line then carries an `error` and no `drvPath`, so
        // `drv_path` is `None`. There is no `meta`. Unknown fields (system,
        // fatal, …) are simply ignored.
        let stdout = concat!(
            r#"{"attr":"hello","attrPath":["hello"],"drvPath":"/nix/store/a-hello.drv","system":"aarch64-linux"}"#,
            "\n",
            r#"{"attr":"bad","attrPath":["bad"],"error":"boom","fatal":false}"#,
            "\n",
        );
        let attrs = parse_jobs(stdout.as_bytes()).unwrap();
        assert_eq!(attrs.len(), 2);

        assert_eq!(attrs[0].attr, "hello");
        assert_eq!(attrs[0].drv_path.as_deref(), Some("/nix/store/a-hello.drv"));

        assert_eq!(attrs[1].attr, "bad");
        assert_eq!(attrs[1].drv_path, None); // threw
    }

    #[test]
    fn profile_config_reflects_the_allow_flags() {
        // Strict allows only unfree; each axis adds exactly its allow-flag.
        let strict = Profile {
            broken: false,
            unsupported: false,
            insecure: false,
        };
        assert_eq!(
            profile_config(strict),
            "{ allowUnfree = true; allowAliases = false; }"
        );
        let all = Profile {
            broken: true,
            unsupported: true,
            insecure: true,
        };
        let c = profile_config(all);
        assert!(c.contains("allowBroken = true"));
        assert!(c.contains("allowUnsupportedSystem = true"));
        assert!(c.contains("allowInsecurePredicate = _: true"));
    }

    #[test]
    fn full_expr_fetches_and_imports() {
        let repo = Path::new("/repo");
        let full = build_expr(repo, "abc123", "aarch64-linux", "{ allowBroken = true; }");
        assert!(full.contains(r#"builtins.fetchGit { url = "/repo"; rev = "abc123"; }"#));
        assert!(full.contains("allowBroken = true"));
    }

    #[test]
    fn eval_slots_from_invariants() {
        const G: u64 = 1024;
        // Core-bound when RAM is plentiful; RAM-bound (total / per-slot budget)
        // when it isn't; never zero.
        // At the default SLOT_MEM_MB (~2 GiB) the three benchmark boxes get:
        assert_eq!(eval_slots(32, 62 * G, SLOT_MEM_MB), 31); // amd64 (core-bound near 32)
        assert_eq!(eval_slots(18, 31 * G, SLOT_MEM_MB), 15); // aarch64-linux
        assert_eq!(eval_slots(18, 16 * G, SLOT_MEM_MB), 8); //  darwin
        assert_eq!(eval_slots(18, 256 * G, SLOT_MEM_MB), 18); // core-bound
        assert_eq!(eval_slots(4, 2 * G, SLOT_MEM_MB), 1); //    never zero
        // The heavy `--tests` budget (4 GiB/slot) trims the same boxes further.
        assert_eq!(eval_slots(18, 31 * G, TESTS_SLOT_MEM_MB), 7);
    }

    #[test]
    fn recovery_ladder_is_ordered_and_bottoms_out() {
        use Recovery::*;
        // Rung 0 outranks every back-off: work a dead pass already landed is never
        // redone, however many slots are running.
        assert_eq!(recovery(0, 36, 4), Done);
        assert_eq!(recovery(0, 1, 1), Done);
        assert_eq!(recovery(16, 36, 4), Retry);
        // Then concurrency, halved to 1 …
        assert_eq!(recovery(36, 36, 4), Slots(4, 2));
        assert_eq!(recovery(36, 36, 2), Slots(2, 1));
        // … then the atom, halved until a single item is left (both halves always
        // non-empty, so this makes progress) …
        assert_eq!(recovery(36, 36, 1), Split(18));
        assert_eq!(recovery(3, 3, 1), Split(1));
        assert_eq!(recovery(2, 2, 1), Split(1));
        // … and only then does it stop. A bottom rung is what makes the requeue
        // loop terminate by construction rather than spin (DESIGN §6).
        assert_eq!(recovery(1, 1, 1), GiveUp);
    }

    #[test]
    fn shard_expr_subsets_the_import() {
        let e = shard_expr(
            Path::new("/repo"),
            "abc123",
            "aarch64-linux",
            &["hello".into(), "with\"quote".into()],
            "{ allowUnfree = true; }",
        );
        // The same import as the whole-set walk, narrowed via listToAttrs,
        // names escaped.
        assert!(e.contains(r#"builtins.fetchGit { url = "/repo"; rev = "abc123"; }"#));
        assert!(e.contains("builtins.listToAttrs"));
        assert!(e.contains(r#""hello" "#));
        assert!(e.contains(r#""with\"quote" "#));
    }
}
