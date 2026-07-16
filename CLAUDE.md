# npd

Read DESIGN.md before making non-trivial changes; it is the source of truth for
the architecture and is kept up to date as part of any change that affects it.

## Finishing a change: `nix flake check`, then commit to `main`

Development happens on `main` — don't create feature branches or open pull
requests. Committing your work to `main` is part of *finishing* the task, not a
separate step to wait for permission on.

Before committing, run `nix flake check`: it is the source of truth for "does
this pass". It builds the crate, runs the tests, runs Clippy
(`--all-targets -- --deny warnings`), and checks formatting — so a lone
warning, an unformatted line, or a broken `examples/` file fails it even when
`cargo build` is happy. Local `cargo` can drift from it; the flake is what
counts.

One non-obvious gotcha: **Nix builds from the git tree, so it never sees
untracked files.** `git add` any new file (a new `src/*.rs` module, an
`examples/` entry) *before* running the check, or the build fails with a
confusing `E0583` ("file not found for module") and cascade noise — the module
simply isn't in the source Nix copied.

## Commit messages: `Assisted-by:`, no session links

Attribute Claude assistance with an `Assisted-by:` trailer naming the model,
and nothing else:

```
Assisted-by: Claude:opus-4.8
Assisted-by: Claude:fable-5
```

Do **not** use `Co-Authored-By: Claude … <noreply@anthropic.com>` trailers, and
do **not** include links to Claude Code sessions (`Claude-Session:` lines or
`claude.ai/code/…` URLs) — the whole history has been rewritten to this
convention, so keep new commits consistent with it.

## No backward compatibility, ever

npd has exactly one user, no releases, and no deployments. Everything it stores
(`~/.cache/nix-npd`) is a re-derivable cache. Therefore (DESIGN.md §1):

- Never write migration code: no SQLite schema upgrades or `ALTER TABLE`/purge
  steps for data an older version wrote, no readers or fallbacks for previous
  file formats, no comments like "may linger on old databases".
- Change formats in place. If existing cached data would become wrong to read,
  invalidate instead of migrating: delete `~/.cache/nix-npd` (it is all
  re-derivable) and let the next run regenerate it.
- When removing a feature, remove all of it in the same change — enum variants,
  struct fields, table columns, parsing, tests, and doc references. Don't keep
  dead fields around because they might be useful later.
