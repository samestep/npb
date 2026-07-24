//! The eval-cache format version and its one-time wipe (DESIGN.md §1, §4).
//!
//! Everything under `~/.cache/nix-npb` is re-derivable, and npb writes no
//! migration code (CLAUDE.md). The observation log (`observation`) and the
//! patch-tree cache (`patch_tree`) are format-stable and drvpath-keyed — valid
//! across every profile and format — so they survive untouched. But the eval
//! *files* and the eval-derived DB caches (`eval_key` / `test_pkg` / `test_drv`)
//! change shape from time to time. Rather than coexist or migrate, a bump to
//! [`CURRENT`] wipes exactly those on first run — after a Y/n prompt — while
//! leaving the observation log intact. A `format-version` file at the cache root
//! records the version; its absence on a cache that already holds data means
//! version 0 (an npb from before this file existed).
//!
//! This is a deliberate, dated exception to the otherwise-additive store policy
//! (DESIGN.md §1): the durable, expensive-to-re-derive facts survive, and only
//! the cheap re-derivable evaluations are discarded.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use crate::paths::{cache_root, db_path};
use crate::prompt::confirm;

/// The current eval-cache format version. Bump when the eval-file layout or an
/// eval-derived table changes incompatibly; the bump wipes the old eval cache on
/// first run (see [`ensure_current`]). Version 1 introduced profile-qualified
/// eval keys (`<token>/<system>`) and dropped the eval-file meta bit. Version 2
/// added `allowAliases = false` to the eval config, dropping redundant alias
/// attrs (e.g. `claude-code-bin`) from every eval.
const CURRENT: u32 = 2;

/// The version-marker file at the cache root.
fn version_path() -> Result<PathBuf> {
    Ok(cache_root()?.join("format-version"))
}

/// Reconcile the on-disk cache with [`CURRENT`], before anything reads or writes
/// it. Four cases:
/// - marker present and equal to [`CURRENT`] → nothing to do;
/// - marker present but newer → refuse (this npb can't read it);
/// - no marker and no `npb.sqlite` → a fresh cache: record the version, proceed;
/// - otherwise (an older cache) → prompt to wipe the eval cache and the
///   eval-derived DB tables, keeping the observation and patch-tree data; on
///   yes, wipe and record the version; on no, abort.
pub fn ensure_current() -> Result<()> {
    let vpath = version_path()?;
    let found: Option<u32> = match fs::read_to_string(&vpath) {
        Ok(s) => Some(
            s.trim()
                .parse()
                .with_context(|| format!("parsing {}", vpath.display()))?,
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", vpath.display())),
    };
    match found {
        Some(v) if v == CURRENT => return Ok(()),
        Some(v) if v > CURRENT => bail!(
            "the npb cache at {} was written by a newer npb (format version {v}; this npb \
             is version {CURRENT}). Upgrade npb, or remove that directory to start fresh.",
            cache_root()?.display(),
        ),
        None if !db_path()?.exists() => {
            // A fresh cache (no data yet): just stamp the current version.
            write_version()?;
            return Ok(());
        }
        _ => {} // an older cache (v0, or a lower recorded version): wipe below.
    }

    let root = cache_root()?;
    let from = found.map(|v| v.to_string()).unwrap_or_else(|| "0".into());
    eprintln!(
        "npb's eval-cache format has changed (version {from} -> {CURRENT}). The cached\n\
         evaluations and --tests cache must be regenerated; your build history (the\n\
         observation log) is kept."
    );
    if !confirm(
        &format!("Delete the eval cache under {}? [Y/n] ", root.display()),
        true,
    )? {
        bail!("aborted: the cache format is out of date and was left untouched");
    }
    wipe_eval_cache()?;
    write_version()?;
    Ok(())
}

/// Write [`CURRENT`] to the marker file, creating the cache root if needed.
fn write_version() -> Result<()> {
    fs::create_dir_all(cache_root()?).context("creating cache directory")?;
    fs::write(version_path()?, format!("{CURRENT}\n"))
        .context("writing the cache format version")?;
    Ok(())
}

/// Remove every eval file (all profile/system subtrees under the cache root) and
/// drop the eval-derived DB tables, leaving `npb.sqlite`'s observation and
/// patch-tree data intact. The dropped tables are recreated empty by the next
/// `Store::open` (its `CREATE TABLE IF NOT EXISTS` schema).
fn wipe_eval_cache() -> Result<()> {
    let root = cache_root()?;
    // Every entry under the root except the DB (a file, plus its WAL/SHM
    // sidecars) and the version marker (a file) is an eval-file subtree — so
    // removing the subdirectories removes exactly the eval files, of either the
    // old or the new layout.
    if root.exists() {
        for ent in fs::read_dir(&root).with_context(|| format!("reading {}", root.display()))? {
            let ent = ent?;
            if ent.file_type()?.is_dir() {
                fs::remove_dir_all(ent.path())
                    .with_context(|| format!("removing {}", ent.path().display()))?;
            }
        }
    }
    // Drop the eval-derived tables if the DB exists; observation/patch_tree stay.
    // Every child that references `eval_key` (`test_pkg`, `test_drv`, and an old
    // db's `transitive_block`) must be dropped *before* `eval_key` itself:
    // foreign keys are enforced in this SQLite build, so dropping the parent while
    // a referencing row survives fails the constraint. `IF EXISTS` keeps it
    // idempotent, so a re-run after a partially-applied wipe still completes.
    let db = db_path()?;
    if db.exists() {
        let conn = Connection::open(&db).with_context(|| format!("opening {}", db.display()))?;
        conn.execute_batch(
            "DROP TABLE IF EXISTS test_drv; \
             DROP TABLE IF EXISTS test_pkg; \
             DROP TABLE IF EXISTS transitive_block; \
             DROP TABLE IF EXISTS eval_key; \
             VACUUM;",
        )
        .context("dropping the eval-derived cache tables")?;
    }
    Ok(())
}
