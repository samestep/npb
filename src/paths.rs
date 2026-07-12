//! Where npd's on-disk state lives. Everything npd stores is a re-derivable
//! cache (DESIGN.md §4), so it all sits under the platform cache directory.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// The root of everything npd stores: `~/.cache/nix-npd` (or the platform
/// equivalent).
pub fn cache_root() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("could not determine cache directory")?
        .join("nix-npd"))
}

/// The SQLite fact store: the observation log and the `--tests` cache.
pub fn db_path() -> Result<PathBuf> {
    Ok(cache_root()?.join("npd.sqlite"))
}
