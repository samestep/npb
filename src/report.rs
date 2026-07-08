//! Render a Markdown report of a change (base -> head): for each attr in the
//! changed set, derive a *base* and *head* build verdict from the observation
//! log, classify the delta, and group. Read-only over stored facts (DESIGN.md
//! §8); `npd report` populates those facts itself by building first.

use crate::diff::{DiffEntry, DiffKind};
use crate::model::{Observation, Outcome, Source};

/// A side's (base or head) build verdict, reduced from its observations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Built,
    Failed,
    /// The attr has a derivation on this side but we have no build fact for it.
    Unknown,
    /// The attr does not exist on this side (no derivation) — a *known* fact,
    /// not an unknown: rendered as `—`, never `?`.
    Absent,
}

/// A verdict plus the source that decided it (`L`ocal build / `C`ache) and
/// whether local observations disagree (flaky).
#[derive(Clone, Copy)]
pub struct Side {
    pub verdict: Verdict,
    pub tag: &'static str,
    pub flaky: bool,
}

/// Reduce a drv's observations to a verdict. Local builds are ground truth and
/// win over the cache; a local success beats a local failure (it *can* build)
/// but the disagreement is flagged flaky.
pub fn side_verdict(obs: &[Observation]) -> Side {
    let is = |src: Source, built: bool| {
        obs.iter().any(|o| {
            o.source == src
                && match o.outcome {
                    Outcome::Built => built,
                    Outcome::Failed | Outcome::DepFailed => !built,
                    Outcome::NotAttempted => false,
                }
        })
    };
    let (lb, lf) = (is(Source::Local, true), is(Source::Local, false));
    if lb || lf {
        return Side {
            verdict: if lb { Verdict::Built } else { Verdict::Failed },
            tag: "L",
            flaky: lb && lf,
        };
    }
    if is(Source::Cache, true) {
        return Side { verdict: Verdict::Built, tag: "C", flaky: false };
    }
    Side { verdict: Verdict::Unknown, tag: "", flaky: false }
}

/// The verdict for a side, given whether the attr has a derivation there.
/// No drv ⇒ `Absent` (a fact); a drv with no observations ⇒ `Unknown`.
fn side_for(drv: &Option<String>, obs: &[Observation]) -> Side {
    match drv {
        None => Side { verdict: Verdict::Absent, tag: "", flaky: false },
        Some(_) => side_verdict(obs),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    Regression,
    Unverified,
    Fixed,
    NewlyFails,
    Dropped,
    PreExisting,
    NewlyBuilds,
    Builds,
}

/// Emission order (worst/most-actionable first) with emoji + heading.
pub const CATEGORIES: &[(Category, &str, &str)] = &[
    (Category::Regression, "🔴", "regression — builds on base, fails here"),
    (Category::Unverified, "❔", "unverified — need a base/head build to classify"),
    (Category::Fixed, "🟢", "fixed — fails on base, builds here"),
    (Category::NewlyFails, "🟣", "added but fails to build"),
    (Category::Dropped, "🟠", "dropped — present on base, gone here"),
    (Category::PreExisting, "⚪", "pre-existing failure — fails on base and here"),
    (Category::NewlyBuilds, "🔵", "added, builds"),
    (Category::Builds, "✅", "builds (unchanged or base already green)"),
];

pub fn classify(kind: DiffKind, base: Verdict, head: Verdict) -> Category {
    use Verdict::{Built, Failed};
    match kind {
        DiffKind::Added => match head {
            Built => Category::NewlyBuilds,
            Failed => Category::NewlyFails,
            _ => Category::Unverified,
        },
        DiffKind::Removed => Category::Dropped,
        DiffKind::Changed | DiffKind::Unchanged => match (base, head) {
            (Built, Failed) => Category::Regression,
            (Failed, Built) => Category::Fixed,
            (Failed, Failed) => Category::PreExisting,
            (_, Built) => Category::Builds,
            // head fails/unknown with an unknown-or-absent base -> needs a build
            _ => Category::Unverified,
        },
    }
}

fn glyph(v: Verdict) -> &'static str {
    match v {
        Verdict::Built => "✓",
        Verdict::Failed => "✗",
        Verdict::Unknown => "?",
        Verdict::Absent => "—",
    }
}

fn fmt_side(s: Side) -> String {
    if s.tag.is_empty() {
        glyph(s.verdict).to_string()
    } else {
        format!("{}({})", glyph(s.verdict), s.tag)
    }
}

/// One classified attr for a system.
pub struct Row {
    pub attr: String,
    pub category: Category,
    pub base: Side,
    pub head: Side,
}

