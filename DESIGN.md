# npd design

## 1. Purpose and scope

`npd` supports a **durable, iterative** nixpkgs workflow on a fixed set of
long-lived build machines with plenty of disk. It exists to make these cheap:

- evaluate a revision → the set of `attr → derivation` on each platform;
- diff two revisions (and, three-way, their merge base) to a set of changed attrs;
- learn what Hydra knows about a derivation or job;
- build derivations locally;
- render human-readable Markdown reports from all of the above;

…while **never repeating expensive work whose answer is already known**, and
while making it ergonomic to *deliberately* ignore the cache (build locally
instead of substituting; rebuild a success you suspect is flaky; skip a failure
you expect to just repeat).

### What `npd` is not

- Not a `nixpkgs-review` replacement. The build-result **classifier / comparison
  report** (the 3×3 base×change grid, the composed status token, `narinfo`/drift
  logic) is a *presentation* concern that belongs upstream in nixpkgs-review as a
  self-contained feature; `npd` reuses that rendering rather than owning it.
- Not a re-implementation of Nix's primitives. Evaluation goes through
  `nix-eval-jobs`; building goes through `nix build` + the existing remote
  builders. `npd` owns the **memory** and the **orchestration**, not the plumbing.

## 2. The one load-bearing decision: key facts on `drvpath`

A **derivation path** (`/nix/store/<hash>-name.drv`) is the identity of a build
*recipe* — a hash of its inputs. An **output path** (`/nix/store/<hash>-name`) is
the identity of a produced *artifact*. They differ, and the difference dictates
the schema:

- A **failed** build has no output but always has a drvpath. Keying on drvpath
  lets us remember failures; keying on output path can only remember successes
  (that's all a binary cache stores).
- The same drvpath recurring in two different commits/PRs is automatically **one**
  cache entry — cross-review sharing falls out for free.
- Output paths are many-to-one with drvs for fixed-output/CA derivations (one
  source path, countless fetch drvs), so they're a poor primary key anyway.

Therefore: **build facts are keyed on `drvpath`.** Output paths are used only
where they are the right key (narinfo / substituter presence).

## 3. Three kinds of facts, three caching disciplines

Conflating these is the classic way to be subtly wrong. Keep them separate.

| fact | key | discipline |
| --- | --- | --- |
| **eval** — attr→drv map + meta | `(commit, system, config-hash)` | **pure** → cache forever, never invalidate |
| **local build observations** | `drvpath` | **append-only log** — never overwrite; each entry is one attempt |
| **Hydra facts** — job status, narinfo, drift | `(job, system)` and `drvpath` / outpath | **mutable** → TTL / explicit refresh; Hydra's answer changes over time |

Rationale: an eval at a fixed `(commit, system, config)` is deterministic, so its
result is valid forever. A build *attempt* is an event, not a current-value — we
append it and never discard it, which is exactly what makes flakiness
representable (multiple observations of the same drv with differing outcomes).
Hydra's state is a moving target (a queued job finishes; a cached path is GC'd),
so those facts carry a `fetched_at` and expire.

## 4. On-disk layout

Durable state lives under `$NPD_STATE` (default `$XDG_STATE_HOME/npd`, i.e.
`~/.local/state/npd`) — **state**, not cache, because we do not want it swept by
cache-cleaning or Nix GC.

```
$NPD_STATE/
  evals/<commit>-<system>-<confighash>.json   # pure: {attr: AttrEval}
  observations/<drv-hash>.jsonl               # append-only Observation log per derivation
  hydra/job/<jobset>/<attr>.<system>.json     # mutable: last Hydra job status + fetched_at
  hydra/narinfo/<out-hash>.json               # mutable: substituter presence + fetched_at
  logs/<drv-hash>/<obs-id>.log                # build logs referenced by observations
  gcroots/<drv-hash>-<output>                 # nix gcroots for outputs we choose to keep
```

`<drv-hash>` is the 32-char hash component of the drvpath. gcroots are
mandatory for anything we want to survive `nix-collect-garbage`.

## 5. The observation log and the build-policy predicate

