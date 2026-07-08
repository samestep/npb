//! Render a Markdown report of a change (`base → head`).
//!
//! Each attr in the changed set has a *state* on each side — reduced from the
//! observation log (§8) — and the report groups attrs by the `(base, head)`
//! state pair. The section header *is* a composable `before → after` token
//! (one emoji per side); no per-row glyphs. Attrs that share a derivation are
//! collapsed onto one line (`a = b = c`), like `nixpkgs-review`'s aliases.

use std::collections::{BTreeMap, HashMap};

use crate::model::{Observation, Outcome, Source};

/// One side's build state, reduced from a drv's observations (or its absence).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum State {
    /// Output valid — built locally, or substitutable from the cache.
    Built,
    /// Its own build failed (a direct failure).
    Failed,
    /// A dependency failed, so this never ran (a transitive/cascade failure).
    Blocked,
    /// No derivation on this side (the attr doesn't exist there).
    Absent,
    /// Has a derivation but no build fact yet (only reachable with `--no-build`).
    Unknown,
}

impl State {
    fn glyph(self) -> &'static str {
        match self {
            State::Built => "✅",
            State::Failed => "❌",
            State::Blocked => "🚫",
            State::Absent => "➖",
            State::Unknown => "❓",
        }
    }
}

/// Reduce a side (its optional drv + that drv's observations) to a state.
///
/// Local observations are ground truth and win over the cache; a local success
/// beats a local failure (it *can* build). A direct failure outranks a
/// dependency failure (it's the more specific fact about this drv).
pub fn side_state(drv: &Option<String>, obs: &[Observation]) -> State {
    if drv.is_none() {
        return State::Absent;
    }
    let has = |src: Source, out: Outcome| {
        obs.iter().any(|o| o.source == src && o.outcome == out)
    };
    if has(Source::Local, Outcome::Built) {
        State::Built
    } else if has(Source::Local, Outcome::Failed) {
        State::Failed
    } else if has(Source::Local, Outcome::DepFailed) {
        State::Blocked
    } else if has(Source::Cache, Outcome::Built) {
        State::Built
    } else {
        State::Unknown
    }
}

/// One changed attr for a system: its drv on each side and each side's state.
pub struct Entry {
    pub attr: String,
    pub base_drv: Option<String>,
    pub head_drv: Option<String>,
    pub base: State,
    pub head: State,
}

/// The section a `(base, head)` pair belongs to: an emission-priority index
/// (lower = worse / more actionable, emitted first), a count noun, and a phrase.
fn cell(base: State, head: State) -> (usize, &'static str, &'static str) {
    use State::{Absent, Blocked, Built, Failed, Unknown};
    // Nouns are singular count-nouns (pluralized with a trailing "s" by the
    // renderer), so the phrase, not the noun, carries the before→after detail.
    match (base, head) {
        (Built, Failed) => (0, "regression", "build on the base, fail here"),
        (Built, Blocked) => (1, "blocked package", "build on the base, a dependency fails here"),
        (Absent, Failed) => (2, "new failure", "added here, fail to build"),
        (Absent, Blocked) => (3, "new blocked package", "added here, blocked by a failed dependency"),
        (Unknown, Failed) => (4, "failure", "fail here; base status unknown"),
        (Unknown, Blocked) => (5, "blocked package", "blocked here; base status unknown"),
        (Failed, Failed) => (6, "pre-existing failure", "fail on the base and here"),
        (Failed, Blocked) => (7, "pre-existing failure", "fail on the base, blocked here"),
        (Blocked, Failed) => (8, "pre-existing failure", "blocked on the base, fail here"),
        (Blocked, Blocked) => (9, "pre-existing blocked package", "blocked on the base and here"),
        (Built, Absent) => (10, "dropped package", "build on the base, gone here"),
        (Failed, Absent) => (11, "removed package", "failed on the base, gone here"),
        (Blocked, Absent) => (12, "removed package", "blocked on the base, gone here"),
        (Failed, Built) => (13, "fixed package", "fail on the base, build here"),
        (Blocked, Built) => (14, "fixed package", "blocked on the base, build here"),
        (Absent, Built) => (15, "new package", "new here, build"),
        (Unknown, Built) => (16, "built package", "build here; base status unknown"),
        (Built, Built) => (17, "unchanged package", "build on the base and here"),
        // Any leftover (e.g. Unknown↔Unknown under --no-build) — last, generic.
        _ => (18, "package", "see the before → after glyphs"),
    }
}

