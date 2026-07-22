# npb design

## 1. Purpose and scope

`npb` supports a **durable, iterative** nixpkgs workflow on a fixed set of
long-lived build machines with plenty of disk. It exists to make these cheap:

- evaluate a revision → the set of `attr → derivation` on each platform;
- diff two revisions to a set of changed attrs;
- learn whether a derivation is already substitutable from `cache.nixos.org`;
- build derivations locally, remembering the outcome (Nix keeps the log itself);
- render human-readable Markdown reports from all of the above;

…while **never repeating expensive work whose answer is already known**, and
while making it ergonomic to _deliberately_ ignore the cache (build locally
instead of substituting; rebuild a success you suspect is flaky; skip a failure
you expect to just repeat).

### What `npb` is not

- A `nixpkgs-review` **alternative**, not a clone. It does the same core job —
  evaluate a PR's `base → head`, build the changed set, render a delta report —
  and on the pre-build eval path it is competitive-to-faster (measured across
  62/31/16 GiB machines; §6). What distinguishes it is _what it keeps_: the
  durable, `drvpath`-keyed fact store (§2–§5) that makes an _iterative_ loop of
  related reviews cheap — never repeating work whose answer it already knows —
  where nixpkgs-review is one-shot and throws the workspace away.
- Not a re-implementation of Nix's primitives. Evaluation goes through
  `nix-eval-jobs`; building goes through `nix build` + the existing remote
  builders. `npb` owns the **memory** and the **orchestration**, not the plumbing.

### No breaking changes, and still no migration code

`npb` is public, with users beyond its author. We are **committed to the
formats and interfaces we have already chosen.** Two constraints now hold at
once, and it is their _combination_ that dictates the discipline:

- We can no longer just change a format when it's convenient. `~/.cache/nix-npb`
  (the SQLite schema, the eval-file format) lives on other people's machines,
  holding history re-derivable only at the cost of re-running every build and
  probe behind it, and a current `npb` must read a store an older `npb` wrote.
- **And we still write no migration code** — no SQLite schema upgrades or `ALTER
  TABLE`/purge steps for data an older version wrote, no readers or fallbacks
  for previous file formats, no "this column may linger on old databases"
  tolerance. That rule has not relaxed; what's gone is the escape hatch that
  paired with it (changing the format in place, then hand-migrating the single
  local store).

With neither "just change it" nor "migrate it" available, the only move left is
**restraint**: don't make a format change that would need a migration. Evolve a
stored format only in ways an old and new `npb` both tolerate — a purely
additive column or file — or leave it alone; never delete or invalidate the
store to sidestep the problem. The CLI (flags, subcommands) and the report
format are interfaces people script against and share: keep them stable and grow
them additively, never breaking an existing flag, subcommand, or output shape.

When a feature is removed, still remove **all** of its dead code in the same
change — enum variants, struct fields, table columns, parsing, tests, doc
references — but a removal that changes a stored format or a user-facing
interface is itself a breaking change, and is held to the rule above.

(Design _rationale_ for dropped approaches — e.g. why Hydra isn't consulted,
§7 — is worth keeping in this document. Code paths for them are not.)

## 2. The one load-bearing decision: key facts on `drvpath`

A **derivation path** (`/nix/store/<hash>-name.drv`) is the identity of a build
_recipe_ — a hash of its inputs. An **output path** (`/nix/store/<hash>-name`) is
the identity of a produced _artifact_. They differ, and the difference dictates
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

## 3. Two kinds of facts

There are only two, and collapsing everything else into the second is a
deliberate simplification (it dropped out of the design discussion):

| fact                                    | key                                    | discipline                                 | storage               |
| --------------------------------------- | -------------------------------------- | ------------------------------------------ | --------------------- |
| **eval** — attr→drv map                 | `(tree, system, config)`               | **pure** → cache forever, never invalidate | one flat file per key |
| **observation** — one build/probe event | `drvpath` (or output path for `Cache`) | **append-only log** — never overwrite      | SQLite                |

An eval at a fixed `(tree, system, config)` is deterministic, so its result is
valid forever. The key is the git **tree** (the source content), not the commit
that carries it — the evaluation can't observe a commit's parents, author,
message, or timestamps (`fetchGit`'s checkout has no `.git`, and npb forwards
only the path into `import`), so two commits with one tree share an eval (§6). Everything else is an **observation**: a single event — an outcome we
watched a local build produce, or narinfo presence on a substituter, recorded
as the same plain `Built` fact (§7; a success is a success, wherever the bits
came from). We append and never discard, which is what
makes flakiness representable (multiple observations of one drv with differing
outcomes); rows carry no timestamp — the log is append-only, so insertion
order _is_ the history.

**A cache probe is an observation too** — "is output H in the cache right now"
is just something we observed, recorded so a later run needn't
re-probe. There is no eviction and no TTL, which keeps full history (a drv that
went green → red → green is visible) under one log.

> **History:** `npb` once also consulted Hydra (a `HydraJob` source + an `npb
hydra` command). That was dropped: the public Hydra API has no reverse
> drvpath→build lookup, so its forward-job answers _drift_ (a different drv than
> ours) and are unreliable to key facts on. `npb` now consults only
> `cache.nixos.org` (drv-precise) and local builds (ground truth).

## 4. Storage

Everything `npb` stores is re-derivable, so it lives under
`dirs::cache_dir()/nix-npb` (i.e. `~/.cache/nix-npb`), like `npc`. The records are
all cache: losing them costs re-evaluation / re-building, not correctness. `npb`
keeps **no gcroots** — a built output may be GC'd, but the _observation_ that it
built survives, and that's the fact we actually need; if the output is wanted
again, Nix rebuilds or substitutes it.

**npb requires Nix ≥2.35, and this is load-bearing for the disk story.** 2.35
copies sources to the store lazily: since `build_expr`'s `fetchGit` tree (§6) is
only ever _read_ — imported and walked, never forced to a store path — Nix hashes
it in place instead of materializing a ~400 MB `/nix/store/…-source` object per
reviewed tree, which older Nix wrote eagerly (and which npb, keeping no gcroots,
left for `nix-collect-garbage` to reclaim). Both eval binaries must be 2.35 for
this to hold — `nix-instantiate` enumerates the attr names and `nix-eval-jobs`
evaluates the shards, and either one forcing the tree would copy it — so the
flake pins both to the 2.35 series (`nix-eval-jobs` built from its 2.35.0 release
candidate, since nixpkgs packages only 2.34 so far; §9).

The two fact kinds have opposite access patterns, so they get different backends.

**Evals → one flat file per `(tree, system)`** under `<system>/`, sorted
`attr\tdrv` lines (empty drv = no derivation; a third field `!` marks the few
attrs npb skips — meta broken/unsupported/insecure; `src/eval.rs`). The drv is stored
stripped of its constant `/nix/store/…​.drv` prefix/suffix, and the whole file is
zstd-compressed (default level) — together ~3× smaller (~11 MB → ~3.4 MB). An eval is bulk,
write-once, read-as-a-whole data whose _only_ use is to be diffed against another
eval, so a file beats SQLite on every axis that matters here:

- **smaller** — ~3.4 MB compressed (vs ~11 MB raw, ~22 MB in SQLite: no per-row
  overhead, no `(run_id, attr)` index duplicating the data);
- **faster to diff** — both files are sorted by attr, so the changed set is a
  linear two-pointer merge over two line streams, each decompressed on its own
  thread (~12 ms, never materializing a whole file) rather than ~114k
  primary-key point-lookups (~94 ms). The cross-cutting SQL queries that would
  have justified a table never materialised (we only ever diff);
