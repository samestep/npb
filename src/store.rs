//! The SQLite fact store, in `~/.cache/nix-npd/npd.sqlite` (DESIGN.md §3–§4): the
//! append-only observation log and the `--tests` eval cache. Full-set evals do
//! *not* live here — they're standalone files (see `eval.rs`) — so what remains
//! is only the small, index-worthy data an engine actually earns.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::evalfile::{restore_drv, strip_drv};
use crate::model::{Observation, Outcome, Source, TestJob};

// No migrations, ever (CLAUDE.md): change this schema freely and in place. The
// whole store is a re-derivable cache, so the remedy for an incompatible
// change is deleting `~/.cache/nix-npd`, never a compat shim.
const SCHEMA: &str = "
-- The append-only observation log (DESIGN.md §3): the build driver appends a
-- `local`/`cache` fact here per drv. `drv_path` is stored stripped of its
-- constant `/nix/store/` prefix and `.drv` suffix (`evalfile::strip_drv`), and
-- `blocker`'s output paths of their `/nix/store/` prefix (`strip_out` — an
-- output path has no `.drv`); both restored on read. `source` and `outcome` are
-- small integer enum codes (`source_code`/`outcome_code`), not their English
-- labels. This is the one append-only, never-evicted table, so trimming its
-- per-row bytes is what compounds over time.
CREATE TABLE IF NOT EXISTS observation (
    id         INTEGER PRIMARY KEY,
    drv_path   TEXT    NOT NULL,
    source     INTEGER NOT NULL,
    outcome    INTEGER NOT NULL,
    when_      INTEGER NOT NULL,
    -- For a `dep-failed`: newline-joined output paths of the culprit dependency
    -- (DESIGN.md §5), so a later run can re-check the block's validity offline.
    -- NULL for every other outcome.
    blocker    TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS observation_drv ON observation (drv_path);

-- The `--tests` passthru.tests eval cache (DESIGN.md §4, §6). A test's drv is a
-- pure function of (tree, system, package-attr) — the source *tree*, not the
-- commit (see `model::Rev`) — so we cache per package and reuse across reviews at
-- a tree (a rebase/amend, or committing an as-is working tree, all hit).
--
-- The `(tree, system)` an eval belongs to is *interned* into `eval_key` and
-- referenced by its small integer `id`, rather than repeated as a 40-char tree
-- hash + system string on every row of both the table and its index. A handful
-- of distinct keys back thousands of test rows, so this is ~25% off the whole
-- `--tests` cache on real data (the biggest lever; DESIGN.md §4). It's also the
-- eviction unit: dropping an eval file (`--clean`) purges its key here, cascading
-- to the rows below.
CREATE TABLE IF NOT EXISTS eval_key (
    id     INTEGER PRIMARY KEY,
    tree   TEXT NOT NULL,
    system TEXT NOT NULL,
    UNIQUE (tree, system)
) STRICT;

-- `test_pkg` marks a package fully evaluated (present even when it has zero
-- tests, so a no-test package isn't re-evaluated every run); `test_drv` holds
-- each resolved `<pkg>.tests.<name>` drv (a package may contribute zero rows).
-- Drv paths are stored *stripped* of their constant `/nix/store/` prefix and
-- `.drv` suffix, exactly like the eval files (`evalfile::strip_drv`) — restored
-- on read.
-- `skipped` is the test's own meta-blocked bit (a test can be unsupported on this
-- system even when its package builds — an x86-only NixOS test on aarch64), so
-- it's stored per test, not inferred from the package.
CREATE TABLE IF NOT EXISTS test_pkg (
    key_id   INTEGER NOT NULL REFERENCES eval_key (id),
    pkg_attr TEXT NOT NULL,
    PRIMARY KEY (key_id, pkg_attr)
) STRICT, WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS test_drv (
    key_id    INTEGER NOT NULL REFERENCES eval_key (id),
    pkg_attr  TEXT NOT NULL,
    test_attr TEXT NOT NULL,
    drv_path  TEXT NOT NULL,
    skipped   INTEGER NOT NULL,
    PRIMARY KEY (key_id, test_attr)
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS test_drv_pkg ON test_drv (key_id, pkg_attr);
";

// `source`/`outcome` persist as small integer enum codes, not English labels.
// The values are stable on-disk (the cache is re-derivable, so they need no
// migration — but keeping them fixed keeps the `failing_drvs` query below and
// any hand-inspection legible). Changing a variant's code is a format change:
// delete `~/.cache/nix-npd` (CLAUDE.md), never migrate.
fn source_code(s: Source) -> i64 {
    match s {
        Source::Local => 0,
        Source::Cache => 1,
    }
}

fn source_from_code(c: i64) -> Result<Source> {
    Ok(match c {
        0 => Source::Local,
        1 => Source::Cache,
        other => anyhow::bail!("unknown observation source code in store: {other}"),
    })
}

fn outcome_code(o: Outcome) -> i64 {
    match o {
        Outcome::Built => 0,
        Outcome::Failed => 1,
        Outcome::DepFailed => 2,
    }
}

fn outcome_from_code(c: i64) -> Result<Outcome> {
    Ok(match c {
        0 => Outcome::Built,
        1 => Outcome::Failed,
        2 => Outcome::DepFailed,
        other => anyhow::bail!("unknown observation outcome code in store: {other}"),
    })
}

/// Strip the constant `/nix/store/` prefix from a store *output* path — a
/// `blocker` culprit path (DESIGN.md §5). Unlike a drv it has no `.drv` suffix,
/// so `evalfile::strip_drv` (prefix *and* suffix) doesn't fit; [`restore_out`]
/// re-adds the prefix. Every blocker is a real store path, so this is only ever
/// called on one (the `debug_assert` catches a stray non-store path in dev).
fn strip_out(p: &str) -> &str {
    debug_assert!(
        p.starts_with("/nix/store/"),
        "blocker output path not under /nix/store: {p}"
    );
    p.strip_prefix("/nix/store/").unwrap_or(p)
}

/// Reconstruct a full output path from its stored (prefix-stripped) form — [`strip_out`]'s inverse.
fn restore_out(p: &str) -> String {
    format!("/nix/store/{p}")
}

/// A comma-joined run of `n` SQL bind placeholders (`?,?,…`) for an `IN (…)` clause.
fn placeholders(n: usize) -> String {
    std::iter::repeat_n("?", n).collect::<Vec<_>>().join(",")
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).context("creating cache directory")?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        // WAL: readers don't block the writer; better for a durable local store.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        // Two *writers* still conflict (even in WAL), and SQLite's default busy
        // handler fails immediately — a second npd (or the report path's Cache
        // writes) would abort a batch mid-run with SQLITE_BUSY. Waiting out the
        // other writer's millisecond-scale autocommit is always right here.
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .context("setting busy_timeout")?;
        conn.execute_batch(SCHEMA).context("initializing schema")?;
        Ok(Self { conn })
    }

    /// Append one observation to the log (never overwrites; DESIGN.md §3).
    pub fn add_observation(&mut self, o: &Observation) -> Result<()> {
        // Drv path and blocker outputs are stored with their `/nix/store/…`
        // affixes stripped (like the eval files and the `--tests` cache),
        // restored on read. Store paths never contain a newline, so the blocker's
        // newline join round-trips losslessly; an empty blocker is NULL (only
        // `dep-failed` carries one).
        let blocker: Option<String> = if o.blocker.is_empty() {
            None
        } else {
            Some(
                o.blocker
                    .iter()
                    .map(|p| strip_out(p))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        };
        self.conn.execute(
            "INSERT INTO observation \
             (drv_path, source, outcome, when_, blocker) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                strip_drv(&o.drv_path),
                source_code(o.source),
                outcome_code(o.outcome),
                o.when,
                blocker,
            ],
        )?;
        Ok(())
    }

    /// All observations for a derivation, oldest first.
    pub fn load_observations(&self, drv_path: &str) -> Result<Vec<Observation>> {
        Ok(self
            .load_observations_many(std::slice::from_ref(&drv_path))?
            .remove(drv_path)
            .unwrap_or_default())
    }

    /// Load observations for many drvs in one query (oldest first per drv). Drvs
    /// with no observations are simply absent from the map. This is how a report
    /// or build over a whole changed set stays a single round-trip to SQLite
    /// rather than one query per target.
    pub fn load_observations_many(
        &self,
        drv_paths: &[&str],
    ) -> Result<std::collections::HashMap<String, Vec<Observation>>> {
        let mut out: std::collections::HashMap<String, Vec<Observation>> =
            std::collections::HashMap::new();
        if drv_paths.is_empty() {
            return Ok(out);
        }
        // `WHERE drv_path IN (?,?,…)` with one placeholder per drv.
        let placeholders = placeholders(drv_paths.len());
        let sql = format!(
            "SELECT drv_path, source, outcome, when_, blocker \
             FROM observation WHERE drv_path IN ({placeholders}) ORDER BY when_, id",
        );
        let mut stmt = self.conn.prepare(&sql)?;
        // The column is stored stripped, so match against the stripped query keys.
        let params = rusqlite::params_from_iter(drv_paths.iter().map(|d| strip_drv(d)));
        let rows = stmt.query_map(params, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (stored_drv, source, outcome, when, blocker) = row?;
            let drv_path = restore_drv(Some(&stored_drv)).expect("Some maps to Some");
            out.entry(drv_path.clone()).or_default().push(Observation {
                drv_path,
                source: source_from_code(source)?,
                outcome: outcome_from_code(outcome)?,
                when,
                blocker: blocker
                    .filter(|s| !s.is_empty())
                    .map(|s| s.split('\n').map(restore_out).collect())
                    .unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// Every drv whose *local* history is failures-only — at least one local
    /// observation, none of them (nor a `Cache` hit) a success. This is exactly
    /// the `local_failed_only` condition [`crate::model::BuildPolicy::decide`]
    /// applies per drv, lifted to a set so the build driver can propagate a known
    /// failure *forward* through the dependency graph (DESIGN.md §5): any target
    /// whose build closure contains such a drv would only `DepFail`, so it can be
    /// skipped without building. A drv that ever built (locally or, per §7, is
    /// substitutable via a recorded `Cache`/`Built`) is excluded — nix wouldn't
    /// re-attempt it as a dependency, so it blocks nothing.
    pub fn failing_drvs(&self) -> Result<std::collections::HashSet<String>> {
        // The enum codes are interpolated from the mappers so the query can't
        // drift from them (both are integer literals in a STRICT column).
        let sql = format!(
            "SELECT drv_path FROM observation GROUP BY drv_path \
             HAVING SUM(outcome = {built}) = 0 AND SUM(source = {local}) > 0",
            built = outcome_code(Outcome::Built),
            local = source_code(Source::Local),
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            // Stored stripped; callers compare against full `/nix/store` drv paths.
            out.insert(restore_drv(Some(&row?)).expect("Some maps to Some"));
        }
        Ok(out)
    }

    // --- the `--tests` passthru.tests cache (DESIGN.md §4, §6) ---------------

    /// The interned id for `(tree, system)`, or `None` if it has never been
    /// recorded. One indexed point-lookup on the `eval_key` UNIQUE index; a
    /// read-path miss lets the caller skip its query entirely (no rows exist).
    fn key_id(&self, tree: &str, system: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM eval_key WHERE tree = ?1 AND system = ?2",
                params![tree, system],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// [`Store::key_id`], creating the `(tree, system)` row if absent — the
    /// write-path form, resolved once per `cache_test_eval` (not per row).
    fn key_id_get_or_create(tx: &rusqlite::Transaction, tree: &str, system: &str) -> Result<i64> {
        // `ON CONFLICT DO NOTHING` leaves `last_insert_rowid` unset on a hit, so
        // always read the id back rather than trusting the insert.
        tx.execute(
            "INSERT INTO eval_key (tree, system) VALUES (?1, ?2) \
             ON CONFLICT (tree, system) DO NOTHING",
            params![tree, system],
        )?;
        Ok(tx.query_row(
            "SELECT id FROM eval_key WHERE tree = ?1 AND system = ?2",
            params![tree, system],
            |r| r.get(0),
        )?)
    }

    /// Which of `pkgs` have already had their tests evaluated at this key (so a
    /// run need only `eval_tests` the rest). Absence means "never evaluated",
    /// distinct from "evaluated, has no tests" (present here, no `test_drv` rows).
    pub fn tests_cached_pkgs(
        &self,
        tree: &str,
        system: &str,
        pkgs: &[String],
    ) -> Result<std::collections::HashSet<String>> {
        let mut out = std::collections::HashSet::new();
        let Some(key_id) = self.key_id(tree, system)? else {
            return Ok(out); // key never recorded ⇒ nothing cached
        };
        if pkgs.is_empty() {
            return Ok(out);
        }
        let placeholders = placeholders(pkgs.len());
        let sql = format!(
            "SELECT pkg_attr FROM test_pkg \
             WHERE key_id = ?1 AND pkg_attr IN ({placeholders})",
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(
            std::iter::once(key_id.to_string()).chain(pkgs.iter().cloned()),
        );
        let rows = stmt.query_map(params, |r| r.get::<_, String>(0))?;
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    /// Record a completed test eval of `pkgs` (the miss set) and its resulting
    /// `jobs`, in one transaction. Every package in `pkgs` gets a `test_pkg`
    /// marker (even those with no tests — so they're not re-evaluated); each job
    /// with a drv gets a `test_drv` row (its drv stored stripped — see
    /// `evalfile::strip_drv`). Idempotent (`INSERT OR REPLACE`), so a re-run over
    /// the same key is harmless.
    pub fn cache_test_eval(
        &mut self,
        tree: &str,
        system: &str,
        pkgs: &[String],
        jobs: &[TestJob],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let key_id = Self::key_id_get_or_create(&tx, tree, system)?;
        for pkg in pkgs {
            tx.execute(
                "INSERT OR REPLACE INTO test_pkg (key_id, pkg_attr) VALUES (?1, ?2)",
                params![key_id, pkg],
            )?;
        }
        for j in jobs {
            if let Some(drv) = &j.drv_path {
                tx.execute(
                    "INSERT OR REPLACE INTO test_drv \
                     (key_id, pkg_attr, test_attr, drv_path, skipped) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![key_id, j.pkg_attr, j.test_attr, strip_drv(drv), j.skipped],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// All cached test drvs for `pkgs` at this key, as `test_attr → (drv_path,
    /// skipped)` (only tests that resolved to a derivation), with drv paths
    /// restored to their full `/nix/store/…​.drv` form. One query for the whole set.
    pub fn tests_drvs_for(
        &self,
        tree: &str,
        system: &str,
        pkgs: &[String],
    ) -> Result<std::collections::HashMap<String, (String, bool)>> {
        let mut out = std::collections::HashMap::new();
        let Some(key_id) = self.key_id(tree, system)? else {
            return Ok(out);
        };
        if pkgs.is_empty() {
            return Ok(out);
        }
        let placeholders = placeholders(pkgs.len());
        let sql = format!(
            "SELECT test_attr, drv_path, skipped FROM test_drv \
             WHERE key_id = ?1 AND pkg_attr IN ({placeholders})",
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(
            std::iter::once(key_id.to_string()).chain(pkgs.iter().cloned()),
        );
        let rows = stmt.query_map(params, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })?;
        for row in rows {
            let (test_attr, stored, skipped) = row?;
            let drv = restore_drv(Some(&stored)).expect("Some maps to Some");
            out.insert(test_attr, (drv, skipped));
        }
        Ok(out)
    }

    /// Drop the `--tests` cache for one `(tree, system)` — its `eval_key` row and
    /// the `test_pkg`/`test_drv` rows that reference it — when its eval file is
    /// evicted (`--clean`, DESIGN.md §4). Returns the number of `test_drv` rows
    /// removed (the bulk); a no-op if the key was never recorded. The caller
    /// [`Store::vacuum`]s once after a batch of these to return the pages.
    pub fn purge_tests(&mut self, tree: &str, system: &str) -> Result<usize> {
        let Some(key_id) = self.key_id(tree, system)? else {
            return Ok(0);
        };
        let tx = self.conn.transaction()?;
        let drvs = tx.execute("DELETE FROM test_drv WHERE key_id = ?1", [key_id])?;
        tx.execute("DELETE FROM test_pkg WHERE key_id = ?1", [key_id])?;
        tx.execute("DELETE FROM eval_key WHERE id = ?1", [key_id])?;
        tx.commit()?;
        Ok(drvs)
    }

    /// Rebuild the database file to reclaim the pages freed by [`Store::purge_tests`]
    /// (a `DELETE` only moves them to the freelist). Run once after an eviction batch.
    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM").context("vacuuming")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observations_append_and_load() {
        let dir = std::env::temp_dir().join(format!("npd-obs-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store::open(&dir.join("npd.sqlite")).unwrap();

        assert!(s.load_observations("/nix/store/x.drv").unwrap().is_empty());

        let mk = |outcome, when| Observation {
            drv_path: "/nix/store/x.drv".into(),
            source: Source::Local,
            outcome,
            when,
            blocker: Vec::new(),
        };
        s.add_observation(&mk(Outcome::Failed, 100)).unwrap();
        s.add_observation(&mk(Outcome::Built, 200)).unwrap();

        let got = s.load_observations("/nix/store/x.drv").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].outcome, Outcome::Failed); // oldest first
        assert_eq!(got[1].outcome, Outcome::Built);
        assert_eq!(got[1].source, Source::Local);
        // a different drv is independent
        assert!(s.load_observations("/nix/store/y.drv").unwrap().is_empty());

        // A dep-failed's culprit output paths round-trip through the blocker
        // column (newline-joined); a non-dep-failed carries none.
        let mut dep = mk(Outcome::DepFailed, 300);
        dep.drv_path = "/nix/store/z.drv".into();
        dep.blocker = vec!["/nix/store/o1".into(), "/nix/store/o2".into()];
        s.add_observation(&dep).unwrap();
        let got = s.load_observations("/nix/store/z.drv").unwrap();
        assert_eq!(got[0].blocker, vec!["/nix/store/o1", "/nix/store/o2"]);
        assert!(
            s.load_observations("/nix/store/x.drv").unwrap()[0]
                .blocker
                .is_empty()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failing_drvs_are_failures_only() {
        let dir = std::env::temp_dir().join(format!("npd-failing-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store::open(&dir.join("npd.sqlite")).unwrap();
        let obs = |drv: &str, source, outcome, when| Observation {
            drv_path: drv.into(),
            source,
            outcome,
            when,
            blocker: Vec::new(),
        };

        // a: only local failures -> failing.
        s.add_observation(&obs("/nix/store/a.drv", Source::Local, Outcome::Failed, 1))
            .unwrap();
        s.add_observation(&obs(
            "/nix/store/a.drv",
            Source::Local,
            Outcome::DepFailed,
            2,
        ))
        .unwrap();
        // b: failed then built (flaky success) -> NOT failing.
        s.add_observation(&obs("/nix/store/b.drv", Source::Local, Outcome::Failed, 1))
            .unwrap();
        s.add_observation(&obs("/nix/store/b.drv", Source::Local, Outcome::Built, 2))
            .unwrap();
        // c: local failure but substitutable (Cache/Built) -> NOT failing.
        s.add_observation(&obs("/nix/store/c.drv", Source::Local, Outcome::Failed, 1))
            .unwrap();
        s.add_observation(&obs("/nix/store/c.drv", Source::Cache, Outcome::Built, 2))
            .unwrap();
        // d: only a cache hit (never failed locally) -> NOT failing.
        s.add_observation(&obs("/nix/store/d.drv", Source::Cache, Outcome::Built, 1))
            .unwrap();
        // e: dep-failed only -> failing.
        s.add_observation(&obs(
            "/nix/store/e.drv",
            Source::Local,
            Outcome::DepFailed,
            1,
        ))
        .unwrap();

        let failing = s.failing_drvs().unwrap();
        assert_eq!(
            failing,
            [
                "/nix/store/a.drv".to_string(),
                "/nix/store/e.drv".to_string()
            ]
            .into_iter()
            .collect()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cache_round_trip_and_negative() {
        let dir = std::env::temp_dir().join(format!("npd-testcache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store::open(&dir.join("npd.sqlite")).unwrap();
        let (c, sys) = ("treeA", "aarch64-linux");
        let pkgs = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();

        // Nothing cached yet.
        assert!(
            s.tests_cached_pkgs(c, sys, &pkgs(&["hello", "ripgrep"]))
                .unwrap()
                .is_empty()
        );

        // hello has two tests (one skipped); ripgrep has none; one test
        // errored (no drv).
        let jobs = vec![
            TestJob {
                pkg_attr: "hello".into(),
                test_attr: "hello.tests.run".into(),
                drv_path: Some("/nix/store/a.drv".into()),
                skipped: false,
            },
            TestJob {
                pkg_attr: "hello".into(),
                test_attr: "hello.tests.version".into(),
                drv_path: Some("/nix/store/b.drv".into()),
                skipped: true,
            },
            TestJob {
                pkg_attr: "hello".into(),
                test_attr: "hello.tests.err".into(),
                drv_path: None,
                skipped: false,
            },
        ];
        s.cache_test_eval(c, sys, &pkgs(&["hello", "ripgrep"]), &jobs)
            .unwrap();

        // Both packages are now marked evaluated — including the no-test one, so
        // it isn't re-evaluated (negative caching).
        let done = s
            .tests_cached_pkgs(c, sys, &pkgs(&["hello", "ripgrep", "curl"]))
            .unwrap();
        assert!(done.contains("hello") && done.contains("ripgrep") && !done.contains("curl"));

        // hello resolves to its two drv'd tests (the errored one is not stored),
        // each carrying its own meta-blocked bit.
        let hd = s.tests_drvs_for(c, sys, &pkgs(&["hello"])).unwrap();
        assert_eq!(hd.len(), 2);
        assert_eq!(
            hd.get("hello.tests.run"),
            Some(&("/nix/store/a.drv".to_string(), false))
        );
        assert_eq!(
            hd.get("hello.tests.version"),
            Some(&("/nix/store/b.drv".to_string(), true))
        );
        // ripgrep is cached-done but has no test drvs.
        assert!(
            s.tests_drvs_for(c, sys, &pkgs(&["ripgrep"]))
                .unwrap()
                .is_empty()
        );
        // a different tree shares nothing.
        assert!(
            s.tests_cached_pkgs("treeB", sys, &pkgs(&["hello"]))
                .unwrap()
                .is_empty()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn purge_tests_drops_one_key_only() {
        let dir = std::env::temp_dir().join(format!("npd-purge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store::open(&dir.join("npd.sqlite")).unwrap();
        let sys = "aarch64-linux";
        let pkgs = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let job = |t: &str, drv: &str| TestJob {
            pkg_attr: "hello".into(),
            test_attr: t.into(),
            drv_path: Some(drv.into()),
            skipped: false,
        };

        // Two trees on the same system, each with one drv'd test.
        s.cache_test_eval(
            "treeA",
            sys,
            &pkgs(&["hello"]),
            &[job("hello.tests.a", "/nix/store/a.drv")],
        )
        .unwrap();
        s.cache_test_eval(
            "treeB",
            sys,
            &pkgs(&["hello"]),
            &[job("hello.tests.b", "/nix/store/b.drv")],
        )
        .unwrap();

        // Evicting treeA removes exactly its rows (1 test_drv) and leaves treeB.
        assert_eq!(s.purge_tests("treeA", sys).unwrap(), 1);
        assert!(
            s.tests_cached_pkgs("treeA", sys, &pkgs(&["hello"]))
                .unwrap()
                .is_empty()
        );
        assert!(
            s.tests_drvs_for("treeA", sys, &pkgs(&["hello"]))
                .unwrap()
                .is_empty()
        );
        assert!(
            s.tests_cached_pkgs("treeB", sys, &pkgs(&["hello"]))
                .unwrap()
                .contains("hello")
        );
        assert_eq!(
            s.tests_drvs_for("treeB", sys, &pkgs(&["hello"]))
                .unwrap()
                .len(),
            1
        );

        // Purging an unknown key is a no-op, and VACUUM after a batch is fine.
        assert_eq!(s.purge_tests("treeA", sys).unwrap(), 0);
        s.vacuum().unwrap();

        let _ = fs::remove_dir_all(&dir);
    }
}
