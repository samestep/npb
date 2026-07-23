//! npb core data model.
//!
//! Pure data types the rest of npb is organized around. The guiding decision
//! (see DESIGN.md §2): build facts are keyed on the *derivation path* — the
//! stable identity of a build recipe. It survives failures (unlike an output
//! path) and is shared across commits automatically.
//!
//! Nothing here performs I/O. That keeps the model deterministic and trivially
//! testable and lets the orchestration layer own all the impurity.

/// A revision to evaluate, split into the two git identities it plays plus a
/// display label (DESIGN.md §6).
///
/// The eval is a pure function of the source **tree** — the checked-out file
/// content — not of the commit that carries it. A commit adds parents, an
/// author, a message, and timestamps, none of which the evaluation can observe:
/// `fetchGit`'s checkout has no `.git`, and npb forwards only the resulting
/// *path* into `import`. So the eval (and `--tests`) cache keys on [`tree`]: two
/// commits with the same tree share one eval — a rebase that doesn't touch the
/// changed files, a message-only `--amend`, a cherry-pick landing identical
/// content, and, crucially, committing an as-is working tree (so an
/// uncommitted-then-committed edit is a cache *hit*).
///
/// [`commit`] is a commit that realizes that tree, for `builtins.fetchGit`
/// (which fetches by commit, not by a bare tree). For a committed state it is
/// the real commit; for the uncommitted working tree it is a synthetic,
/// content-addressed commit minted over the tree. [`label`] identifies the
/// side: the commit sha for a real revision, or `worktree` for a synthetic
/// working-tree/patch head — the report renders the latter as its anchor commit
/// with a trailing `\*` ("this commit, plus a diff"), not the bare word.
///
/// [`display`] is the *human* name of the side for the live progress tree
/// ([`crate::live`]): the ref the user actually expressed (or the default's
/// name) rather than a resolved sha — `master`, `HEAD`, a branch, `#431 base` —
/// and, for a commit npb *derives*, an honest description of it: `merge(a, b)`
/// for a synthetic merge, `merge-base(a, b)` for a `--no-merge` fork point,
/// `HEAD*` for a working-tree/patch head. It describes the tree actually
/// evaluated (DESIGN §6), so a sha appears only if the user typed one. Distinct
/// from [`label`] precisely because `label` is a real committish the repro path
/// feeds to `git`, and the report heading keeps showing it as a sha (GitHub
/// auto-links it); the tree wants the friendly form.
///
/// [`tree`]: Rev::tree
/// [`commit`]: Rev::commit
/// [`label`]: Rev::label
/// [`display`]: Rev::display
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rev {
    /// The git tree hash — the eval cache key.
    pub tree: String,
    /// A commit realizing `tree`, used as `builtins.fetchGit`'s `rev`.
    pub commit: String,
    /// Identity label: a commit sha, or `worktree` for a synthetic
    /// working-tree/patch head (rendered as its anchor commit + `\*`).
    pub label: String,
    /// Human name of this side for the live progress tree (see type docs).
    pub display: String,
}

/// An evaluation profile: which meta-blocked packages nixpkgs' `check-meta` is
/// told to *allow* rather than throw on (DESIGN §6). npb evaluates under the
/// profile the user means — strict by default — so a broken/unsupported/insecure
/// package (and anything that forces such a dependency) simply throws and falls
/// out, precisely and for free, with no separate meta bit to reconstruct.
/// `allowUnfree` is always on (matching nixpkgs-review), so it isn't a profile
/// axis.
///
/// The profile is part of the eval cache key: it prefixes the on-disk system
/// token ([`Profile::qualify`]) so evals under different profiles never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub broken: bool,
    pub unsupported: bool,
    pub insecure: bool,
}

impl Profile {
    /// The canonical three-character profile token: one position each for
    /// unsupported / broken / insecure, its letter when allowed else `-`. So
    /// strict is `---`, allow-everything `ubi`, allow-unsupported-and-insecure
    /// `u-i`. Deterministic and filesystem-safe — it names the profile's eval
    /// subtree on disk.
    pub fn token(&self) -> String {
        let f = |on, c| if on { c } else { '-' };
        format!(
            "{}{}{}",
            f(self.unsupported, 'u'),
            f(self.broken, 'b'),
            f(self.insecure, 'i'),
        )
    }