- **evictable** — `npb --clean <SIZE|DATE|DURATION>` (`src/clean.rs`) deletes
  whole eval files least-recently-used-first until the corpus fits a byte budget
  (`4GiB`), or drops everything older than a date (`2026-07-15`) or unused for a
  duration (`2mo`); no `VACUUM` of a monolith. It's a destructive maintenance
  action, so it first prints how much it would remove (file count + bytes, not
  the individual files — there may be very many) and waits for a `y` on stdin,
  deleting nothing without it (a closed stdin reads as _no_). "Least-recently-_used_" is the
  file's mtime, which a cache **hit** re-stamps (`evalfile::touch_eval`, called
  from `eval::eval_pairs`) — a read alone wouldn't, so a shared base eval reused
  across many reviews would otherwise look as old as its first write. Evicting an
  eval also purges that `(tree, system)`'s `--tests` rows (below), keyed on the
  same tree, so the two stay in lockstep. (The "millions of tiny files" failure
  mode is about a file _per attr_; one file per _eval_ is ~two files per review.)

Writes are atomic — a uniquely-named temp file in the same directory (rename is
only atomic within one filesystem), then `rename` into place — so a crash can't
leave a truncated file that would poison the cache, and concurrent writers of
the same eval can't collide.

**Observations → SQLite** (`npb.sqlite`), where the append-only log actually
wants an engine: indexed lookup by `drvpath`, transactional appends, no torn
writes. The log itself stays tiny (KBs — a few hundred rows); the database
file's bulk is the `--tests` cache below, which scales with the number of
distinct trees reviewed (like the eval files, but ~two orders of magnitude
smaller per review). Build logs are stored nowhere: Nix keeps them under
`/nix/var/log/nix/drvs` (`nix log <drv>`, success or failure).

**The `--tests` cache → SQLite too** (`test_pkg` / `test_drv` tables, §6). Same
reasoning inverted from evals: it's a _keyed, incremental, partial_ fact (look up
a package, append new ones), not a bulk write-once map to diff — so it wants the
engine, not a file. Two space measures keep it lean, since it dominates the
database file: the `(tree, system)` a row belongs to is **interned** into an
`eval_key` table and referenced by a small integer id rather than repeated as a
40-char tree hash on every row (the bulk of the
win — a handful of keys back thousands of rows); and drv paths are stored
**stripped** of their constant `/nix/store/…​.drv` affixes, exactly like the eval
files (`evalfile::strip_drv`), restored on read. Every query is already scoped to
one constant `(tree, system)`, so interning adds no per-row join — just one
indexed point-lookup per operation to resolve the id. It's evictable by
`(tree, system)` in lockstep with the eval files (`Store::purge_tests`, driven by
`--clean`), then `VACUUM`ed to return the pages. The observation log strips its
paths the same way — `drv_path` of the `/nix/store/`+`.drv` affixes, and each
`blocker` output path of the `/nix/store/` prefix (an output has no `.drv`, so
it uses a prefix-only `strip_out` rather than `strip_drv`) — and stores its
`source`/`outcome` as small integer enum codes rather than English labels. This
matters more there than anywhere else: it's the one append-only, never-evicted
table, so its per-row bytes are what compound over time (~15% off it, measured).

```
~/.cache/nix-npb/
  npb.sqlite                    # observation log (tiny) + --tests cache (the bulk) + patch-tree cache (§8)
  <sys>/<tree>.tsv.zst          # attr→drv maps (zstd), one file per eval — evicted by --clean
```

`nix-eval-jobs` stderr (a full Nix traceback per errored attr — megabytes over a
package set) is _not_ persisted: we drain it into a small in-memory ring buffer
and surface only its tail if the eval aborts fatally.

## 5. The observation log and the build-policy predicate

Every local build appends an `Observation` (outcome, plus a failure's
`blocker` outputs). The ergonomics the workflow needs are then a **pure
predicate** over that log (substituter presence is already _in_ the log — a
probe hit is recorded as a plain `Built`, §7):

- meta-blocked (broken/unsupported/insecure), `--no-skip` off → **skipped**
  — never attempted, like nixpkgs-review; the report shows ⏩. (Checked first,
  so `--retry` alone doesn't build it. The marking _masks_ recorded facts, in
  the predicate and the report alike: a default run shows ⏩ even for a drv an
  earlier `--no-skip` run built or failed, so default-run output never depends
  on what past `--no-skip` runs happened to learn.)
- never observed, or forced → **build**
- a recorded success exists — built here, or substitutable (§7) → **skip (ok)**
- only failures observed, `--retry` off → **skip (fail)**
- otherwise → **build**

("Skipped" is npb's name for what nixpkgs-review calls skipped — its
meta-blocked subset; a _missing_ attr is a separate state, ➖ absent. The
cache-skips above — `skip (ok)`/`skip (fail)` — are not that state: they still
render as the real built/failed outcome.)

So the cache-bypass knobs are just fields on the policy: `retry` (re-attempt a
known failure) and `no_skip` (build the meta-blocked packages npb otherwise
skips). See `BuildPolicy::decide` in `src/model.rs`. (Tests run by default;
`--no-tests` opts out.)

**Staying instant when cached.** The driver loads every target's history in one
SQLite query, and only _probes the cache_ for drvs the log has no fact about
at all; those probes
run concurrently (`cache::in_cache_many`). So a changed set whose facts are all
known costs one query and no network — the whole build set is decided in
milliseconds. (Builds stay strictly behind the eval phase: they are the memory
heavyweights, and co-scheduling them with eval workers risks an OOM-killed
build being recorded as a false `Failed` fact.)
The actual build is a single batched `nix build` piped through
`nom` for the live tree, from which we recover, per drv, its outcome (built /
direct failure / dependency cascade).

**Surviving ^C.** Each outcome is recorded (and committed — every observation is
its own SQLite autocommit) the moment that drv's build activity stops, not after
the batch: nix registers a successful build's outputs _before_ emitting the
activity's stop event, so output validity at stop time is the build's own
result — **ground truth, never an exit-status guess**. This fires for **every**
drv nix builds, not just the requested set — a transitive **dependency** that
fails is recorded too (keyed on its own drvpath; a dependency _success_ needs no
row, since nix's store validity already remembers it — and the propagation below
re-checks exactly that validity). Interrupting the batch therefore keeps every
fact observed so far — including the failures nix itself forgets — and a re-run
only re-pays for the in-flight and never-started builds. Requested targets with
no build activity (blocked by a failed dep, or already valid) are attributed in
a post-batch step that records only what it can ground in the store: `Built`
(outputs valid) or a `DepFailed` naming a _verified still-failing_ culprit —
never a bare failure inferred from nix's ambiguous exit code.

**Forward-propagating failures, and self-healing them.** Recording a
dependency's failure is only half the recovery. The changed-set _target_ a
failed dependency blocks never gets its own build activity, so before building,
the driver drops any target whose **build closure** (`nix-store --query
--requisites` on its `.drv`) contains a still-failing dependency, recording a
`DepFailed` immediately (committed, so a ^C keeps it and the next run skips the
dependent without re-pulling the failing dependency). Two properties make this
both sound and _self-correcting_:

- **Verified, not assumed.** `Store::failing_drvs` (drvs with a local failure
  and no success _in the log_) is only a **candidate** set; each candidate
  reachable from a target's closure is re-checked against the store
  (`verify_failing`: are its outputs actually still invalid?) before it may
  block anything. A dependency that has since built or been substituted — a
  flaky failure, a since-fixed one — drops out, and never blocks a dependent on
  stale news. (A target's own drv is excluded from its culprit search:
  `--requisites` lists a drv among its own inputs, and a re-opened target still
  carries its old failure, so without this a target would block _itself_.)
- **Self-healing via the culprit `blocker`.** A recorded `DepFailed` stores the
  culprit dependency's output paths (`Observation::blocker`). A later run
  re-checks those paths' validity **offline** — one `nix-store
