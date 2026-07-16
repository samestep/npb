//! npd core data model.
//!
//! Pure data types the rest of npd is organized around. The guiding decision
//! (see DESIGN.md §2): build facts are keyed on the *derivation path* — the
//! stable identity of a build recipe. It survives failures (unlike an output
//! path) and is shared across commits automatically.
//!
//! Nothing here performs I/O or reads the clock; timestamps are passed in. That
//! keeps the model deterministic and trivially testable and lets the
//! orchestration layer own all the impurity.

/// A revision to evaluate, split into the two git identities it plays plus a
/// display label (DESIGN.md §6).
///
/// The eval is a pure function of the source **tree** — the checked-out file
/// content — not of the commit that carries it. A commit adds parents, an
/// author, a message, and timestamps, none of which the evaluation can observe:
/// `fetchGit`'s checkout has no `.git`, and npd forwards only the resulting
/// *path* into `import`. So the eval (and `--tests`) cache keys on [`tree`]: two
/// commits with the same tree share one eval — a rebase that doesn't touch the
/// changed files, a message-only `--amend`, a cherry-pick landing identical
/// content, and, crucially, committing an as-is working tree (so an
/// uncommitted-then-committed edit is a cache *hit*).
///
/// [`commit`] is a commit that realizes that tree, for `builtins.fetchGit`
/// (which fetches by commit, not by a bare tree). For a committed state it is
/// the real commit; for the uncommitted working tree it is a synthetic,
/// content-addressed commit minted over the tree. [`label`] is how the side is
/// shown to a human: the commit sha for a real revision, or `worktree`.
///
/// [`tree`]: Rev::tree
/// [`commit`]: Rev::commit
/// [`label`]: Rev::label
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rev {
    /// The git tree hash — the eval cache key.
    pub tree: String,
    /// A commit realizing `tree`, used as `builtins.fetchGit`'s `rev`.
    pub commit: String,
    /// Human-facing label (a commit sha, or `worktree`).
    pub label: String,
}

/// Result of evaluating one attribute on one platform at one commit.
///
/// Pure fact: fully determined by (tree, system, config). `drv_path` is
/// `None` when evaluation itself errored (assertion, IFD failure, …) — distinct
/// from a *build* failure, which is an [`Observation`]. The diff and report
/// deliberately render an errored attr as *absent* (➖): in a delta view an
/// eval breakage is visible as the attr disappearing, so no separate error
/// state is needed. `broken` folds
/// `meta.broken` / `meta.unsupported` / `meta.insecure` into one bit: the
/// profile's allow-flags let such a package evaluate to a drv anyway, but by
/// default it is not *built* (like nixpkgs-review) — see [`BuildPolicy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrEval {
    pub attr: String,
    pub drv_path: Option<String>,
    pub broken: bool,
}

/// One resolved `passthru.tests` entry from a targeted test eval (`--tests`).
///
/// Pure fact like [`AttrEval`], but decomposed for the per-package test cache:
/// `pkg_attr` is the package the test hangs off (the attr-path's first element),
/// `test_attr` is the full `<pkg>.tests.<name>` label, and `drv_path` is `None`
/// when the test errored (no derivation) — the same shape the full-set walk gives
/// an errored attr. `broken` is the test's own meta-blocked bit (broken /
/// unsupported-on-this-system / insecure) — a test can be unavailable even when
/// its package is fine (e.g. an x86-only NixOS test hung off a cross-platform
/// package on `aarch64-linux`), so it must be tracked per test, not inferred from
/// the package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestJob {
    pub pkg_attr: String,
    pub test_attr: String,
    pub drv_path: Option<String>,
    pub broken: bool,
}

/// Where a build observation came from. Local builds and substituter presence
/// are both observations in one append-only log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// We ran `nix build` on one of our machines.
    Local,
    /// narinfo presence on a substituter (success only).
    Cache,
}

/// The result of a single build attempt/observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Built,
    /// The derivation itself failed to build.
    Failed,
    /// A (transitive) dependency failed; this drv never ran.
    DepFailed,
}

/// One append-only fact about one derivation, keyed externally by `drv_path`.
///
/// We never overwrite an observation; flakiness is simply multiple observations
/// of the same `drv_path` with differing outcomes. `when` is unix seconds,
/// passed in by the caller (the model never reads the clock).
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub drv_path: String,
    pub source: Source,
    pub outcome: Outcome,
    pub when: i64,
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
    /// Marked broken/unsupported/insecure and `build_broken` is off — not
    /// attempted (like nixpkgs-review); the report shows it as 🚧.
    SkipBroken,
}

/// Turns a derivation's observation history into an action.
///
/// The ergonomic core (DESIGN.md §5): the cache-bypass knobs are just fields,
/// and [`BuildPolicy::decide`] is a pure predicate over the append-only log
/// plus whether the output is substitutable.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildPolicy {
    /// Rebuild even a previously-succeeded drv (suspect a flaky success).
    pub recheck: bool,
    /// Re-attempt a previously-failed drv (expect it might pass now).
    pub retry: bool,
    /// Ignore a substitutable (cached) success; require a genuine local build.
    pub prefer_local: bool,
    /// Also build packages marked broken/unsupported/insecure (skipped by
    /// default, like nixpkgs-review).
    pub build_broken: bool,
}