    /// The storage key for an eval on `system` under this profile:
    /// `<token>/<system>` (DESIGN §4). It is both the on-disk eval-file directory
    /// (`evalfile::eval_path`) and the `eval_key.system` column value, so the
    /// `--tests` cache keys on the profile for free — no schema change. The
    /// embedded `/` makes it two path segments when joined onto the cache root.
    pub fn qualify(&self, system: &str) -> String {
        format!("{}/{}", self.token(), system)
    }
}

/// Result of evaluating one attribute on one platform at one commit under a
/// given [`Profile`].
///
/// Pure fact: fully determined by `(tree, system, profile)`. `drv_path` is
/// `None` when the attr **threw** during evaluation — because it is
/// broken/unsupported/insecure under the profile, or forces a dependency that
/// is (nix-eval-jobs emits the attr with an `error` and no `drvPath`). The
/// report renders such a side as ⏩ ("present, but doesn't evaluate under this
/// profile"), distinct from a *missing* attr — no row at all — which is ➖
/// absent. There is no separate meta bit: evaluating under the profile you mean
/// makes the throw itself the signal (DESIGN §6), and a threw side is a pure
/// eval fact, so it never depends on build history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrEval {
    pub attr: String,
    pub drv_path: Option<String>,
}

/// One resolved `passthru.tests` entry from a targeted test eval (`--tests`).
///
/// Pure fact like [`AttrEval`], but decomposed for the per-package test cache:
/// `pkg_attr` is the package the test hangs off (the attr-path's first element),
/// `test_attr` is the full `<pkg>.tests.<name>` label, and `drv_path` is `None`
/// when the test errored (no derivation) — the same shape the full-set walk gives
/// a threw attr. A test unavailable under the profile (unsupported/insecure) is
/// dropped during eval (`build_tests_expr`), so it yields no job at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestJob {
    pub pkg_attr: String,
    pub test_attr: String,
    pub drv_path: Option<String>,
}

/// The result of a single build attempt/observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The drv's outputs were observed valid: a local build succeeded, or a
    /// substituter has them (a narinfo probe hit, DESIGN.md §7). The log
    /// deliberately doesn't record which — a success is a success.
    Built,
    /// The derivation itself failed to build.
    Failed,
    /// A (transitive) dependency failed; this drv never ran.
    DepFailed,
}

/// One append-only fact about one derivation, keyed externally by `drv_path`.
///
/// We never overwrite an observation; flakiness is simply multiple observations
/// of the same `drv_path` with differing outcomes. Rows carry no timestamp —
/// the log is append-only, so insertion order *is* the history.
///
/// `blocker` holds the output paths whose store validity re-decides this fact
/// (DESIGN.md §5), populated for the two failure outcomes: for a
/// [`Outcome::DepFailed`] it is the *specific* still-failing dependency that
/// blocked this drv (the "culprit"); for an [`Outcome::Failed`] it is the drv's
/// *own* outputs. Either way it makes the failure *self-healing* without
/// re-evaluation: a later run re-checks those paths offline — no `.drv`, no
/// closure walk — and the moment they are valid (the culprit built/substituted,
/// or the drv itself built out of band) the stale failure is overridden. Empty
/// for a `Built`, and for a failure whose paths weren't recorded
/// (treated conservatively as still-failing).
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub drv_path: String,
    pub outcome: Outcome,
    /// Paths whose validity re-decides this fact: a `DepFailed`'s culprit
    /// outputs, or a `Failed`'s own outputs; else empty.
    pub blocker: Vec<String>,
}

/// What the build policy says to do about a derivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Never observed, or explicitly forced — attempt it.
    Build,
    /// A trusted success exists — don't rebuild.
    SkipOk,
    /// Only failures observed — don't waste time (unless `retry`).
    SkipFail,
}

/// Turns a derivation's observation history into an action.
///
/// The ergonomic core (DESIGN.md §5): the cache-bypass knobs are just fields,
/// and [`BuildPolicy::decide`] is a pure predicate over the append-only log
/// plus whether the output is substitutable. Meta-blocked packages never reach
/// the build policy at all — they threw during eval under the profile, so they
/// have no drv and produce no build target (DESIGN §6).
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildPolicy {
    /// Re-attempt a previously-failed drv (expect it might pass now).
    pub retry: bool,
}

