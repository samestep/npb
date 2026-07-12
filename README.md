# npd

A persistent **fact store** for iterating on nixpkgs changes across a set of
long-lived build machines, keyed on the identity of build *recipes*
(derivation paths).

`npd` is **not** a re-implementation of [`nixpkgs-review`](https://github.com/Mic92/nixpkgs-review).
nixpkgs-review reviews one PR, one-shot, and throws the workspace away. `npd`
optimizes for the opposite: a durable loop where you evaluate, build, and
re-build the same and related derivations many times over days, and never want
to repeat work you already know the answer to.

The Nix store + substituters already remember **successful** builds. What Nix
throws away is everything else `npd` cares about:

- **local failures** — Nix retries a failed build every time; `npd` remembers a
  failed drv (direct failure vs. dependency cascade) so your loop doesn't repeat it.
- **eval diffs** — the attr→drv map of a revision is expensive and uncached.
- **reports** — human-readable Markdown over all of the above.

The one remote fact `npd` consults is `cache.nixos.org` (is this exact drv already
built and substitutable?). So `npd` is a thin **fact store + policy layer over
`nix-eval-jobs` and `nix build`**, not a fork of a review tool. See
[`DESIGN.md`](DESIGN.md).

## Status

Rust (edition 2024, à la [`npc`](https://github.com/samestep/npc)). `npd` is a
single command: evaluate a `base → head` change, build whatever the changed set
needs, and render the report — **instant when the result is already known**.

```
npd [BASE] [HEAD]
```

With no arguments, `head` = `HEAD` and `base` = merge-base of `HEAD` and `master`.
It **builds whatever the states need** first (both sides of the changed set,
skipping anything already known, substitutable, or marked
broken/unsupported/insecure — the latter reported as 🚧, like nixpkgs-review),
then groups the result by its `before → after` delta (regression /
blocked-by-a-regression / newly-marked-broken / fixed / dropped / …), folded,
with drv-sharing attrs collapsed (`a = b = c`). Flags: `--no-build`
(render from existing facts only), `--recheck` / `--retry` / `--prefer-local`
(build-policy knobs), `--tests` (also build each changed package's
`passthru.tests`, on both sides — ported from
[nixpkgs-review#397](https://github.com/Mic92/nixpkgs-review/pull/397)),
`--build-broken` (build meta-blocked packages too), `--max` (everything on:
implies `--tests` and `--build-broken`),
`--system` (repeatable), `--nixpkgs`, and sizing knobs for the parallel
evaluator (`--eval-workers`, `--worker-mem-mb`). Under the hood: evals cached
as flat per-commit files (diffed by a streaming linear merge), a tiny SQLite
observation log, streamed `nix-eval-jobs` run in parallel across the machine's
cores (an eval that dies — usually a worker OOM — retries at half width), and
one batched `nom` build with concurrent cache probing.

## Development

Toolchain comes from the flake (like `npc`): `direnv allow`, or `nix develop`.

```sh
nix develop --command cargo test    # unit tests (an ignored end-to-end test needs real nix)
nix develop --command cargo run -- --help
```
