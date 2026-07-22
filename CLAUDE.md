# npb

Read DESIGN.md before making non-trivial changes; it is the source of truth for
the architecture and is kept up to date as part of any change that affects it.

## Finishing a change: `nix flake check`, then commit to `main`

Development happens on `main` — don't create feature branches or open pull
requests. Committing your work to `main` is part of _finishing_ the task, not a
separate step to wait for permission on.

Before committing, run `nix flake check`: it is the source of truth for "does
this pass". It builds the crate, runs the tests, runs Clippy
(`--all-targets -- --deny warnings`), and checks formatting — so a lone
warning, an unformatted line, or a broken `examples/` file fails it even when
`cargo build` is happy. Local `cargo` can drift from it; the flake is what
counts.

One non-obvious gotcha: **Nix builds from the git tree, so it never sees
untracked files.** `git add` any new file (a new `src/*.rs` module, an
`examples/` entry) _before_ running the check, or the build fails with a
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

## No breaking changes, and still no migration code

npb is public now, so we're committed to the formats and interfaces we've
already chosen (DESIGN.md §1). Two constraints hold at once — the second is the
one that's easy to forget:

- We no longer just change a format when it's convenient. The on-disk store at
  `~/.cache/nix-npb` (the SQLite schema, the eval-file format) lives on users'
  machines and holds history re-derivable only by re-running the builds and
  probes behind it, and a current npb must read a store an older npb wrote.
- We still write **no migration code**: no SQLite schema upgrades or `ALTER
  TABLE`/purge steps for data an older version wrote, no readers or fallbacks
  for previous file formats, no "may linger on old databases" comments. That
  rule hasn't relaxed — what's gone is the old escape hatch of changing the
  format in place and hand-migrating the single local store.

With neither "just change it" nor "migrate it" on the table, the move is
**restraint**: don't make a format change that would need a migration. Evolve a
stored format only additively (something an old and new npb both tolerate) or
leave it alone; never delete or invalidate the store. Likewise the CLI (flags,
subcommands) and the report format are interfaces users script against — keep
them stable and grow them additively.

When removing a feature, still remove all of its dead code in the same change
(enum variants, struct fields, table columns, parsing, tests, doc references),
but a removal that changes a stored format or a user-facing interface is a
breaking change, held to the rule above.