/// Render one section: its `before → after` header, then one bullet per group
/// of attrs sharing a derivation (`a = b = c`, shortest attr first).
fn render_section(base: State, head: State, entries: &[&Entry]) -> String {
    // Group attrs by their (base, head) drv pair — same pair ⇒ same build.
    let mut by_drv: BTreeMap<(Option<String>, Option<String>), Vec<String>> = BTreeMap::new();
    for e in entries {
        by_drv
            .entry((e.base_drv.clone(), e.head_drv.clone()))
            .or_default()
            .push(e.attr.clone());
    }
    let groups = by_drv.len();
    let attrs_total = entries.len();

    let (_, noun, phrase) = cell(base, head);
    let plural = if groups == 1 { "" } else { "s" };
    // Note the raw attr count too, but only when grouping actually collapsed rows.
    let note = if attrs_total != groups {
        format!(" ({attrs_total} attrs)")
    } else {
        String::new()
    };

    let mut s = format!(
        "<details><summary>{} → {} · <b>{groups} {noun}{plural}</b>{note} — {phrase}</summary>\n\n",
        base.glyph(),
        head.glyph(),
    );
    // One line per drv-group; within a line, shortest attr first; lines sorted.
    let mut lines: Vec<String> = by_drv
        .values()
        .map(|attrs| {
            let mut a = attrs.clone();
            a.sort_by(|x, y| x.len().cmp(&y.len()).then_with(|| x.cmp(y)));
            a.iter().map(|x| format!("`{x}`")).collect::<Vec<_>>().join(" = ")
        })
        .collect();
    lines.sort();
    for line in lines {
        s.push_str(&format!("- {line}\n"));
    }
    s.push_str("</details>\n\n");
    s
}

/// Render the per-system entries to Markdown, grouped into `before → after`
/// sections ordered worst-delta-first.
pub fn render(base: &str, head: &str, per_system: &[(String, Vec<Entry>)]) -> String {
    // Bare commit hashes (no code span) so GitHub auto-links them as short SHAs.
    let mut out = format!("## `npd` report: {base} → {head}\n");
    for (system, entries) in per_system {
        out.push_str(&format!("\n### `{system}`\n\n"));
        if entries.is_empty() {
            out.push_str("_No changed attrs._\n");
            continue;
        }
        // Bucket by (base, head) state, then emit buckets in priority order.
        let mut buckets: HashMap<(State, State), Vec<&Entry>> = HashMap::new();
        for e in entries {
            buckets.entry((e.base, e.head)).or_default().push(e);
        }
        let mut keys: Vec<(State, State)> = buckets.keys().copied().collect();
        keys.sort_by_key(|(b, h)| cell(*b, *h).0);
        for (b, h) in keys {
            out.push_str(&render_section(b, h, &buckets[&(b, h)]));
        }
    }
    out
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
            system: None,
            duration_s: None,
            machine: None,
        }
    }

    #[test]
    fn state_reduction() {
        // No drv on a side is Absent, not Unknown.
        assert_eq!(side_state(&None, &[]), State::Absent);
        // A drv with no facts is Unknown (distinct from Absent).
        let d = Some("/nix/store/x.drv".to_string());
        assert_eq!(side_state(&d, &[]), State::Unknown);
        // Direct vs transitive failures are distinguished.
        assert_eq!(side_state(&d, &[obs(Source::Local, Outcome::Failed)]), State::Failed);
        assert_eq!(side_state(&d, &[obs(Source::Local, Outcome::DepFailed)]), State::Blocked);
        // Cache success reads as Built; a local build wins over it.
        assert_eq!(side_state(&d, &[obs(Source::Cache, Outcome::Built)]), State::Built);
        let s = side_state(
            &d,
            &[obs(Source::Cache, Outcome::Built), obs(Source::Local, Outcome::Failed)],
        );
        assert_eq!(s, State::Failed);
    }

    fn entry(attr: &str, base: State, head: State, bd: Option<&str>, hd: Option<&str>) -> Entry {
        Entry {
            attr: attr.into(),
            base_drv: bd.map(str::to_string),
            head_drv: hd.map(str::to_string),
            base,
            head,
        }
    }

    #[test]
    fn render_sections_tokens_grouping_and_folding() {
        let entries = vec![
            // regression (state changed → open)
            entry("pkgA", State::Built, State::Failed, Some("/b/a.drv"), Some("/h/a.drv")),
            // two distinct blocked drvs (state changed → open), transitive glyph 🚫
            entry("dep1", State::Built, State::Blocked, Some("/b/d1"), Some("/h/d1")),
            entry("dep2", State::Built, State::Blocked, Some("/b/d2"), Some("/h/d2")),
            // two attrs sharing one drv, unchanged (state same → collapsed, grouped)
            entry("z.foo", State::Built, State::Built, Some("/b/f"), Some("/h/f")),
            entry("foo", State::Built, State::Built, Some("/b/f"), Some("/h/f")),
        ];
        let out = render("base", "head", &[("aarch64-linux".into(), entries)]);

        // Composable tokens and the transitive distinction.
        assert!(out.contains("✅ → ❌ · <b>1 regression</b>"), "{out}");
        assert!(out.contains("✅ → 🚫 · <b>2 blocked packages</b>"), "{out}");
        // Grouping: shared drv collapses to one equals-joined line, shortest first.
        assert!(out.contains("- `foo` = `z.foo`"), "{out}");
        assert!(out.contains("✅ → ✅ · <b>1 unchanged package</b> (2 attrs)"), "{out}");
        // All sections are folded closed.
        assert!(out.contains("<details><summary>✅ → ❌"), "{out}");
        assert!(out.contains("<details><summary>✅ → ✅"), "{out}");
        assert!(!out.contains("<details open>"), "{out}");
        // Ordering: regression before blocked before unchanged.
        let reg = out.find("→ ❌").unwrap();
        let blk = out.find("→ 🚫").unwrap();
        let unch = out.find("✅ → ✅").unwrap();
        assert!(reg < blk && blk < unch, "{out}");
    }
}
