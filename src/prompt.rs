//! A small yes/no confirmation prompt on stderr, shared by the destructive
//! maintenance paths: `--clean` eviction (`crate::clean`) and the one-time
//! cache-format-version wipe (`crate::cacheversion`).

use std::io::Write;

use anyhow::Result;

/// Prompt on stderr and read a yes/no answer from stdin. `default` is the answer
/// for an empty line (a bare Enter) or an unrecognized one, so the caller picks
/// the prompt's polarity: `false` for a `[y/N]` prompt (an explicit `y`/`yes`
/// proceeds — the safe default for a destructive action), `true` for a `[Y/n]`
/// prompt (an explicit `n`/`no` aborts). A closed stdin (EOF, e.g. run in a pipe
/// with no input) also takes the default.
pub fn confirm(prompt: &str, default: bool) -> Result<bool> {
    // Prompt on stderr so a redirected stdout keeps only the machine-ish summary.
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        eprintln!(); // move past the prompt line on EOF
        return Ok(default);
    }
    Ok(answer(&line, default))
}

/// Resolve a prompt answer: an explicit `y`/`yes` or `n`/`no` (case- and
/// space-insensitive) wins; anything else (empty, or unrecognized) takes the
/// prompt's `default`.
fn answer(line: &str, default: bool) -> bool {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_yes_or_no_wins_over_the_default() {
        // An explicit yes/no is honored regardless of the prompt's default.
        for yes in ["y", "Y", "yes", "YES", " yes \n", "y\n"] {
            assert!(answer(yes, false), "{yes:?} should confirm");
            assert!(answer(yes, true), "{yes:?} should confirm");
        }
        for no in ["n", "N", "no", "NO", " no \n", "n\n"] {
            assert!(!answer(no, false), "{no:?} should decline");
            assert!(!answer(no, true), "{no:?} should decline");
        }
        // Empty or unrecognized falls to the default: `[y/N]` → no, `[Y/n]` → yes.
        for other in ["", "\n", "nope", "yep", "sure", "1"] {
            assert!(
                !answer(other, false),
                "{other:?} should take the no default"
            );
            assert!(answer(other, true), "{other:?} should take the yes default");
        }
    }
}
