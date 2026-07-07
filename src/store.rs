//! The SQLite fact store: pure eval facts and (later) the append-only
//! observation log, in one `~/.cache/nix-npd/npd.sqlite` (DESIGN.md §3–§4).
//!
//! `existence` is not persisted — it is recomputed from `drv_path` + the meta
//! flags on load, so there is one source of truth for that mapping.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::{AttrEval, Existence};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS eval_run (
    commit_      TEXT    NOT NULL,
    system       TEXT    NOT NULL,
    profile      TEXT    NOT NULL,
    eval_version INTEGER NOT NULL,
    evaluated_at INTEGER NOT NULL,
    PRIMARY KEY (commit_, system, profile, eval_version)
) STRICT;

CREATE TABLE IF NOT EXISTS attr_eval (
    commit_      TEXT    NOT NULL,
    system       TEXT    NOT NULL,
    profile      TEXT    NOT NULL,
    eval_version INTEGER NOT NULL,
    attr         TEXT    NOT NULL,
    drv_path     TEXT,
    broken       INTEGER,
    unsupported  INTEGER,
    insecure     INTEGER,
    error        TEXT,
    PRIMARY KEY (commit_, system, profile, eval_version, attr)
) STRICT;

-- The append-only observation log (DESIGN.md §3). Not yet written to; the build
-- driver and hydra fetch will append here.
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
    log_ref    TEXT,
    build_id   INTEGER
) STRICT;
CREATE INDEX IF NOT EXISTS observation_drv ON observation (drv_path);
";

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
        conn.execute_batch(SCHEMA).context("initializing schema")?;
        Ok(Self { conn })
    }

    /// The cached full-set eval for this key, or `None` if never evaluated.
    pub fn load_eval(
        &self,
        commit: &str,
        system: &str,
        profile: &str,
        version: u32,
    ) -> Result<Option<Vec<AttrEval>>> {
        let evaluated: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM eval_run \
                 WHERE commit_ = ?1 AND system = ?2 AND profile = ?3 AND eval_version = ?4",
                params![commit, system, profile, version],
                |r| r.get(0),
            )
            .optional()?;
        if evaluated.is_none() {
            return Ok(None);
        }

        let mut stmt = self.conn.prepare(
            "SELECT attr, drv_path, broken, unsupported, insecure, error FROM attr_eval \
             WHERE commit_ = ?1 AND system = ?2 AND profile = ?3 AND eval_version = ?4 \
             ORDER BY attr",
        )?;
        let rows = stmt.query_map(params![commit, system, profile, version], |r| {
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
        tx.execute(
            "DELETE FROM attr_eval \
             WHERE commit_ = ?1 AND system = ?2 AND profile = ?3 AND eval_version = ?4",
            params![commit, system, profile, version],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO attr_eval \
                 (commit_, system, profile, eval_version, attr, drv_path, broken, unsupported, insecure, error) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for a in attrs {
                stmt.execute(params![
                    commit,
                    system,
                    profile,
                    version,
                    a.attr,
                    a.drv_path,
                    a.broken,
                    a.unsupported,
                    a.insecure,
                    a.error,
                ])?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO eval_run \
             (commit_, system, profile, eval_version, evaluated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![commit, system, profile, version, evaluated_at],
        )?;
        tx.commit()?;
        Ok(())
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
}
