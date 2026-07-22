//! The SQLite fact store, in `~/.cache/nix-npb/npb.sqlite` (DESIGN.md §3–§4): the
//! append-only observation log and the `--tests` eval cache. Full-set evals do
//! *not* live here — they're standalone files (see `eval.rs`) — so what remains
//! is only the small, index-worthy data an engine actually earns.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::evalfile::{restore_drv, strip_drv};
use crate::model::{Observation, Outcome, TestJob};

// npb is public and writes no migration code (DESIGN.md §1, CLAUDE.md): this
// schema evolves only *additively* — a new `CREATE TABLE IF NOT EXISTS`, or a
// column an old binary tolerates — never a rename, drop, or `ALTER` that would
// need a migration, and never a compat shim. The store is re-derivable but never
// deleted or invalidated to dodge a format change. The DDL lives in its own
// `.sql` file (embedded at compile time) purely for editor syntax highlighting;
// the `outcome` enum-code mapping below is its Rust half.
const SCHEMA: &str = include_str!("schema.sql");

// `outcome` persists as a small integer enum code, not its English label. The
// values are fixed on-disk — an older npb must read what a newer one wrote — so
// existing codes never change (the additive rule above); a new outcome may only
// take a new, unused code. Keeping them fixed also keeps the `failing_drvs`
// query below and any hand-inspection legible.
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
        // handler fails immediately — a second npb (or the report path's Cache
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
        // newline join round-trips losslessly; an empty blocker is NULL (a
        // success carries none).
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
            "INSERT INTO observation (drv_path, outcome, blocker) \
             VALUES (?1, ?2, ?3)",
            params![strip_drv(&o.drv_path), outcome_code(o.outcome), blocker],
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
            "SELECT drv_path, outcome, blocker \
             FROM observation WHERE drv_path IN ({placeholders}) ORDER BY id",
        );
        let mut stmt = self.conn.prepare(&sql)?;
        // The column is stored stripped, so match against the stripped query keys.
        let params = rusqlite::params_from_iter(drv_paths.iter().map(|d| strip_drv(d)));
        let rows = stmt.query_map(params, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (stored_drv, outcome, blocker) = row?;
            let drv_path = restore_drv(Some(&stored_drv)).expect("Some maps to Some");
            out.entry(drv_path.clone()).or_default().push(Observation {
                drv_path,
                outcome: outcome_from_code(outcome)?,
                blocker: blocker
                    .filter(|s| !s.is_empty())
                    .map(|s| s.split('\n').map(restore_out).collect())
                    .unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// Every drv whose history is failures-only — at least one observation, none
    /// of them a success. This is exactly the failures-only condition
    /// [`crate::model::BuildPolicy::decide`] applies per drv, lifted to a set so
    /// the build driver can propagate a known failure *forward* through the
    /// dependency graph (DESIGN.md §5): any target whose build closure contains
    /// such a drv would only `DepFail`, so it can be skipped without building. A
    /// drv with any recorded `Built` — its own build, or cache presence (§7) —
    /// is excluded: nix wouldn't re-attempt it as a dependency, so it blocks
    /// nothing.
    pub fn failing_drvs(&self) -> Result<std::collections::HashSet<String>> {
        // The enum code is interpolated from the mapper so the query can't
        // drift from it (an integer literal in a STRICT column).
        let sql = format!(
            "SELECT drv_path FROM observation GROUP BY drv_path \
             HAVING SUM(outcome = {built}) = 0",
            built = outcome_code(Outcome::Built),
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

    /// Cached transitive-meta-block verdicts (DESIGN.md §6) for `attrs` at this
    /// key, as `attr → blocked`. An attr absent from the result was never checked
    /// (or the key isn't recorded) and must be recomputed — the same
    /// absence-means-unknown contract as [`Store::tests_cached_pkgs`]. Keyed on
    /// attr, not a drv, because the verdict is genuinely per-attr: the strict-eval
    /// throw depends on which attr's `meta` is forced (§6). One query.
    pub fn blocked_verdicts(
        &self,
        tree: &str,
        system: &str,
        attrs: &[String],
    ) -> Result<std::collections::HashMap<String, bool>> {
        let mut out = std::collections::HashMap::new();
        let Some(key_id) = self.key_id(tree, system)? else {
            return Ok(out);
        };
        if attrs.is_empty() {
            return Ok(out);
        }
        let placeholders = placeholders(attrs.len());
        let sql = format!(
            "SELECT attr, blocked FROM transitive_block \
             WHERE key_id = ?1 AND attr IN ({placeholders})",
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(
            std::iter::once(key_id.to_string()).chain(attrs.iter().cloned()),
        );
        let rows = stmt.query_map(params, |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))
        })?;
        for row in rows {
            let (attr, blocked) = row?;
            out.insert(attr, blocked);
        }
        Ok(out)
    }

    /// Record transitive-meta-block verdicts (`(attr, blocked)`) at this key, in
    /// one transaction. Idempotent (`INSERT OR REPLACE`). Only *checked* attrs
    /// (evaluated this run) belong here — an unchecked one is left uncached so a
    /// later run rechecks it.
    pub fn cache_blocked(
        &mut self,
        tree: &str,
        system: &str,
        verdicts: &[(String, bool)],
    ) -> Result<()> {
        if verdicts.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        let key_id = Self::key_id_get_or_create(&tx, tree, system)?;
        for (attr, blocked) in verdicts {
            tx.execute(
                "INSERT OR REPLACE INTO transitive_block (key_id, attr, blocked) \
                 VALUES (?1, ?2, ?3)",
                params![key_id, attr, blocked],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Drop the caches for one `(tree, system)` — its `eval_key` row and the
    /// `test_pkg`/`test_drv`/`transitive_block` rows that reference it — when its
    /// eval file is evicted (`--clean`, DESIGN.md §4). Returns the number of
    /// `test_drv` rows removed (the bulk); a no-op if the key was never recorded.
    /// The caller [`Store::vacuum`]s once after a batch of these to return the pages.
    pub fn purge_tests(&mut self, tree: &str, system: &str) -> Result<usize> {
        let Some(key_id) = self.key_id(tree, system)? else {
            return Ok(0);
        };
        let tx = self.conn.transaction()?;
        let drvs = tx.execute("DELETE FROM test_drv WHERE key_id = ?1", [key_id])?;
        tx.execute("DELETE FROM test_pkg WHERE key_id = ?1", [key_id])?;
        tx.execute("DELETE FROM transitive_block WHERE key_id = ?1", [key_id])?;
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

    // --- the patch-tree cache (DESIGN.md §8) --------------------------------

    /// The head tree a `--patch <A...B>` compare reconstructs onto `anchor`, if
    /// npb has recorded it. `None` on a miss (the caller then downloads).
    pub fn patch_tree(&self, anchor: &str, expr: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT tree FROM patch_tree WHERE anchor = ?1 AND expr = ?2",
                params![anchor, expr],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Record that applying compare `expr`'s diff onto `anchor` yields head `tree`.
    /// Idempotent (`INSERT OR REPLACE`) — the mapping is a pure function of its key.
    pub fn put_patch_tree(&mut self, anchor: &str, expr: &str, tree: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO patch_tree (anchor, expr, tree) VALUES (?1, ?2, ?3)",
            params![anchor, expr, tree],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observations_append_and_load() {
        let dir = std::env::temp_dir().join(format!("npb-obs-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store::open(&dir.join("npb.sqlite")).unwrap();

        assert!(s.load_observations("/nix/store/x.drv").unwrap().is_empty());

        let mk = |outcome| Observation {
            drv_path: "/nix/store/x.drv".into(),
            outcome,
            blocker: Vec::new(),
        };
        s.add_observation(&mk(Outcome::Failed)).unwrap();
        s.add_observation(&mk(Outcome::Built)).unwrap();

        let got = s.load_observations("/nix/store/x.drv").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].outcome, Outcome::Failed); // oldest first
        assert_eq!(got[1].outcome, Outcome::Built);
        // a different drv is independent
        assert!(s.load_observations("/nix/store/y.drv").unwrap().is_empty());

        // A failure's blocker output paths round-trip through the blocker
        // column (newline-joined); a success carries none.
        let mut dep = mk(Outcome::DepFailed);
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
        let dir = std::env::temp_dir().join(format!("npb-failing-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store::open(&dir.join("npb.sqlite")).unwrap();
        let obs = |drv: &str, outcome| Observation {
            drv_path: drv.into(),
            outcome,
            blocker: Vec::new(),
        };

        // a: only failures -> failing.
        s.add_observation(&obs("/nix/store/a.drv", Outcome::Failed))
            .unwrap();
        s.add_observation(&obs("/nix/store/a.drv", Outcome::DepFailed))
            .unwrap();
        // b: failed then built — a flaky local success, or a later cache hit
        // (recorded as the same Built, §7) -> NOT failing.
        s.add_observation(&obs("/nix/store/b.drv", Outcome::Failed))
            .unwrap();
        s.add_observation(&obs("/nix/store/b.drv", Outcome::Built))
            .unwrap();
        // d: only a success (e.g. a recorded cache hit) -> NOT failing.
        s.add_observation(&obs("/nix/store/d.drv", Outcome::Built))
            .unwrap();
        // e: dep-failed only -> failing.
        s.add_observation(&obs("/nix/store/e.drv", Outcome::DepFailed))
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
        let dir = std::env::temp_dir().join(format!("npb-testcache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store::open(&dir.join("npb.sqlite")).unwrap();
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
        let dir = std::env::temp_dir().join(format!("npb-purge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store::open(&dir.join("npb.sqlite")).unwrap();
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

    #[test]
    fn blocked_verdict_cache_round_trip_and_purge() {
        let dir = std::env::temp_dir().join(format!("npb-tb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store::open(&dir.join("npb.sqlite")).unwrap();
        let sys = "aarch64-darwin";
        let attrs = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let (a, b, c) = (
            "matrix-synapse-plugins.matrix-synapse-ldap3".to_string(),
            "hello".to_string(),
            "ripgrep".to_string(),
        );

        // Nothing cached: every candidate is a miss (absent from the map).
        assert!(
            s.blocked_verdicts("treeA", sys, &attrs(&[&a]))
                .unwrap()
                .is_empty()
        );

        // Record a blocked and a clean verdict; both are definitive (`false` is a
        // real "checked, clean" answer, not a miss).
        s.cache_blocked("treeA", sys, &[(a.clone(), true), (b.clone(), false)])
            .unwrap();
        let v = s
            .blocked_verdicts("treeA", sys, &attrs(&[&a, &b, &c]))
            .unwrap();
        assert_eq!(v.get(&a), Some(&true));
        assert_eq!(v.get(&b), Some(&false));
        assert_eq!(v.get(&c), None); // never recorded ⇒ still a miss

        // A different tree shares nothing.
        assert!(
            s.blocked_verdicts("treeB", sys, &attrs(&[&a]))
                .unwrap()
                .is_empty()
        );

        // Eviction (via purge_tests) drops this key's verdicts in lockstep.
        s.cache_blocked("treeB", sys, &[(a.clone(), true)]).unwrap();
        s.purge_tests("treeA", sys).unwrap();
        assert!(
            s.blocked_verdicts("treeA", sys, &attrs(&[&a]))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            s.blocked_verdicts("treeB", sys, &attrs(&[&a]))
                .unwrap()
                .get(&a),
            Some(&true)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn patch_tree_round_trip() {
        let dir = std::env::temp_dir().join(format!("npb-patchtree-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store::open(&dir.join("npb.sqlite")).unwrap();

        // Miss before anything is recorded.
        assert_eq!(s.patch_tree("anchorsha", "a...b").unwrap(), None);
        // Record and read back; a different (anchor, expr) is independent.
        s.put_patch_tree("anchorsha", "a...b", "treesha").unwrap();
        assert_eq!(
            s.patch_tree("anchorsha", "a...b").unwrap(),
            Some("treesha".to_string())
        );
        assert_eq!(s.patch_tree("anchorsha", "a...c").unwrap(), None);
        assert_eq!(s.patch_tree("other", "a...b").unwrap(), None);
        // The mapping is a pure function of its key: overwrite is idempotent.
        s.put_patch_tree("anchorsha", "a...b", "treesha").unwrap();
        assert_eq!(
            s.patch_tree("anchorsha", "a...b").unwrap(),
            Some("treesha".to_string())
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