impl BuildPolicy {
    /// Decide whether to build `drv_path` given its observations.
    ///
    /// Substituter presence needs no input of its own: a cache probe's hit is
    /// recorded as a plain `Built` observation (DESIGN.md §7), so it decides
    /// here exactly like any other success.
    ///
    /// `dep_block_stale` distinguishes the two kinds of recorded failure
    /// (DESIGN.md §5). A **direct** failure (the drv's own build failed) is
    /// sticky: presumed to keep failing, `--retry` to re-attempt. A
    /// **dependency block** (`DepFailed`) is only trusted while the blocking
    /// dependency is *still* failing; once that culprit has built or been
    /// substituted, the block is stale and the caller passes
    /// `dep_block_stale = true` so we re-attempt — no `--retry` needed. The
    /// caller computes staleness by re-checking the culprit's store validity
    /// (`Observation::blocker`), which the pure predicate can't do itself.
    pub fn decide(&self, observations: &[Observation], dep_block_stale: bool) -> Decision {
        let built = observations.iter().any(|o| o.outcome == Outcome::Built);
        let direct_failed = observations.iter().any(|o| o.outcome == Outcome::Failed);
        let dep_failed = observations.iter().any(|o| o.outcome == Outcome::DepFailed);

        // A trusted success short-circuits.
        if built {
            return Decision::SkipOk;
        }
        // A known-failing derivation is not worth re-running unless asked. A
        // direct failure is sticky; a dependency block only holds while its
        // culprit is still failing (`!dep_block_stale`).
        if !self.retry {
            if direct_failed {
                return Decision::SkipFail;
            }
            if dep_failed && !dep_block_stale {
                return Decision::SkipFail;
            }
        }
        Decision::Build
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(outcome: Outcome) -> Observation {
        Observation {
            drv_path: "/nix/store/x.drv".into(),
            outcome,
            blocker: Vec::new(),
        }
    }

    #[test]
    fn never_observed_builds() {
        assert_eq!(BuildPolicy::default().decide(&[], false), Decision::Build);
    }

    #[test]
    fn recorded_success_skips() {
        // A Built fact — a local build or a recorded cache hit, which the log
        // doesn't distinguish (DESIGN.md §7) — decides SkipOk.
        let o = [obs(Outcome::Built)];
        assert_eq!(BuildPolicy::default().decide(&o, false), Decision::SkipOk);
    }

    #[test]
    fn only_failures_skip_unless_retry() {
        let o = [obs(Outcome::Failed)];
        assert_eq!(BuildPolicy::default().decide(&o, false), Decision::SkipFail);
        let p = BuildPolicy { retry: true };
        assert_eq!(p.decide(&o, false), Decision::Build);
    }

    #[test]
    fn flaky_success_wins() {
        let o = [obs(Outcome::Failed), obs(Outcome::Built)];
        assert_eq!(BuildPolicy::default().decide(&o, false), Decision::SkipOk);
    }

    #[test]
    fn dep_block_holds_until_culprit_heals() {
        // A dependency block (DepFailed, no direct failure) is skipped while its
        // culprit still fails, but re-attempted the moment the block goes stale —
        // no --retry needed. This is the self-healing property (DESIGN.md §5).
        let o = [obs(Outcome::DepFailed)];
        assert_eq!(BuildPolicy::default().decide(&o, false), Decision::SkipFail);
        assert_eq!(BuildPolicy::default().decide(&o, true), Decision::Build);
    }

    #[test]
    fn direct_failure_stays_sticky_even_when_a_dep_block_is_stale() {
        // A drv that failed *directly* is sticky regardless of dep staleness: a
        // stale sibling dep-block must not resurrect a real direct failure.
        // --retry is the only escape.
        let o = [obs(Outcome::Failed), obs(Outcome::DepFailed)];
        assert_eq!(BuildPolicy::default().decide(&o, true), Decision::SkipFail);
        let retry = BuildPolicy { retry: true };
        assert_eq!(retry.decide(&o, true), Decision::Build);
    }

    #[test]
    fn profile_token_and_qualify() {
        // The three-character token: unsupported / broken / insecure, letter when
        // allowed else `-`. Strict is all dashes; allow-everything spells `ubi`.
        let strict = Profile {
            broken: false,
            unsupported: false,
            insecure: false,
        };
        let all = Profile {
            broken: true,
            unsupported: true,
            insecure: true,
        };
        let ui = Profile {
            broken: false,
            unsupported: true,
            insecure: true,
        };
        assert_eq!(strict.token(), "---");
        assert_eq!(all.token(), "ubi");
        assert_eq!(ui.token(), "u-i");
        // The storage key prefixes the system with the token.
        assert_eq!(strict.qualify("x86_64-linux"), "---/x86_64-linux");
        assert_eq!(all.qualify("aarch64-darwin"), "ubi/aarch64-darwin");
    }
}