Every local build appends an `Observation` (source, outcome, when, duration,
machine, log). The ergonomics the workflow needs are then a **pure predicate**
over that log plus substituter presence:

- never observed, or forced → **build**
- a `LOCAL` success exists, `--recheck` off → **skip (ok)**
- substitutable success, `--prefer-local`/`--recheck` off → **skip (ok)**
- only failures observed, `--retry` off → **skip (fail)**
- otherwise → **build**

So the cache-bypass knobs are just fields on the policy: `recheck` (rebuild a
suspected-flaky success), `retry` (re-attempt a known failure), `prefer_local`
(don't trust a substituted/Hydra success — build it here). See
`BuildPolicy.decide` in `npd/model.py`.

## 6. Evaluation and the three-way diff

`eval(commit, system)` → `{attr: AttrEval}` via `nix-eval-jobs` (cached, pure).
A two-way diff is a set-diff on `(attr, drv_path)`. The **three-way** diff also
evaluates the **merge base** of the two commits, which classifies each changed
attr the way a git three-way merge does:

- changed by *this side* only (base == merge-base, differs at head),
- changed by the *other side* only (head == merge-base, differs at base — e.g.
  the target branch advanced / a mass rebuild landed),
- changed by *both* (all three differ — genuine interaction).

This is the main capability nixpkgs-review lacks; it is nearly free once
`eval` is a cached primitive.

## 7. Hydra facts — best-effort, tiered

There is **no reverse index** from a store path to a Hydra job on
hydra.nixos.org (search is name-keyed and 500s on paths; no `/store-path`
endpoint). So Hydra answers are best-effort, cheapest-first:

1. **narinfo** `HEAD cache.nixos.org/<out-hash>.narinfo` — drv-precise, drift-free,
   but **success-only** (404 conflates never-built / failed / GC-evicted). Cheap.
2. **forward job** `/job/<jobset>/<attr>.<system>/latest` — status + logs, but for
   the job's *latest* drv, which may differ from ours (**drift**). Medium.
3. **local rebuild** — ground truth; disambiguates narinfo's 404. Expensive.

Because our own eval already yields the base revision's *exact* output paths,
narinfo on **those** paths is drv-precise, and disagreeing with the forward-job
verdict is a **drift detector** (Hydra's green is a different derivation than
ours — the failure mode that first motivated this whole line of work). Hydra's
`isCachedBuild` flag / build duration further tells us whether a Hydra verdict is
a genuine run or a reused cached result.

Upstream opportunity (separate): Hydra already indexes `BuildOutputs.path`
(hash) and `Builds.drvpath` (btree + trigram); its `/search` merely uses a
substring `ilike` that can't use those indexes and times out. A small PR adding
an exact `drvpath`/`path` lookup would give a real reverse endpoint (surfacing
failures + cached flags), which `npd` would prefer over narinfo when available.

## 8. Reports

Markdown, reusing the nixpkgs-review comparison classifier: group each attr by
its **delta** (regression / fixed / dropped / added / pre-existing / unchanged /
uncertain) for triage, and render a **composed token** per row (`before → after`,
tagged with source and confidence/drift) so no information is lost. Cascades
(`dependency failed`) are separated from direct failures and attributed to their
root.

## 9. Build order (spine first; resist features until the spine carries weight)

1. cached `eval(commit, system)` → attr→drv map (`nix-eval-jobs`).
2. two-way diff, then the three-way (merge-base) diff.
3. the drvpath-keyed observation store + `BuildPolicy` + a local build driver
   that consults/appends it and manages gcroots.
4. Markdown report reusing the classifier.
5. *then* Hydra facts (narinfo → job → drift), richer reports, remote-builder
   fan-out.

## 10. Open questions

- `config-hash` in the eval key: which inputs actually affect the attr→drv map
  (nixpkgs config, overlays, system) and how to hash them stably.
- Concurrency / locking on the append-only logs across the three machines
  (shared store? per-machine store that syncs?).
- How aggressively to prefetch/refresh Hydra facts vs. fetch on demand.
- Whether the classifier lives as a shared library or is vendored from the
  nixpkgs-review PR.
