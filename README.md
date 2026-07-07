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

- **local failures** — Nix retries a failed build every time; Hydra solved this
  with its global `FailedPaths` table. `npd` keeps the same memory for your loop.
- **eval diffs** — the attr→drv map of a revision is expensive and uncached.
- **Hydra facts** — job status, `narinfo` presence, and derivation *drift*.
- **reports** — human-readable Markdown over all of the above.

So `npd` is a thin **fact store + policy layer over `nix-eval-jobs` and
`nix build`**, not a fork of a review tool. See [`DESIGN.md`](DESIGN.md).

## Status

Early scaffolding. The pure data model (`npd/model.py`) is in place; the
orchestration (eval / diff / build / hydra / report) is stubbed and being
fleshed out spine-first — see the build order in `DESIGN.md`.