--check-validity`, no `.drv`, no closure walk, so a fully-cached run stays
  instant (§6) — and the moment the culprit is valid, the block is _stale_: the
  dependent is re-attempted with **no `--retry` needed**, and its success (or a
  fresh block on whatever is still broken) supersedes the stale row. This is the
  dependency-side of `flaky_success_wins`: a later success outranks an earlier
  failure, read from the store rather than from a recorded `Built`.

A **direct** failure (a drv's own build failed) is stickier by contrast —
presumed to recur, `--retry` to re-attempt — because it _is_ a fact about that
drv, not a second-order inference about a dependency. It still self-heals on the
same store-validity signal, though (`recheck_direct_failures`): a `Failed`
records the drv's _own_ output paths in its `blocker` (the direct-failure
analogue of a `DepFailed`'s culprit blocker), and a later run re-checks them —
the moment they are valid, the drv built out of band (a plain `nix build`, an
unrelated realisation) and the sticky `Failed` becomes a `Built`, no `--retry`
needed. A failure that recorded its outputs is checked **offline** (no `.drv`,
one `nix-store --check-validity`), so a warm run whose failures are all recorded
stays instant. A failure with _no_ recorded outputs — nothing to validate
against — isn't a dead end: it's simply a materialize candidate
(`needs_selfheal_instantiate`), so `drvs_to_materialize` (§6) pulls it into the
one instantiate pass, its outputs are resolved from the freshly-written `.drv`,
and the same check runs. If it's still invalid it records those outputs, so the
next run re-checks it offline instead of re-materializing — self-limiting. The
only sticky residue is a failure whose outputs can't be resolved at all (no
`blocker`, `.drv` GC'd), overridden as before by `--retry` or a later `Built`.
`--retry` disables propagation entirely; the check is gated behind a non-empty
failing set and a union-closure query, so a run with nothing failing pays
nothing.

**Soundness caveats (known, accepted).** Every recorded fact is now grounded in
store validity: `Built` from valid outputs, `Failed` from a drv's own stop event
with invalid outputs, `DepFailed` only when a culprit dependency is _verified_
still-invalid. Nothing is inferred from nix's exit status, so the old gap — a
batch aborting with a normal error code mis-attributing never-started drvs as
`DepFailed` — is closed: a target nix simply never reached, with nothing
verifiably failing in its closure, is left unrecorded and re-attempted next run.
What remains, deliberately: a `Failed`/`DepFailed` row is only re-examined
against the store _lazily_, when the policy is about to act on it (skip a build,
propagate a block), so a since-healed failure lingers in the log until then —
harmlessly, since it is overridden at use (a direct failure by `--retry`, a
later `Built`, or its own recorded outputs going valid; a dependency block
automatically, via the `blocker` re-check).
And a probe-recorded `Built` fact records substitutability _at probe time_ —
the remote cache
deleting a path later doesn't invalidate the fact (by design, §3), it just means
nix substitutes from source instead.

## 6. Evaluation, its cache key, and the diff

**The cache key is `(tree, system, config)`, and it is not a can of worms —
provided `npb` owns the config.** What determines the attr→drv map is the
nixpkgs source _tree_, the platform, and the nixpkgs _config_ (allowlists like
`allowBroken`/`allowUnfree`/`allowUnsupportedSystem`, `permittedInsecurePackages`,
overlays, `config.allowAliases`, …). The trap is letting a user pass arbitrary
Nix as config — that isn't cleanly hashable. `npb` avoids it by **defining the
eval config itself**: one fixed allow-everything config (`EVAL_CONFIG` in
`src/eval.rs`), so the key is just `(tree, system)`. There is no extra tag in
the key: the file format, _how_ `nix-eval-jobs` is invoked, and the config
itself all determine the stored map, so they are part of the compatibility
surface we hold fixed (§1) — a change to any of them would invalidate every
cached eval, and with neither a migration path nor the freedom to discard the
store, they don't change. (An earlier design threaded a named "profile" label
through the key to leave room for several configs, and a later one an
eval-version tag baked into each filename; with exactly one config ever defined,
both were redundant and dropped.)

**Why the git _tree_, not the commit.** The eval is a pure function of the
checked-out file content — a commit merely wraps a tree with parents, an author,
a message, and timestamps, none of which the evaluation can see: `fetchGit`'s
checkout carries no `.git`, and npb passes only the resulting _path_ into
`import` (never the fetchGit attrset's `rev`/`lastModified`/`revCount`). So
keying on the commit was strictly _over_-specific — two commits with the same
tree evaluate identically, and even fetch to the byte-identical store path.
Keying on `tree` (`git rev-parse <commit>^{tree}`) collapses them into one cache
entry: a rebase that leaves the changed files alone, a message-only `--amend`, a
cherry-pick landing identical content, and — the payoff — committing an as-is
working tree all become cache _hits_. npb resolves each requested revision to a
`Rev { tree, commit, label }` (`src/model.rs`): `tree` is the eval/`--tests`
cache key, `commit` is what `fetchGit` fetches (a commit is still needed — there
is no fetch-a-bare-tree), and `label` identifies the side (a sha, or `worktree`
for a synthetic working-tree/patch head — the report heading shows the latter as
its anchor commit + `\*`, §8). The soundness rests on npb never forwarding
`rev`/`lastModified`
into the eval; if it ever did (to stamp `lib.version`/`config.revision`,
flake-style), the eval would regain a commit dependency and tree-keying would
serve a stale eval — so `build_expr` (`src/eval.rs`) deliberately interpolates
only the path.

**Reviewing the uncommitted working tree.** Because the key is a tree, an
uncommitted working tree is reviewable like any revision: on the default head
path (no explicit `head`), when the working tree has uncommitted changes, npb
captures them with `git stash create` — which snapshots edits/deletions to
tracked files and staged-new files (but _not_ fully-untracked files, a
documented limitation) into a commit without disturbing the branch/index/working
tree, and reuses git's real index stat cache so a clean tree costs ~`git status`
time rather than re-hashing every tracked file. Over that stash's _tree_ npb
mints its own **deterministic** synthetic commit (pinned identity + epoch dates,
parent `HEAD` — the stash commit's own sha is timestamped, hence unstable, so it
is not used), pinned under `refs/npb/worktree` so a `git gc` can't drop the
dangling object before `fetchGit` reads it (`worktree_source` in `src/main.rs`).
The tree hash is pure content, so an unchanged working tree re-runs against the
same cache entry, and committing it as-is hits that same entry (the real commit's
tree equals the synthetic one). An explicit `head` is always taken literally —
the working tree is used only on the default path.

Caching is sound because nixpkgs evaluation is deterministic given those inputs
(drv paths are content-addressed by their inputs, stable across time and
machines); IFD is still deterministic, and impurities like `currentSystem` are
fixed by the `system` key. So "should we cache evals?" — yes, unreservedly, once
`npb` owns the config.

**Scheduling — one queue of shards.** The scheduling and failure atom is not a
whole-set eval but a **shard**: a ~1024-name slice of one eval's top-level attr
names — enumerated by one cheap `builtins.attrNames` call per pair, itself run
through this same scheduler as a single-shard group so a multi-system run's
enumerations overlap behind the shared display (the ~1024 is overridable with
`--shard-size`) — evaluated by its own one-worker `nix-eval-jobs` over the same
import narrowed via `listToAttrs` — validated byte-for-byte to reproduce the
monolithic walk (thunks force per-attr in the worker, so error isolation is
identical). Bigger shards amortize the per-shard nixpkgs re-import; ~800–1600 is
a flat measured best across 62/31/16 GiB machines (400 left 20–30% on the
table), with peak memory bounded by the RAM ceiling since it scales as
shard-size × slots. All shards of all pending evals share **one global queue**
and one knob: the number of slots (concurrent shard jobs), started at
`min(cores, total RAM / ~2 GiB)` — where the ~2 GiB per-slot budget is the
_typical_ worker footprint, kept distinct from the 4 GiB per-worker restart cap
(only the few giant subtrees approach the cap, so counting slots at it
under-parallelizes). Invariants only (total RAM further capped by any cgroup
memory limit the
process runs under: a container's ceiling is as much a configured promise as
the DIMMs). The dynamic part of RAM is handled by feedback, TCP-style
(AIMD), instead of measurement: a shard that aborts (in practice a worker
OOM-kill, caught by the integrity gate) is simply **requeued** while the slot
count halves; sustained success creeps it back up. The requeue is in-memory —
the aborted shard goes back on the queue and completed shards' rows are held in
memory until assembly — so an in-run worker OOM is transparent, but a
whole-process interruption (^C, crash) discards the in-flight eval, which
re-runs from scratch next time rather than resuming. (Nothing transient is
written to disk: an eval is either fully cached as its one file or not at all.
Shard partials were persisted for cross-run resume once, but the resilience
that matters — the OOM requeue above — never needed them, and they left
uncompressed files to garbage-collect for a resume that only helped the narrow
case of re-running an interrupted _first_ eval of the same commit.) When an
eval's last shard lands, its rows are assembled into the one cached file. Small
atoms are what make everything cheap: an abort re-pays seconds (not a whole eval),
idle slots drain any eval's remaining shards (no straggler eval), and the
degenerate case — a machine that fits only one worker — is just the queue at
one slot, not a special phase. The costs: each shard job re-imports the
nixpkgs spine (a few seconds; single-digit percent of a shard's runtime at
this size), and a giant single subtree (`haskellPackages`, `linuxKernel`, the
python package sets, ~20k attrs each) is one indivisible ~minute shard that
bounds the makespan once slots ≥ total-work/max-shard (measured 1.39× over the
perfect-packing bound at 15 slots).

> Recursive splitting of those subtrees was tried and **backed out** after
> measurement: selecting attrs inside a giant package set forces that set's
> _fixpoint construction_ (~15 s for `haskellPackages`) in **every** child
> shard — and once more to enumerate its names — so splitting a ~60 s subtree
> into k shards costs ~k×15 s of new work for a tail floor that can never drop
> below the construction cost. Net effect, measured on identical work: one
> fresh eval went 122 s → 191 s on a 7-slot machine, and the projected ~19 s
> tail win at 15 slots is eaten by the same overhead. Splitting only makes
> sense with a time model that knows each subtree's construction cost, or
> upstream support for sharing a constructed set across workers — revisit
> there, not with attr-count heuristics. `--eval-slots` overrides the starting slot count.

> Two earlier schemes are recorded for context. A _planner_ divided measured
> available RAM into per-eval worker slots — but that snapshot lies (free RAM
> moves during a minutes-long eval, with no recovery when it did) and the
> arithmetic idled cores. A _width ladder_ then retried a whole aborted eval
> at halving worker counts, with a final serialize-alone rung — it worked, but
> every rung re-paid minutes because the retry atom was the whole eval, and
> cross-eval balance was still fixed at spawn. Both dissolved into the queue:
> the ladder _is_ the slot count backing off, the rung _is_ the queue draining
> to one slot, and rebalancing is what a shared queue does natively.

**Eval purity vs `builtins.getEnv`.** A handful of nixpkgs packages leak the
_environment_ into their derivations (drbd bakes `$SHELL` into a Makefile
patch), so two evals of the same `(commit, system)` from different shells
disagree on those drvs. npb scrubs the known offenders from the evaluator's
environment (`SHELL` removed, so `getEnv` yields `""`, matching a hermetic
eval) — the cache key stays honest without hashing the environment.

`eval(commit, system)` → `{attr: AttrEval}` via `nix-eval-jobs --meta` (cached,
pure). Each attr carries its drv plus one meta bit — the **skipped** flag (meta
broken/unsupported/insecure) — since meta is _not_ part of the drv hash, so the
build policy and report can't recover it from the drv alone. The diff is a
set-diff on `(attr, drv_path, skipped)` — a meta-only (un)marking changes no
drv but is still a review event and gets a row. (An earlier design also
sketched a _three-way_ diff against the merge base, classifying each changed
attr as changed-by-this-side / by-the-other / by-both; it turned out not to
matter in practice and was dropped. The merge base survives only as the
`--no-merge` base of a report.)

**Eval does not instantiate; the changed set is materialized before building.**
`nix-eval-jobs` runs with `--no-instantiate`: npb needs only the `drvPath` and
`outputs` (both emitted regardless), so it skips writing the `.drv` files — ~40%
faster (measured, all platforms), and it stops instantiating the ~114k attrs it
never builds (only the changed set of a few dozen is). The two consumers that
_do_ need the `.drv` present in the store — the narinfo probe (§7, which reads a
drv's output paths) and the local build (`nix build <drv>^*`, §5) — get it from
a just-in-time `eval::instantiate` step: one `nix-eval-jobs` run per
`(commit, system)`, instantiation on, over exactly the changed attr paths
(nested paths included, via `lib.attrByPath`), run right before building. These
per-pair runs go through the **same shard scheduler** as the two eval paths
(`run_shards`), so a fresh multi-system run instantiates all pairs concurrently
(up to the same slot count) behind the identical live display, instead of
silently re-importing nixpkgs once per pair in series. Each pair is _one_ shard
— the cost here is the per-run nixpkgs import, so sub-slicing a pair's handful
of changed attrs would only re-pay that import — so this trims the phase's
wall-time from the _sum_ of the imports toward the _slowest single_ one at no
extra total work. Crucially, it instantiates _only the drvs the build phase
will actually touch_.
A drv already decided from the observation log alone — built, substitutable, or
a failure with its outputs recorded (checked offline, §5) — buys nothing from a
`.drv`; the driver asks the log which drvs still need probing, building, **or a
self-heal-check** (`build::drvs_to_materialize`, the pre-probe form of the
build-policy predicate — one SQLite query, no `.drv` required) and instantiates
just those. That last case is a failure with _no_ recorded outputs: it can't be
re-checked offline, so its `.drv` is materialized here to resolve them and the
build phase re-checks store validity (§5) — folding the self-heal's cache-miss
into this one pass rather than a bespoke query. In the warm-cache iterative loop
npb is built for, _every_ changed drv is already decided from the log — successes
and recorded failures alike — that set is empty, and the instantiation eval is
skipped entirely — without this, a fully-cached run still paid a couple of
seconds re-importing nixpkgs to write `.drv` files nothing would read. On a RAM-constrained machine
the lean `--no-instantiate` workers are also what let npb parallelize at all —
instantiating workers hit the memory ceiling and thrash (measured on 16 GiB).

**Choosing `base` and `head`.** Every input mode resolves to one shape: a
_base-branch tip_ and a _head_ to review against it (`resolve_local`/`resolve_pr`
in `src/main.rs`), onto which a single merge rule (`apply_merge`) then applies.
The pair comes from one of three modes:

- _Default_ — no arguments: base-branch tip = `master`, head = `HEAD`. When the
  working tree has uncommitted edits to tracked files, `head` becomes the working
  tree itself (a synthetic tree-keyed revision, §6) so in-progress work is
  reviewable. An explicit `--head` opts out. `--patch` (below) applies its diff
  _on top of_ this same default head — so with a dirty tree it stacks on the
  working tree rather than silently dropping it; `--head HEAD` anchors it on the
  committed tree instead.
- _Explicit_ — `--base <rev>` / `--head <rev>` override either end with any
  revision (ref, sha, tag, `HEAD~1`, …), resolved with `git rev-parse`.
- _PR_ — `npb --pr N` is shorthand for a `(base, head)` pair drawn from GitHub's
  published refs. GitHub publishes, on the **base repo** (so cross-fork PRs need
  no fork URL), `refs/pull/N/head` (the PR tip) and — when the PR merges cleanly
  — `refs/pull/N/merge`, a merge commit whose **first parent is the base-branch
  tip** and second parent is the PR head. So `--pr` sets base-branch tip =
  `merge^1` (the PR's _actual_ target branch — `staging`, `haskell-updates`, a
  release branch — whatever it is) and head = `merge^2` (the PR tip). This needs
  **no GitHub API and no token**: the refs come over anonymous git, unlike
  `nixpkgs-review`, which calls the REST API to learn the merge sha (and nags for
  `GITHUB_TOKEN`/`gh`). `--pr` is a deliberate exception to "no network when
  cached" (§1) — as is a `--patch <A...B>` compare download on a _cache miss_
  (§8); every other path, and a warm compare re-run, is offline. The merge ref is
  a _moving pointer_ GitHub regenerates on a
  rebase or base move, so npb re-fetches it every run and resolves the fresh
  pointer — a repeat `--pr` always reflects the current PR, never a stale
  snapshot. This doesn't defeat the caches that matter: an unchanged PR is a
  near-free "up to date" fetch, and eval/build stay keyed on the git
  tree/drvpath, so a genuinely-unchanged PR still hits them; only a PR that
  _actually_ moved (new tree) re-evaluates, which is exactly right. An
  unreachable upstream is fatal (npb won't review a stale snapshot), so `--pr`
  needs the network where every other path is offline.

**The merge rule (`apply_merge`), and `--no-merge`.** Given the `(base-branch
tip, head)` pair, npb reports one of two deltas:

- _Merge (default)_ — a **synthetic merge** of the head onto the base (base as
  first parent), reported as `base → merge`. This reflects the head applied on
  the _current_ base — base drift included — exactly what a merge would produce,
  the same shape ofborg/Hydra and `nixpkgs-review pr` evaluate. npb **always
  mints the merge itself** with `git merge-tree --write-tree` + `commit-tree` — a
  deterministic, content-addressed commit (pinned identity + epoch dates, pinned
  under `refs/npb/merge` against `git gc`), exactly like the working-tree capture
  (§6) — _including under `--pr`_, where GitHub already publishes a test-merge at
  `refs/pull/N/merge`. npb deliberately does **not** adopt GitHub's merge, and
  this is a **soundness** requirement, not a preference. A report's reproduction
  command can only _re-merge_: it rebuilds the head from a diff, which carries no
  ancestry (§8). GitHub's test-merge, by contrast, was computed by whatever git
  ran when the PR last changed — for an idle PR, an old git whose 3-way
  resolution can differ from a fresh one. So reviewing GitHub's merge while the
  repro re-merges would break the invariant that _the repro evaluates the same
  trees or fails loudly_ — it could silently evaluate a different tree.
  (Confirmed in the wild: nixpkgs#21303's 2017 test-merge swaps two option
  defaults vs. a current `git merge-tree` — one of 1 in ~525 sampled mergeable
  PRs.) Running both the review and its repro through `merge_source` makes them
  identical by construction.

  **The merge uses one _explicit_ merge base.** `git merge-tree` on a head that
  carries real ancestry builds ort's recursive _virtual_ base over every merge
  base of the pair; a repro rebuilds the head as a single-parent synthetic
  commit, so its merge has exactly one base. `merge_source` pins that single base
  (`--merge-base=<merge-base>`), so review and repro agree even across a
  criss-cross history — empirically vanishing in nixpkgs (0 of ~1100 sampled PRs;
  the contribution workflow discourages merge commits), but real. npb also runs
  git with the user's global/system config neutralized (`git_command`), so a
  stray `~/.gitconfig` merge driver or `merge.conflictStyle` can't perturb the
  result; `.gitattributes` drivers (nixpkgs' `module-list.nix merge=union`) still
  apply, since they are content under review, not environment (`git merge-tree`
  honors them). What remains — and is accepted, the same class as "eval
  reproducibility assumes a pinned Nix" (§4) — is a _git-version_ dependence: two
  machines on incompatible git could resolve the same 3-way differently.

  When the head already descends from the base the merge is a
  fast-forward, so its tree equals the head's and this collapses to a plain
  `base → head` at no extra eval; a distinct merged tree appears only under
  genuine base drift — precisely when you want to see it. A bonus: every review
  against the same base-branch tip shares its base eval (per-PR fork points never
  did). A conflicted PR (no `merge` ref) or a conflicting local merge can't take
  this path, so it fails with a message pointing at `--no-merge`.

  > **Alternative considered, equally reasonable.** Keep reviewing GitHub's exact
  > test-merge (byte-identical to what CI built) and make the _repro_ reconstruct
  > it by pinning a compare to the merge commit itself (`--patch merge^1...merge`)
  > rather than to the PR head. That preserves CI fidelity but rides on GitHub
  > still serving a _superseded_ merge commit's sha after the PR moves — a
  > base-repo synthesis, orphaned on update, unlike the fork-network-durable PR
  > head — which we could not confirm (the durability question is inherently
  > longitudinal; only the live-merge happy path is observable). npb-owns-the-
  > merge needs no such guarantee, keeps the compact PR-head compare in the repro
  > (§8, unchanged), and the fidelity it gives up is exactly the stale-git quirks
  > (nixpkgs#21303) we would rather shed than faithfully reproduce.

  When the resolved `base` and `head` land on the **same tree** — a bare `npb`
  on a clean checkout, an unmoved `--pr`, a `--base`/`--head` typo — there is
  nothing to review: the eval is tree-keyed, so the diff is empty and the whole
  build/report is a no-op reached only after a minute of cold eval. npb bails
  with an error before evaluating rather than warm one base eval as a silent
  side effect; equal trees is a mistake far more often than a deliberate
  cache-warm, and erroring surfaces it loudly.

- _`--no-merge`_ — the older, cheaper shape: `merge-base(base, head) → head`,
  the fork point. Offline and instant (no merge to build), but blind to base
  drift since the fork point, and — in the default mode — it assumes `master`
  even for a change branched off a non-`master` base. For a PR it lands on the
  fork point with the PR's real target branch (`merge-base(merge^1, head)`), or,
  if the PR is conflicted (no `merge` ref), the fork point with `master`.

**Tests — the changed set's `passthru.tests`.** Ported from
[nixpkgs-review#397](https://github.com/Mic92/nixpkgs-review/pull/397): for each
changed package, also build its `passthru.tests` (building a test derivation _is_
running it). On by default; `--no-tests` opts out. The full-set eval never
reaches these — a package's `tests` is a plain attrset without
`recurseForDerivations`, so `nix-eval-jobs` doesn't descend into it — so this
runs a **targeted second eval** over just the changed set: a job tree `<pkg>.tests.<name>` whose per-package `tests` node is a thunk
`nix-eval-jobs` forces in a worker (so a package that fails to evaluate errors
only its own subtree, never the whole run — the same per-attr isolation the
full-set walk relies on). Each test carries its own meta-blocked bit
(broken/unsupported/insecure), and a test can be blocked while its package is not
(an x86-only `nixosTest` hung off a cross-platform package is _unsupported_ on
`aarch64-linux`), so the bit is tracked per test, never inferred from the
package. Unlike a normal package, a `passthru.tests` entry is a
`nixosTest`/`vm-test-run` derivation that bypasses `check-meta`'s `commonMeta`,
so its raw `meta` has _no_ computed `unsupported`/`insecure` field for `--meta`
to carry — so the tests expression **computes** the bit itself (platform support
via `lib.meta.availableOn`, insecurity via `knownVulnerabilities`) and injects it
into each test's meta (`build_tests_expr` in `src/eval.rs`). This lands the same
verdict nixpkgs-review reaches by `tryEval`-ing the outPath under a strict
config: a meta-blocked test is skipped and rendered ⏩, exactly as nixpkgs-review
lists it under "marked broken and skipped". npb evaluates the tests on **both**
sides and keeps a test only where its `(drv, skipped)` pair actually differs
base→head, so the resulting rows classify (regression / fixed / new /
skipped / …) exactly like any other attr — a delta view, a superset of
#397's one-shot head-only build.

This eval **is cached**, but _per package_ rather than as a whole-set file. A
test's drv is a pure function of `(tree, system, package-attr)` — it
does not depend on the base/head pairing — so the cache keys on the package, not
the changed set, which means a package evaluated in one review is reused in any
other at that tree (§6's tree-keying: the same reuse a rebase/amend or a
committed working tree gets on the full eval). Each run looks up which changed packages are already
cached and evaluates only the misses through the **same shard scheduler as the
full-set eval** (`run_shards` in `src/eval.rs`). The misses across _every_
`(commit, system)` in the review are gathered and evaluated in **one** scheduler
run — a group per key, all shown and load-balanced together (just as the full
eval hands all its `(commit, system)` pairs to one queue), rather than one
key at a time. But — like the instantiate phase and _unlike_ the full-set eval —
**the scheduling atom is the whole `(commit, system)` key: one shard per key,
never sub-sliced.** Both phases share the full eval's _machinery_ but not its
work shape: their dominant cost is the per-key nixpkgs-spine re-import over a
changed set of a handful of packages, so slicing a key's packages across shards
would only re-pay that import per shard for no gain. For `--tests` there is a
second, sharper reason: a `nixosTest` worker ≈ a whole NixOS system, so it is the
_heaviest_ fan-out npb runs, and sub-slicing multiplied the concurrent heavy
workers — the earlier `total/(2·slots)` split started `2·slots` of them and
cascaded into OOM, then requeued one fat shard forever once the slot count
bottomed out at 1, because the shard (not the concurrency) was the
memory-bearing unit AIMD could never shrink. With the key as the atom, backing
off the slot count backs off concurrent heavy workers directly — real memory
control — the starting count is budgeted at the heavy-worker footprint
(`TESTS_SLOT_MEM_MB`, the worker restart cap, not the full-set eval's lighter
per-slot figure) and honors `--eval-slots`, and each key's single worker recycles
its heap per package at that cap. It gets
the identical `done + running / total` display. Sharing the scheduler means its
concurrency logic is exercised — and kept correct — by **every** memory-heavy
`nix-eval-jobs` fan-out (enumeration, the full-set eval, `--tests`, and
instantiation, §6) rather than each re-implementing it. And every live readout in
npb shares **one persistent progress tree** (`live::Tree`/`live::with_live` in
`src/live.rs`) spanning the whole pre-build run — a refresher thread redraws it at
a steady 100 ms off lock-free per-node atomics that the workers bump. It is a
tree: each piece of network or nontrivial work (`fetch`/`download`, `enumerate`,
`evaluate`, `tests`, `instantiate`, `probe`) is a top-level node the moment npb
learns it needs it — nesting a system level (always, one system or many) and
the per-side commit _display_ (`Rev::display`, §6: the friendly name of the tree
actually evaluated — `master`, `HEAD`, `merge(a, b)`, `#431 merge` — never a
resolved sha unless the user typed one) — and cached/no-op work never appears at
all, so a fully-cached run shows nothing. Nodes only change: blue _waiting_ →
yellow _running_ → green _done_ (nom's three colors, on the label; a plain middle
count where one applies, with a dim ` / total` or shard-`NN%` column alongside it
while running — `enumerate` carries just a color, `evaluate` the `NN%` since its
true drv total is unknowable), never disappearing. Each line is truncated to the terminal width (one line, one
row) and the live frame is windowed to the terminal _height_ — when the tree
outgrows the screen the last rows are kept (the running frontier and the phases
ahead of it, since earlier phases finish first) and the folded head collapses to
a dim `⋯ N more`, so the relative-cursor redraw never desyncs; the frozen reprint
is plain scrollback and not windowed. When the tree finishes it freezes
into scrollback, a dim separator fences it from what follows (nom's build display,
then the report — the same separator between each), and the build proceeds
(§5, nom's own display, not this tree). Persistence stays path-specific (§4): the full eval assembles a flat
file, `--tests` returns rows for the per-package SQLite cache. A fully-cached
re-run touches no `nix-eval-jobs` at all. Caching matters here because evaluating a test's drv
means evaluating its whole derivation graph, and a `nixosTest` in `passthru.tests`
pulls in an entire NixOS system — seconds and hundreds of MB _per test_ — so a
changed set with a few dozen server/library packages is a minute of evaluation
that would otherwise repeat on every run, defeating "instant when cached". It
lives in SQLite, not a flat eval file, because the access pattern is
keyed/incremental (§4).

## 7. Cache facts — the one remote signal

The only remote fact `npb` gathers is **narinfo presence** on `cache.nixos.org`:
`HEAD /<out-hash>.narinfo` for **every output** of the drv → is this exact drv
fully substitutable? (All outputs, because the recorded fact stands for the
whole drv; substitution is per-output, so one missing output would still force
a local build.) It is drv-precise and drift-free, but **success-only** (a
404 conflates never-built / failed / GC-evicted — it can never assert a
failure). A hit is recorded as a plain `Built` observation — deliberately
indistinguishable from a local success, so the one flakiness rule (a success
outranks failures) covers both — and a later run
skips the probe; a miss records nothing (re-probing is cheap, and cache state
can change under us). Ground truth for anything a narinfo can't answer is a
**local build** (§5).

> Why not Hydra? The public hydra.nixos.org API has **no reverse index** from a
> store path to a build (search is name-keyed, 500s on paths; no `/store-path`
> endpoint). Its forward job endpoint (`/job/.../latest`) returns the _latest_
> build's drv, which routinely **drifts** from ours — so it can't be keyed on
> without inventing false regressions. `npb` dropped it.
>
> Upstream opportunity (separate): Hydra already indexes `BuildOutputs.path` and
> `Builds.drvpath`; a small PR adding an exact `drvpath`/`path` lookup would give
> a real reverse endpoint (surfacing failures + cached flags), which `npb` could
> then consult in place of a local build for drvs Hydra actually built.

## 8. Reports

Markdown, grouped by the **delta** each attr underwent. Each side reduces to one
of six states — `✅` built, `❌` failed (direct), `🚫` blocked (a dependency
failed — the transitive/cascade case, kept distinct from a direct failure), `⏩`
skipped (meta-blocked: broken/unsupported/insecure — not attempted by default,
like nixpkgs-review; the marking masks any recorded fact unless `--no-skip` is
on, so a default report is stable across `--no-skip` history; a _missing_ attr
is `➖` absent, not this), `➖`
absent (no such attr on that side, or it no longer evaluates — a _known_ fact,
never a `?`; in a delta view an eval breakage is visible as disappearance, so
there is no separate eval-error state), `❔` unbuilt
(has a drv but no fact yet — since builds always run, only the build phase's
accepted gap of §5: a target nix never reached with nothing verifiably failing
in its closure). A section is one `(base, head)`
state pair, and its header **is** a composable `before → after` token (one emoji
per side) — no per-row glyphs; the section a row lands in carries all the meaning.
Sections are ordered **worst-delta-first**: each state has a goodness on the
build-outcome axis (`✅` > `⏩` > `🚫` > `❌`, with `➖` absent slotted just under
`✅` as _new_/_gone_), and a section sorts by the signed delta
`goodness(head) − goodness(base)` ascending — so the steepest regression
(`✅→❌`) leads, unchanged pairs sit in the middle, and every improvement trails;
equal deltas break by a worse current state. `❔` unbuilt has no fact to compare,
so any side still `❔` sinks to a final tier. This is a linear extension of the
product order on `(base, head)` — the whole `worst→best` ordering is _computed_
from state goodness (`priority` in `src/report.rs`), not a hand-kept table.
Each section is folded in a `<details>` (an earlier draft opened changed-state
sections by default; all-collapsed read better). Attrs that share a derivation
are collapsed onto one line (`a = b = c`, shortest attr first), like
`nixpkgs-review`'s aliases — npb gets this for free from its drvpath keying.

An `npb` run is not merely read-only: with defaults (`head` = `HEAD` merged onto
the `master` tip; or the PR merged onto its base branch under `--pr`; or the
merge-base under `--no-merge` — §6) it **builds both sides of the changed
set** (skipping anything already known or substitutable), so a fresh report has
a real state for every row rather than a wall of `❔`.

The heading links `npb` to the exact source tree the binary was built from —
`https://github.com/samestep/npb/tree/<rev>`, from the `URL` const in
`src/main.rs`, whose `<rev>` the Nix build bakes in as `NPB_REV` (`self.rev`, or
`main` for a dirty tree). `--version` prints the same URL, so a report and the
binary that produced it point at one commit. This is npc's `--version` scheme.

**Every report carries a copy-pasteable reproduction command** (a `sh`
block folded in a `<details>` under the heading, `repro_command` in
`src/main.rs`), followed by a second `<details>` glossing every glyph, so anyone
can re-run `npb` on the _exact same changeset_ — not the ambiguous invocation the
author happened to type (`npb` alone means a different changeset per machine and
day), but the resolved identity. Every form runs `npb --base <sha> --head <…>`
on a **pinned base** and a head whose **tree** is pinned: because the eval is
tree-keyed and the synthetic merge is deterministic (§6), that reproduces the
review byte-for-byte, and npb re-mints the merge itself — the command never names
a synthetic (local-only) commit. Only report-shaping flags are echoed
(`--no-merge`, `--no-skip`, `--no-tests`, and an explicit `-s` per system, since
the default system is host-specific); `--retry` and the eval-sizing knobs don't
change the changeset, so they're omitted. What varies is only how the _head_'s
tree is recovered on another machine:

- a committed / explicit head is already a fetchable commit → `--head <sha>`;
- otherwise (a `--pr` head or an uncommitted working tree) the head has no
  durably-fetchable commit, so it is **rebuilt** by `--patch`: npb applies a diff
  onto the resolved head (`--head`, else `HEAD`) in a throwaway index and
  `git commit-tree`s the result — the same reconstruction the live working-tree
  capture does internally (§6). The rebuilt commit's _sha_ differs from the
  original, but its _tree_ is identical, which is all a tree-keyed eval needs, so
  we never depend on an ephemeral sha. `--patch` takes one of two diff sources
  (disambiguated by Nix path syntax — a `/` means a path, else a compare
  expression):
  - **`--pr`** → `--head <fork> --patch <fork>...<head>`, a GitHub compare
    expression npb downloads (via its own `ureq`, no `curl`) as
    `compare/<fork>...<head>.diff` and applies onto the fork. `fork` is the PR's
    merge-base, a durable base-branch commit. This is **force-push proof**, which
    matters because nixpkgs PRs rebase constantly: GitHub retains a PR's commits
    by sha in its fork network, so the pinned compare resolves even after the
    branch has moved. It is why we _don't_ `git fetch refs/pull/N/head` (that ref
    tracks the _current_ tip, so the reviewed sha vanishes on a force-push) and
    why we don't try to recreate the exact commit from a `*.patch` (`git am`
    can't — a patch carries no committer identity/date or parent, so the sha
    differs anyway; the tree is what we need). One download covers a multi-commit
    PR (a net diff, not per-commit patches). A fetch failure at reproduction —
    an unreachable sha — is fatal, rather than a silent mis-review. (npb re-mints
    the merge from `--base merge^1` and the rebuilt head, so base drift is still
    reflected exactly as in the review.) **Heading label:** because the anchor
    `<fork>` _is_ the compare's first endpoint and the merge-base, applying
    `<fork>...<head>` onto it reconstructs exactly `tree(head)` — so the reviewed
    side is `head`, and the report names it `head` (not `<fork> \*`), byte-for-byte
    matching what the original `--pr` run's heading showed (`compare_head_display`
    in `src/main.rs`). The `\*` synthetic-head marker is kept only for a compare
    applied onto a _different_ anchor (a user's `--patch A...B` onto `HEAD`), where
    the head really is a rebased edit rather than a real commit's content. The
    reproduction _command_ is identical either way — only the heading text differs.
    **Exception — binary changes:** GitHub's
    text `.diff` can't carry a binary blob, so a PR that touches binary files
    would emit a repro that fails at `git apply`. npb detects this (`git diff
--numstat` shows `-\t-` for a binary file) and falls back to an embedded
    `git diff --binary <fork> <head>` — it has the PR head locally (`merge^2`), so
    it builds a binary-capable diff that reproduces offline (see the embed bullet).
    The compare form is kept for the common text-only PR, where it stays compact.
  - **a compare `--patch A...B`** → `--head <sha> --patch <shaA>...<shaB>`, the
    same compare form, but with both endpoints pinned to immutable shas
    (`pin_compare`) before either the review's download or the repro is formed. A
    raw `A...B` echoed into the repro would name whatever `A`/`B` are (e.g.
    `<sha>...master`), and re-fetching `compare/A...B.diff` later resolves them
    against the _current_ tips — a different diff, applied onto the same pinned
    anchor, silently reviewing a different tree while still exiting zero. Pinning
    both sides keeps the compare compact and re-fetchable yet immutable. An
    endpoint that is already a full 40-hex sha is content-addressed and immutable
    on its own, so it passes through as-is _without_ needing to exist in the local
    clone (`pin_endpoint`) — a compare can thus name a commit the clone never
    fetched (a fork's PR head, say) that GitHub still resolves in its fork
    network. Any other name (a branch, tag, short sha) is resolved in the local
    clone, so a name the clone lacks is a hard error, not a drift.
  - **working tree, or a file `--patch <path>`** → `--head <sha> --patch /dev/stdin`,
    where the diff has no durable re-fetchable identity (a local, unpushable
    working tree, or a diff file that won't exist elsewhere), so it rides along in
    the report as a heredoc piped straight in (`/dev/stdin` is just a path npb
    reads — no `-` special case). (For the working tree, fully-untracked files are
    excluded, the same `git stash create` limitation the live capture has — §6.)

**The compare download is cached, so a warm re-run is offline.** The scenario
that matters: person A posts a `--pr` report; person B pastes its repro command
(`… --head <fork> --patch <fork>...<head>`), runs it, then runs it _again_ — the
second run should touch no network. The `patch_tree` table (`src/store.rs`) maps
`(anchor, sha-pinned expr) → the head tree` npb reconstructed by applying that
compare onto the anchor. On a re-run npb looks it up (`resolve_compare_head`) and
**re-mints the synthetic head over the cached tree** instead of downloading — the
tree's git objects are still in the clone, held by the `refs/npb/worktree` the
first run wrote. Everything downstream (the merge, the tree-keyed eval, the
drvpath-keyed facts) is already cached, so the re-run is fully offline. Three
properties make this the right shape: it keys only on `(anchor, expr)`, both of
which are _in the reproduction command itself_, so it needs **no knowledge of the
original `--pr` run**; it stores **only a tree hash, never the diff** (no patches
in `~/.cache/nix-npb`, mirroring the no-patch-in-the-command choice above); and it
is re-derivable — if `git gc` has meanwhile reclaimed the tree, `commit-tree`
fails and npb simply downloads again (`--clean`/eviction likewise just costs a
re-download). It does _not_ cover a re-run that must **build** a drv it doesn't
yet know: building `fetchGit`s the head, which needs the tree _objects_, and a
hash isn't the objects — that path reconstructs from the diff (a download). But
that is new work, not re-running a finished review, and needs the network anyway.
(An embedded-diff repro would make even that offline, and a fresh machine's _first_
run too, but at the cost of a long diff in every `--pr` report — deliberately not
chosen.)

**Resolve mutable refs once.** A branch or `HEAD` can move mid-run, so npb
resolves each such ref to an immutable sha exactly once and thereafter passes only
that sha: the `--patch` anchor is resolved a single time, up front, then reused
for both the head it builds and the anchor it prints, and a compare's two
endpoints are pinned once (above) and reused for both the download and the repro.
Re-resolving the _same_ ref a second time would reintroduce this class of bug: the
head reviewed and the identity printed could disagree. A full sha re-checked
downstream is harmless — it is content-addressed and cannot resolve to anything
else.

Making `--patch` a real flag (rather than emitting the throwaway-index/`apply`/
`commit-tree` dance as shell) keeps the commands to a single `npb` call with no
external binary, and `--patch` is independently useful — "review a diff, or a
GitHub compare range, on top of a base." Its compare form is a deliberate
network fetch — but a _cached_ one (above): npb's network use is now narinfo
probes (§7), the `--pr` ref fetch, and a `--patch <A...B>` download _on a cache
miss_ — all explicit; the path form, a warm compare re-run, and every other flag
stay offline.

## 9. Build order (spine first; resist features until the spine carries weight)

The spine is implemented (✓).

1. ✓ cached `eval(commit, system)` → attr→drv map (`nix-eval-jobs`), evals run
   in parallel with an OOM-recovery ladder (§6).
2. ✓ the two-way diff: a base-branch tip vs the head merged onto it (a synthetic
   merge npb always mints locally — even under `--pr`, so a review and its repro
   compute the identical merge; §6), or the merge-base under `--no-merge` (§6).
3. ✓ the drvpath-keyed observation store + `BuildPolicy` + a local build driver
   that consults/appends it: one batched `nom` build, parallel cache probing,
   `DepFailed`/cascade detection.
4. ✓ `Cache` facts (narinfo), recorded as observations.
5. ✓ Markdown report classifying the changed set, building both sides first so
   there are no `?`.

All of the above is driven by a single `npb [base] [head]` command (the
eval/diff/build/report primitives are internal modules, not subcommands).

Open refinements: remote-builder fan-out; a `Local`-vs-`Cache` fidelity probe
(from-source build vs. substitution).

**Considered direction — a per-system pipeline over the whole pre-build graph.**
Today the phases up to the build run as global barriers: _all_ pairs enumerate →
_all_ eval → _all_ diffs → _all_ `--tests` → _all_ instantiate → probe → build. But
the real dependency graph is a fixed pipeline replicated per system and side —
`enumerate(c,s) → eval(c,s)`, then `diff(s) → tests(s) → instantiate(s) →
probe(s)` — with systems independent until the report. So a straggler (one slow
system, or a giant `haskellPackages` shard) stalls every _other_ system's
downstream phases behind it, even though they are data-independent. A pipeline
executor (à la a per-item `pipeline()` with no barrier between stages) would let a
fast system flow all the way to the build while a slow one is still evaluating —
the same "small atoms, drain idle slots, no straggler phase" argument as §6,
lifted from _within_ eval to _across_ phases. Two constraints shape it, and are
why this is **not** one universal worker pool:

- **Resource dimensions don't share a limit.** Eval/instantiate/enumerate are
  RAM-bound (the `slots`/AIMD queue above); the narinfo probe is network-bound
  (64 reused connections, no OOM notion). One pool can't serve both — the executor
  needs _typed_ resource pools, with the eval scheduler being the RAM pool.
- **The build barrier is a soundness constraint, not a nicety** (§5): a build
  co-scheduled with eval workers risks an OOM-killed build recorded as a false
  `Failed`. So "everything up to builds, concurrently" is exactly the right cut —
  the probe (network) may overlap freely, but no build starts until the RAM-heavy
  eval-class work has drained.

The prize is concentrated in the **cold-cache, multi-system** case; in the
warm-cache iterative loop npb is built for (§1) eval is instant and little
cross-phase slack remains, so this is gated on cold multi-system runs actually
hurting in practice — it is _not_ a general task-graph engine for what is really a
regular pipeline. The near-term, unconditionally-worthwhile piece of it — one
shared persistent progress tree (`live::Tree`, driven through `live::with_live`)
that every phase feeds nodes into — is already done (§6); the executor is the part
deferred until the cold-run wall-time justifies it.

One **display** slice of the pipeline is implemented ahead of the executor: the
`tests` phase's nodes appear per system _as each system's eval lands_, not after
a whole-set barrier. The instant a system has both its base and head eval files
(cached up front, or cold once evaluated), `run_phases` computes that system's
diff and — while the other systems are still evaluating — reveals its `tests`
leaves as blue/waiting nodes, spliced into the tree in fixed system order (a
later-ready system that sorts earlier is inserted _above_ an already-present one,
via `live::Tree::insert_sorted`; a system with no test-misses never appears). The
signal is a per-`(commit, system)` callback (`eval_two`'s `on_eval_done`) fired as
each eval file is written, plus an up-front firing for systems already cached;
the work runs off a coarse mutex on the eval worker threads (its `Store` lives
inside because `rusqlite` is `!Sync`). Crucially this is _display only_ — the
test-listing jobs themselves still run as **one grouped scheduler pass after all
eval finishes** (`eval::eval_tests` over the pre-created leaves), so nothing is
co-scheduled with eval; only the tree's appearance is early.

**Resolved gotcha (root-caused) — `nix-eval-jobs` restarted its worker after
every job on macOS.** The ~100× darwin slowdown (measured ~1.5 attrs/s on an
`aarch64-darwin` VM vs ~155 attrs/s on `aarch64-linux`, same hardware) was a
units bug in `nix-eval-jobs`' worker-restart check (`shouldRestart`,
`src/worker.cc`): it compared `getrusage`'s `ru_maxrss` against
`--max-memory-size` (MiB) × 1024, which is correct on Linux (`ru_maxrss` in
KiB) but off by 1024× on macOS (`ru_maxrss` in **bytes**). The effective cap
became `--max-memory-size` _KiB_, every worker tripped it after its first job,
and each job paid a fork + full nixpkgs re-import (~0.6 s each; also why "huge"
MB values didn't help — 999999 MB still read as ~1 GB). It was never a GC or
eval-engine problem: with the cap compensated ×1024, the same darwin VM
evaluated _faster_ than the Linux VM (7671 vs 5134 attrs/30 s, one worker).
Reported as [nix-eval-jobs#425](https://github.com/NixOS/nix-eval-jobs/issues/425)
and fixed by [nix-eval-jobs#426](https://github.com/NixOS/nix-eval-jobs/pull/426).
The flake pins a `nix-eval-jobs` that includes the fix (§4), so `stream_jobs`
(`src/eval.rs`) now passes `--max-memory-size` unscaled on every platform — the
former ×1024 macOS workaround is gone.

## 10. Resolved questions

Recorded for context:

- _Eval cache key_ → `(tree, system)`, on the git _tree_ not the commit (the
  eval depends only on source content), so a rebase/amend or a committed
  working tree is a cache hit and the uncommitted working tree is reviewable
  (§6); not a can of worms because `npb` owns the fixed config. No version tag —
  the format is held fixed rather than versioned, evolving only in ways an old
  and new npb both tolerate (§1); never a delete-and-regenerate, never a
  migration.
- _Concurrency_ → not handled. One machine is the driver and keeps its store
  local; multiple drivers keep independent stores, exactly as the Nix store
  already works. The append-only design stays friendly to revisiting this.
- _Cache facts lifetime_ → append-only observations, no eviction/TTL. A probe's
  `Built` observation records the drvpath, so staleness can't affect correctness (§3).
- _Remote facts_ → narinfo on `cache.nixos.org` only; Hydra was dropped (§7).
- _Storage_ → SQLite (`npb.sqlite`) under `dirs::cache_dir()/nix-npb`; all re-derivable cache (§4).

## 11. Progress display: color, interactivity, and the build monitor

The pre-build progress tree (§6, `live::Tree`/`with_live`) and the build monitor
(§5, `nom`) key off **one** predicate, resolved once through the `console` crate:
`live::colors_enabled` (→ `console::colors_enabled_stderr`, honoring `NO_COLOR`,
`CLICOLOR`, `CLICOLOR_FORCE`, and the TTY). It gates **both** color _and_
interactivity — the two are deliberately fused: rather than a third
monochrome-redraw mode, `NO_COLOR` takes the exact same plain path as a pipe.
(The informal `NO_COLOR` standard is strictly _color only_, so treating it as
"non-interactive" is a small deliberate over-reach for simplicity — one fewer
mode to carry, and a `NO_COLOR` user on a TTY still gets clean, readable output.)

So the pre-build tree has two modes, rendering the same node lines:

| stderr                                | mode                                                                                                                                                                                  |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| a color TTY                           | **interactive** — redraw in place, colored, windowed to the terminal height (overflow folds to a dim `⋯ N more`, §6); frozen to scrollback at the end                                 |
| piped, CI, an AI agent, or `NO_COLOR` | **plain** — no color, no cursor moves; each node's line printed once the moment it completes (a leaf on green, its parent headers lazily just before it), a resting footer at the end |

The plain append log (`Tree::emit_completed`) exists so a non-interactive run
gets _incremental_ output — and survives a mid-phase `^C` — where the redraw
would be silent until a final dump. It reads like the final interactive frame
minus color and animation, in completion order (the phases finish in order, so
the sections don't interleave).

The **build monitor** follows the same color axis: `nom` (which honors neither
`NO_COLOR` — [#129] — nor a non-TTY) runs **only when colorizing**. Otherwise
`batch_build` still parses nix's `internal-json` — that's what records each drv's
outcome incrementally, the ^C-safety of §5, independent of nom — but renders a
plain `building`/`built`/`failed` append log itself, matching the plain pre-build
mode — two columns, the event kind then the full `.drv` store path.

[#129]: https://github.com/maralorn/nix-output-monitor/issues/129
