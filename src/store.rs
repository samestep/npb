//! The SQLite fact store: pure eval facts and (later) the append-only
//! observation log, in one `~/.cache/nix-npd/npd.sqlite` (DESIGN.md §3–§4).
//!
//! `existence` is not persisted — it is recomputed from `drv_path` + the meta
//! flags on load, so there is one source of truth for that mapping.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::{AttrEval, Existence, Observation, Outcome, Source};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS eval_run (
    id           INTEGER PRIMARY KEY,
    commit_      TEXT    NOT NULL,
    system       TEXT    NOT NULL,
    profile      TEXT    NOT NULL,
    eval_version INTEGER NOT NULL,
    evaluated_at INTEGER NOT NULL,
    UNIQUE (commit_, system, profile, eval_version)
) STRICT;

-- attr rows reference the run by its small integer id rather than repeating the
-- (40-char commit, system, profile, version) key in every row and its index.
CREATE TABLE IF NOT EXISTS attr_eval (
    run_id       INTEGER NOT NULL REFERENCES eval_run (id) ON DELETE CASCADE,
    attr         TEXT    NOT NULL,
    drv_path     TEXT,
    broken       INTEGER,
    unsupported  INTEGER,
    insecure     INTEGER,
    error        TEXT,
    PRIMARY KEY (run_id, attr)
) STRICT;

-- The append-only observation log (DESIGN.md §3): the build driver appends a
-- `local`/`cache` fact here per drv. (A legacy `build_id` column may linger on
-- old databases from the dropped Hydra source; it is simply never written now.)
CREATE TABLE IF NOT EXISTS observation (
    id         INTEGER PRIMARY KEY,
    drv_path   TEXT    NOT NULL,
    source     TEXT    NOT NULL,
    outcome    TEXT    NOT NULL,
    when_      INTEGER NOT NULL,
    system     TEXT,
    duration_s REAL,
    cached     INTEGER,
    machine    TEXT,
    log_ref    TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS observation_drv ON observation (drv_path);

-- npd once recorded Hydra facts; that source is gone. Purge its rows so the
-- verdict logic (which no longer knows `hydra-job`) never trips over them.
DELETE FROM observation WHERE source = 'hydra-job';
";

fn source_str(s: Source) -> &'static str {
    match s {
        Source::Local => "local",
        Source::Cache => "cache",
    }
}

fn source_from(s: &str) -> Result<Source> {
    Ok(match s {
        "local" => Source::Local,
        "cache" => Source::Cache,
        other => anyhow::bail!("unknown observation source in store: {other:?}"),
    })
}

fn outcome_str(o: Outcome) -> &'static str {
    match o {
        Outcome::Built => "built",
        Outcome::Failed => "failed",
        Outcome::DepFailed => "dep-failed",
        Outcome::NotAttempted => "not-attempted",
    }
}

fn outcome_from(s: &str) -> Result<Outcome> {
    Ok(match s {
        "built" => Outcome::Built,
        "failed" => Outcome::Failed,
        "dep-failed" => Outcome::DepFailed,
        "not-attempted" => Outcome::NotAttempted,
        other => anyhow::bail!("unknown observation outcome in store: {other:?}"),
    })
}