/// Build a row from a diff entry, looking up each side's verdict. A side with no
/// derivation is `Absent` (rendered `—`), distinct from a present-but-unbuilt
/// `Unknown` (`?`).
pub fn row_for(
    entry: &DiffEntry,
    base_obs: &[Observation],
    head_obs: &[Observation],
) -> Row {
    let base = side_for(&entry.base_drv, base_obs);
    let head = side_for(&entry.head_drv, head_obs);
    Row {
        attr: entry.attr.clone(),
        category: classify(entry.kind, base.verdict, head.verdict),
        base,
        head,
    }
}

/// Render the per-system rows to Markdown, grouped by category.
pub fn render(base: &str, head: &str, per_system: &[(String, Vec<Row>)]) -> String {
    let mut out = format!("## `npd` report: `{base}` → `{head}`\n\n");
    out.push_str(
        "Verdicts from the observation log — `base → head`, source tag `L` local build / \
         `C` binary cache. `✓` builds, `✗` fails, `—` absent (no such attr on that side), \
         `?` unbuilt.\n",
    );
    for (system, rows) in per_system {
        out.push_str(&format!("\n### `{system}`\n"));
        if rows.is_empty() {
            out.push_str("\n_No changed attrs._\n");
            continue;
        }
        for (cat, emoji, heading) in CATEGORIES {
            let group: Vec<&Row> = rows.iter().filter(|r| r.category == *cat).collect();
            if group.is_empty() {
                continue;
            }
            out.push_str(&format!("\n<b>{emoji} {} {heading}</b>\n\n", group.len()));
            for r in group {
                let flaky = if r.base.flaky || r.head.flaky {
                    " · ⚠️ flaky locally"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "- `{}`: {} → {}{flaky}\n",
                    r.attr,
                    fmt_side(r.base),
                    fmt_side(r.head)
                ));
            }
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
            cached: None,
            machine: None,
            log_ref: None,
        }
    }

    #[test]
    fn verdict_prefers_local_and_flags_flaky() {
        assert_eq!(side_verdict(&[]).verdict, Verdict::Unknown);
        assert_eq!(
            side_verdict(&[obs(Source::Cache, Outcome::Built)]).verdict,
            Verdict::Built
        );
        // local failure overrides a cache success as the "ground truth" source...
        let s = side_verdict(&[
            obs(Source::Cache, Outcome::Built),
            obs(Source::Local, Outcome::Failed),
        ]);
        assert_eq!(s.verdict, Verdict::Failed);
        assert_eq!(s.tag, "L");
        // ...but a local success + local failure = flaky, and success wins.
        let f = side_verdict(&[
            obs(Source::Local, Outcome::Failed),
            obs(Source::Local, Outcome::Built),
        ]);
        assert_eq!(f.verdict, Verdict::Built);
        assert!(f.flaky);
    }

    #[test]
    fn render_shows_absent_as_dash_not_question() {
        use crate::diff::{DiffEntry, DiffKind};
        // A Removed attr: present (built) on base, absent on head.
        let entry = DiffEntry {
            attr: "gone".into(),
            base_drv: Some("/nix/store/a.drv".into()),
            head_drv: None,
            kind: DiffKind::Removed,
            attribution: None,
        };
        let row = row_for(&entry, &[obs(Source::Cache, Outcome::Built)], &[]);
        let out = render("b", "h", &[("aarch64-linux".into(), vec![row])]);
        assert!(out.contains("`gone`: ✓(C) → —"), "got:\n{out}");
        assert!(!out.contains("→ ?"), "absent head must not render as `?`:\n{out}");
    }

    #[test]
    fn absent_side_is_not_unknown() {
        // No drv on a side -> Absent (`—`), distinct from a present-but-unbuilt `?`.
        let s = side_for(&None, &[]);
        assert_eq!(s.verdict, Verdict::Absent);
        let u = side_for(&Some("/nix/store/x.drv".into()), &[]);
        assert_eq!(u.verdict, Verdict::Unknown);
    }

    #[test]
    fn classify_grid() {
        use Verdict::{Built, Failed, Unknown};
        let c = DiffKind::Changed;
        assert_eq!(classify(c, Built, Failed), Category::Regression);
        assert_eq!(classify(c, Failed, Built), Category::Fixed);
        assert_eq!(classify(c, Failed, Failed), Category::PreExisting);
        assert_eq!(classify(c, Built, Built), Category::Builds);
        assert_eq!(classify(c, Unknown, Failed), Category::Unverified);
        assert_eq!(classify(c, Unknown, Built), Category::Builds);
        assert_eq!(classify(DiffKind::Added, Unknown, Built), Category::NewlyBuilds);
        assert_eq!(classify(DiffKind::Added, Unknown, Failed), Category::NewlyFails);
        assert_eq!(classify(DiffKind::Removed, Built, Unknown), Category::Dropped);
    }
}
