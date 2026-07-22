//! Where npb's on-disk state lives. Everything npb stores is a re-derivable
//! cache (DESIGN.md §4), so it all sits under the platform cache directory.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// The root of everything npb stores: `~/.cache/nix-npb` (or the platform
/// equivalent).
pub fn cache_root() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("could not determine cache directory")?
        .join("nix-npb"))
}

/// The SQLite fact store: the observation log and the `--tests` cache.
pub fn db_path() -> Result<PathBuf> {
    Ok(cache_root()?.join("npb.sqlite"))
}
