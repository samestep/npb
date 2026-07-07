"""npd core data model.

Pure data types the rest of npd is organized around. The guiding decision (see
DESIGN.md §2): build facts are keyed on the *derivation path* — the stable
identity of a build recipe. It survives failures (unlike an output path) and is
shared across commits automatically.

Nothing here performs I/O, spawns processes, or reads the clock; timestamps are
passed in. That keeps the model deterministic and trivially testable, and lets
the orchestration layer own all the impurity.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum


class Existence(Enum):
    """Whether an attribute evaluates to something buildable on a platform.

    Determined purely by evaluation, before any build is attempted."""

    ABSENT = "absent"  # attr path not present in this revision
    BLOCKED = "blocked"  # present but meta.broken / badPlatforms / unsupported / insecure
    BUILDABLE = "buildable"  # present and eligible to build on this platform


@dataclass(frozen=True)
class AttrEval:
    """Result of evaluating one attribute on one platform at one commit.

    Pure fact: fully determined by (commit, system, config). ``drv_path`` is set
    iff ``existence`` is BUILDABLE. The meta flags are cached for later
    classification and may be None when we did not (or could not) determine them.
    """

    attr: str
    existence: Existence
    drv_path: str | None = None
    broken: bool | None = None
    unsupported: bool | None = None
    # per meta.hydraPlatforms for this system — whether Hydra is *expected* to build it
    hydra_platforms_ok: bool | None = None


class Source(Enum):
    """Where a build observation came from."""

    LOCAL = "local"  # we ran `nix build` on one of our machines
    HYDRA_JOB = "hydra-job"  # Hydra's build record for a named job (forward lookup)
    CACHE = "cache"  # narinfo presence on a substituter (success only)


class Outcome(Enum):
    """The result of a single build attempt/observation."""

    BUILT = "built"
    FAILED = "failed"  # the derivation itself failed to build
    DEP_FAILED = "dep-failed"  # a (transitive) dependency failed; this drv never ran
    NOT_ATTEMPTED = "not-attempted"  # source has no record (e.g. narinfo 404, queued job)


@dataclass(frozen=True)
class Observation:
    """One append-only fact about one derivation, keyed externally by drv_path.

    We never overwrite an observation; flakiness is simply multiple observations
    of the same drv_path with differing outcomes. ``when`` is unix seconds,
    passed in by the caller (the model never reads the clock).
    """

    drv_path: str
    source: Source
    outcome: Outcome
    when: int
    system: str | None = None
    duration_s: float | None = None  # ~0 for a substituted/cached result
    cached: bool | None = None  # Hydra isCachedBuild / substituted rather than genuinely run
    machine: str | None = None
    log_ref: str | None = None  # path under $NPD_STATE/logs, if any
    build_id: int | None = None  # Hydra build id, when source is HYDRA_JOB


class Decision(Enum):
    """What the build policy says to do about a derivation."""

    BUILD = "build"  # never observed, or explicitly forced -> attempt it
    SKIP_OK = "skip-ok"  # a trusted success exists -> don't rebuild
    SKIP_FAIL = "skip-fail"  # only failures observed -> don't waste time (unless retry)


@dataclass(frozen=True)
class BuildPolicy:
    """Turns a derivation's observation history into an action.

    This is the ergonomic core (DESIGN.md §5): the cache-bypass knobs the
    workflow needs are just fields, and ``decide`` is a pure predicate over the
    append-only log plus whether the output is substitutable.
    """

    recheck: bool = False  # rebuild even if previously succeeded (suspect flaky success)
    retry: bool = False  # rebuild even if previously failed (expect it might pass now)
    prefer_local: bool = False  # ignore CACHE/HYDRA success; require a genuine LOCAL build

    def decide(
        self, observations: list[Observation], substitutable: bool = False
    ) -> Decision:
        """Decide whether to build ``drv_path`` given its observations.

        ``substitutable`` means a successful output is available from a
        substituter (i.e. Nix could fetch it without building) — a *success*
        signal that says nothing about local reproducibility.
        """
        local = [o for o in observations if o.source is Source.LOCAL]
        local_built = any(o.outcome is Outcome.BUILT for o in local)
        local_failed_only = bool(local) and all(
            o.outcome in (Outcome.FAILED, Outcome.DEP_FAILED) for o in local
        )

        # A trusted success short-circuits unless we're deliberately re-checking.
        if local_built and not self.recheck:
            return Decision.SKIP_OK
        if substitutable and not self.prefer_local and not self.recheck:
            return Decision.SKIP_OK

        # A known-failing derivation is not worth re-running unless asked.
        if local_failed_only and not self.retry:
            return Decision.SKIP_FAIL

        return Decision.BUILD