impl BuildPolicy {
    /// Decide whether to build `drv_path` given its observations.
    ///
    /// `substitutable` means a successful output is available from a substituter
    /// (Nix could fetch it without building) — a *success* signal that says
    /// nothing about local reproducibility. `broken` is the attr's
    /// meta-broken/unsupported/insecure bit from the eval.
    pub fn decide(
        &self,
        observations: &[Observation],
        substitutable: bool,
        broken: bool,
    ) -> Decision {
        let local: Vec<&Observation> = observations
            .iter()
            .filter(|o| o.source == Source::Local)
            .collect();
        let local_built = local.iter().any(|o| o.outcome == Outcome::Built);
        let local_failed_only = !local.is_empty()
            && local
                .iter()
                .all(|o| matches!(o.outcome, Outcome::Failed | Outcome::DepFailed));

        // Marked broken and not overridden: never attempt (checked before the
        // other knobs, so e.g. `--retry` alone still doesn't build it). A real
        // fact recorded earlier (a prior `--build-broken` run) still counts.
        if broken && !self.build_broken {
            return if local_built {
                Decision::SkipOk
            } else {
                Decision::SkipBroken
            };
        }
        // A trusted success short-circuits unless we're deliberately re-checking.
        if local_built && !self.recheck {
            return Decision::SkipOk;
        }
        if substitutable && !self.prefer_local && !self.recheck {
            return Decision::SkipOk;
        }
        // A known-failing derivation is not worth re-running unless asked.
        if local_failed_only && !self.retry {
            return Decision::SkipFail;
        }
        Decision::Build
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(source: Source, outcome: Outcome) -> Observation {
        Observation {
            drv_path: "/nix/store/x.drv".into(),
            source,
            outcome,
            when: 0,
        }
    }

    #[test]
    fn never_observed_builds() {
        assert_eq!(
            BuildPolicy::default().decide(&[], false, false),
            Decision::Build
        );
    }

    #[test]
    fn substitutable_skips_unless_prefer_local() {
        assert_eq!(
            BuildPolicy::default().decide(&[], true, false),
            Decision::SkipOk
        );
        let p = BuildPolicy {
            prefer_local: true,
            ..Default::default()
        };
        assert_eq!(p.decide(&[], true, false), Decision::Build);
    }

    #[test]
    fn local_success_skips_unless_recheck() {
        let o = [obs(Source::Local, Outcome::Built)];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false),
            Decision::SkipOk
        );
        let p = BuildPolicy {
            recheck: true,
            ..Default::default()
        };
        assert_eq!(p.decide(&o, false, false), Decision::Build);
    }

    #[test]
    fn only_failures_skip_unless_retry() {
        let o = [obs(Source::Local, Outcome::Failed)];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false),
            Decision::SkipFail
        );
        let p = BuildPolicy {
            retry: true,
            ..Default::default()
        };
        assert_eq!(p.decide(&o, false, false), Decision::Build);
    }

    #[test]
    fn flaky_success_wins() {
        let o = [
            obs(Source::Local, Outcome::Failed),
            obs(Source::Local, Outcome::Built),
        ];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false),
            Decision::SkipOk
        );
    }

    #[test]
    fn cache_success_does_not_count_as_local() {
        // A recorded Cache success is not a local build; without substitutable we
        // still build (the caller folds a prior Cache-built obs into substitutable).
        let o = [obs(Source::Cache, Outcome::Built)];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false),
            Decision::Build
        );
    }

    #[test]
    fn broken_skips_unless_build_broken() {
        // Marked broken: never attempted by default — not even when
        // substitutable, and not under --retry/--recheck alone.
        let p = BuildPolicy::default();
        assert_eq!(p.decide(&[], false, true), Decision::SkipBroken);
        assert_eq!(p.decide(&[], true, true), Decision::SkipBroken);
        let retry = BuildPolicy {
            retry: true,
            ..Default::default()
        };
        assert_eq!(
            retry.decide(&[obs(Source::Local, Outcome::Failed)], false, true),
            Decision::SkipBroken
        );
        let recheck = BuildPolicy {
            recheck: true,
            ..Default::default()
        };
        assert_eq!(recheck.decide(&[], false, true), Decision::SkipBroken);

        // A prior forced build's success is still a trusted fact.
        let o = [obs(Source::Local, Outcome::Built)];
        assert_eq!(p.decide(&o, false, true), Decision::SkipOk);

        // --build-broken restores the normal policy.
        let bb = BuildPolicy {
            build_broken: true,
            ..Default::default()
        };
        assert_eq!(bb.decide(&[], false, true), Decision::Build);
        assert_eq!(bb.decide(&o, false, true), Decision::SkipOk);
    }
}
