//! Diff two evaluations into a set of changed attrs, optionally attributing each
//! change against the merge base (DESIGN.md §6). Pure over `AttrEval` slices.
//!
//! The diff is drv-centric: an attr's identity on a side is its `drv_path` (or
//! its absence). "No drv" covers both an absent attr and one that errored /
//! is unbuilt — for the rebuild question they're the same.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::AttrEval;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// No drv on base, a drv on head (new, or an eval error that now builds).
    Added,
    /// A drv on base, none on head (removed, or now errors).
    Removed,
    /// Both sides have a drv, but different ones (the rebuild set).
    Changed,
    /// Same drv on both sides (or no drv on either) — nothing to do.
    Unchanged,
}

/// Which side of the fork (relative to the merge base) produced a difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attribution {
    /// Only head diverged from the merge base — the change is head's doing.
    ByHead,
    /// Only base diverged — head matches the fork point; the base branch moved.
    ByBase,
    /// Both diverged from the merge base — a genuine interaction.
    ByBoth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    pub attr: String,
    pub base_drv: Option<String>,
    pub head_drv: Option<String>,
    pub kind: DiffKind,
    /// Set only for non-`Unchanged` entries when a merge base was supplied.
    pub attribution: Option<Attribution>,
}

fn drv_map(evals: &[AttrEval]) -> BTreeMap<&str, Option<&str>> {
    evals
        .iter()
        .map(|a| (a.attr.as_str(), a.drv_path.as_deref()))
        .collect()
}

/// Diff `base` vs `head`. If `merge_base` is given, attribute each real change.
pub fn diff_evals(
    base: &[AttrEval],
    head: &[AttrEval],
    merge_base: Option<&[AttrEval]>,
) -> Vec<DiffEntry> {
    let b = drv_map(base);
    let h = drv_map(head);
    let m = merge_base.map(drv_map);

    let keys: BTreeSet<&str> = b.keys().chain(h.keys()).copied().collect();
    let mut out = Vec::with_capacity(keys.len());
    for attr in keys {
        // `Option<Option<&str>>` flattened: None means "no drv on this side",
        // whether the attr was absent or present-without-drv.
        let bd = b.get(attr).copied().flatten();
        let hd = h.get(attr).copied().flatten();

        let kind = match (bd, hd) {
            (None, None) => DiffKind::Unchanged,
            (None, Some(_)) => DiffKind::Added,
            (Some(_), None) => DiffKind::Removed,
            (Some(x), Some(y)) if x == y => DiffKind::Unchanged,
            (Some(_), Some(_)) => DiffKind::Changed,
        };

        let attribution = match &m {
            Some(m) if kind != DiffKind::Unchanged => {
                let md = m.get(attr).copied().flatten();
                let base_changed = bd != md;
                let head_changed = hd != md;
                Some(match (base_changed, head_changed) {
                    (true, true) => Attribution::ByBoth,
                    (false, true) => Attribution::ByHead,
                    // Only base moved: head still matches the fork point.
                    (true, false) => Attribution::ByBase,
                    // Unreachable: kind != Unchanged implies bd != hd, so at
                    // least one differs from md; fall back to ByBoth.
                    (false, false) => Attribution::ByBoth,
                })
            }
            _ => None,
        };

        out.push(DiffEntry {
            attr: attr.to_string(),
            base_drv: bd.map(str::to_string),
            head_drv: hd.map(str::to_string),
            kind,
            attribution,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Existence;

    fn ev(attr: &str, drv: Option<&str>) -> AttrEval {
        AttrEval {
            attr: attr.into(),
            existence: if drv.is_some() {
                Existence::Buildable
            } else {
                Existence::Error
            },
            drv_path: drv.map(str::to_string),
            broken: None,
            unsupported: None,
            insecure: None,
            hydra_platforms_ok: None,
            error: None,
        }
    }

    fn find<'a>(d: &'a [DiffEntry], attr: &str) -> &'a DiffEntry {
        d.iter().find(|e| e.attr == attr).unwrap()
    }

    #[test]
    fn two_way_kinds() {
        let base = [ev("keep", Some("/d1")), ev("gone", Some("/d2")), ev("chg", Some("/old"))];
        let head = [ev("keep", Some("/d1")), ev("new", Some("/d3")), ev("chg", Some("/new"))];
        let d = diff_evals(&base, &head, None);

        assert_eq!(find(&d, "keep").kind, DiffKind::Unchanged);
        assert_eq!(find(&d, "gone").kind, DiffKind::Removed);
        assert_eq!(find(&d, "new").kind, DiffKind::Added);
        assert_eq!(find(&d, "chg").kind, DiffKind::Changed);
        // no merge base -> no attribution
        assert!(d.iter().all(|e| e.attribution.is_none()));
    }

    #[test]
    fn three_way_attribution() {
        let mb = [ev("a", Some("/m")), ev("b", Some("/m")), ev("c", Some("/m"))];
        // a: only head moved; b: only base moved; c: both moved differently
        let base = [ev("a", Some("/m")), ev("b", Some("/base")), ev("c", Some("/cbase"))];
        let head = [ev("a", Some("/head")), ev("b", Some("/m")), ev("c", Some("/chead"))];
        let d = diff_evals(&base, &head, Some(&mb));

        assert_eq!(find(&d, "a").attribution, Some(Attribution::ByHead));
        assert_eq!(find(&d, "b").attribution, Some(Attribution::ByBase));
        assert_eq!(find(&d, "c").attribution, Some(Attribution::ByBoth));
    }
}