fn recompute_existence(a: &AttrEval) -> Existence {
    if a.drv_path.is_some() {
        if a.broken == Some(true) || a.unsupported == Some(true) || a.insecure == Some(true) {
            Existence::Blocked
        } else {
            Existence::Buildable
        }
    } else {
        Existence::Error
    }
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).context("creating cache directory")?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        // WAL: readers don't block the writer; better for a durable local store.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "foreign_keys", "ON").ok();
        conn.execute_batch(SCHEMA).context("initializing schema")?;
        Ok(Self { conn })
    }

    fn run_id(
        conn: &Connection,
        commit: &str,
        system: &str,
        profile: &str,
        version: u32,
    ) -> Result<Option<i64>> {
        Ok(conn
            .query_row(
                "SELECT id FROM eval_run \
                 WHERE commit_ = ?1 AND system = ?2 AND profile = ?3 AND eval_version = ?4",
                params![commit, system, profile, version],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The cached full-set eval for this key, or `None` if never evaluated.
    pub fn load_eval(
        &self,
        commit: &str,
        system: &str,
        profile: &str,
        version: u32,
    ) -> Result<Option<Vec<AttrEval>>> {
        let Some(run_id) = Self::run_id(&self.conn, commit, system, profile, version)? else {
            return Ok(None);
        };

        let mut stmt = self.conn.prepare(
            "SELECT attr, drv_path, broken, unsupported, insecure, error FROM attr_eval \
             WHERE run_id = ?1 ORDER BY attr",
        )?;
        let rows = stmt.query_map(params![run_id], |r| {
            let mut a = AttrEval {
                attr: r.get(0)?,
                existence: Existence::Error, // fixed up below
                drv_path: r.get(1)?,
                broken: r.get(2)?,
                unsupported: r.get(3)?,
                insecure: r.get(4)?,
                hydra_platforms_ok: None,
                error: r.get(5)?,
            };
            a.existence = recompute_existence(&a);
            Ok(a)
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(Some(out))
    }

    /// Store (replacing any prior) the full-set eval for this key.
    pub fn store_eval(
        &mut self,
        commit: &str,
        system: &str,
        profile: &str,
        version: u32,
        evaluated_at: i64,
        attrs: &[AttrEval],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        // Upsert the run, then (re)populate its attrs. Deleting the run cascades
        // to its old attr_eval rows.
        if let Some(old) = Self::run_id(&tx, commit, system, profile, version)? {
            tx.execute("DELETE FROM eval_run WHERE id = ?1", params![old])?;
        }
        tx.execute(
            "INSERT INTO eval_run (commit_, system, profile, eval_version, evaluated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![commit, system, profile, version, evaluated_at],
        )?;
        let run_id = tx.last_insert_rowid();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO attr_eval \
                 (run_id, attr, drv_path, broken, unsupported, insecure, error) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for a in attrs {
                stmt.execute(params![
                    run_id,
                    a.attr,
                    a.drv_path,
                    a.broken,
                    a.unsupported,
                    a.insecure,
                    a.error,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Append one observation to the log (never overwrites; DESIGN.md §3).
    pub fn add_observation(&mut self, o: &Observation) -> Result<()> {
        self.conn.execute(
            "INSERT INTO observation \
             (drv_path, source, outcome, when_, system, duration_s, cached, machine, log_ref) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                o.drv_path,
                source_str(o.source),
                outcome_str(o.outcome),
                o.when,
                o.system,
                o.duration_s,
                o.cached,
                o.machine,
                o.log_ref,
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
        let placeholders = std::iter::repeat_n("?", drv_paths.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT drv_path, source, outcome, when_, system, duration_s, cached, machine, log_ref \
             FROM observation WHERE drv_path IN ({placeholders}) ORDER BY when_, id",
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(drv_paths.iter());
        let rows = stmt.query_map(params, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<f64>>(5)?,
                r.get::<_, Option<bool>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        })?;
        for row in rows {
            let (drv_path, source, outcome, when, system, duration_s, cached, machine, log_ref) =
                row?;
            out.entry(drv_path.clone()).or_default().push(Observation {
                drv_path,
                source: source_from(&source)?,
                outcome: outcome_from(&outcome)?,
                when,
                system,
                duration_s,
                cached,
                machine,
                log_ref,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(attr: &str, drv: Option<&str>, broken: Option<bool>) -> AttrEval {
        AttrEval {
            attr: attr.into(),
            existence: Existence::Buildable,
            drv_path: drv.map(str::to_string),
            broken,
            unsupported: None,
            insecure: None,
            hydra_platforms_ok: None,
            error: if drv.is_none() { Some("boom".into()) } else { None },
        }
    }

    #[test]
    fn eval_round_trips_and_recomputes_existence() {
        let dir = std::env::temp_dir().join(format!("npd-store-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store::open(&dir.join("npd.sqlite")).unwrap();

        // absent before storing
        assert!(s.load_eval("c1", "aarch64-linux", "default", 1).unwrap().is_none());

        let attrs = vec![
            ev("hello", Some("/nix/store/a-hello.drv"), Some(false)),
            ev("br", Some("/nix/store/b-br.drv"), Some(true)),
            ev("bad", None, None),
        ];
        s.store_eval("c1", "aarch64-linux", "default", 1, 123, &attrs).unwrap();

        let got = s.load_eval("c1", "aarch64-linux", "default", 1).unwrap().unwrap();
        assert_eq!(got.len(), 3);
        // existence recomputed from drv + flags
        let by = |n: &str| got.iter().find(|a| a.attr == n).unwrap().existence;
        assert_eq!(by("hello"), Existence::Buildable);
        assert_eq!(by("br"), Existence::Blocked);
        assert_eq!(by("bad"), Existence::Error);

        // a different key is still absent; wrong version misses
        assert!(s.load_eval("c2", "aarch64-linux", "default", 1).unwrap().is_none());
        assert!(s.load_eval("c1", "aarch64-linux", "default", 2).unwrap().is_none());

        let _ = fs::remove_dir_all(&dir);
    }

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
            system: Some("aarch64-linux".into()),
            duration_s: Some(1.5),
            cached: None,
            machine: Some("host".into()),
            log_ref: Some("logs/x/build.log".into()),
        };
        s.add_observation(&mk(Outcome::Failed, 100)).unwrap();
        s.add_observation(&mk(Outcome::Built, 200)).unwrap();

        let got = s.load_observations("/nix/store/x.drv").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].outcome, Outcome::Failed); // oldest first
        assert_eq!(got[1].outcome, Outcome::Built);
        assert_eq!(got[1].source, Source::Local);
        assert_eq!(got[1].duration_s, Some(1.5));
        // a different drv is independent
        assert!(s.load_observations("/nix/store/y.drv").unwrap().is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}
