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
- **build logs** — kept for every build, success *and* failure.
- **reports** — human-readable Markdown over all of the above.

The one remote fact `npd` consults is `cache.nixos.org` (is this exact drv already
built and substitutable?). So `npd` is a thin **fact store + policy layer over
`nix-eval-jobs` and `nix build`**, not a fork of a review tool. See
[`DESIGN.md`](DESIGN.md).

## Status

Rust (edition 2024, à la [`npc`](https://github.com/samestep/npc)). Implemented
end-to-end; every command is **instant when its result is already known**.

- `npd eval <commit>` — cached attr→drv map (SQLite; streamed `nix-eval-jobs`,
  evals run in parallel under a RAM budget).
- `npd diff <base> <head> [--three-way]` — changed/added/removed, with merge-base
  attribution.
- `npd build <commit> <attrs…>` / `--changed <base>` — observation-backed build
  driver (remembers successes *and failures*, keeps logs), one batched `nom`
  build, parallel cache probing, `--dry-run` / `--recheck` / `--retry` /
  `--prefer-local`.
- `npd report [base] [head]` — groups the changed set by its `before → after`
  delta (regression / blocked-by-a-regression / fixed / dropped / …), folded and
  with drv-sharing attrs collapsed (`a = b = c`). With no args, `head` = `HEAD`
  and `base` = merge-base of `HEAD` and `master`; it **builds whatever the states
  need** first (both sides), so there are no unknowns. `--no-build` renders from
  existing facts only.

## Development

Toolchain comes from the flake (like `npc`): `direnv allow`, or `nix develop`.

```sh
nix develop --command cargo test    # run the model tests
nix develop --command cargo run -- --help
```
