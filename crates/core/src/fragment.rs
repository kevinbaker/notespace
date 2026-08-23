//! Fragments: contiguous runs of top-level subtrees, addressed as a unit.
//!
//! A thread page is one cache entry today, so any write to the thread invalidates all of it —
//! a reply to one subtree costs a rebake of every other. A fragment is a smaller unit of the
//! same tree, so a write dirties only the part it landed in.
//!
//! The materialized path already provides the boundary. Fragment `k` covers top-level ordinals
//! `[k·SPAN, (k+1)·SPAN)`, and because a descendant's path begins with its root's segment, the
//! whole subtree falls inside the same range:
//!
//! ```text
//!   fragment 1, SPAN = 8   ->   path >= "0008"  AND  path < "000G"
//!
//!   0008          in     (root)
//!   0008.0003     in     (descendant: "0008." sorts above "0008", below "000G")
//!   000F.0001     in     (last root in the range, and its subtree)
//!   000G          out    (first root of fragment 2)
//! ```
//!
//! **A subtree is never split across fragments.** That is the property the whole scheme rests
//! on: a reply always lands inside exactly one fragment, so exactly one has to be rebaked. It
//! holds because boundaries are placed between *top-level* ordinals, never inside one.
//!
//! # What this version does not do
//!
//! Boundaries are fixed arithmetic, not packed by size. Real threads are lopsided — a measured
//! 204-post thread had a median subtree of 1 post and a maximum of 37 — so fixed spans give
//! uneven fragments. Packing adjacent subtrees to a target size needs the boundaries stored
//! somewhere, which needs a schema change; this version needs none, because `containing` and
//! `bounds` are pure arithmetic over the path.

use crate::path::Path;

/// Top-level subtrees per fragment.
///
/// Chosen so that a lopsided thread still splits: too large and everything lands in fragment 0,
/// too small and a page needs many lookups to assemble.
pub const FRAGMENT_SPAN: u32 = 8;

/// One fragment of a thread's post tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Fragment(u32);

impl Fragment {
    /// Fragment `index`, counting from 0.
    pub const fn new(index: u32) -> Self {
        Fragment(index)
    }

    pub const fn index(&self) -> u32 {
        self.0
    }

    /// The fragment a post belongs to, from its path.
    ///
    /// Only the top-level segment matters, so depth is irrelevant: a post and its whole subtree
    /// always answer the same.
    pub fn containing(path: &Path) -> Self {
        Fragment(path.segments().next().unwrap_or(0) / FRAGMENT_SPAN)
    }

    /// The half-open path range `[start, end)` this fragment covers.
    ///
    /// `end` is `None` for the last fragment the path encoding can express, where there is no
    /// next root to bound against — the caller reads to the end of the thread.
    pub fn bounds(&self) -> (String, Option<String>) {
        let first = self.0.saturating_mul(FRAGMENT_SPAN);
        let start = Path::root(first)
            .map(|p| p.into_string())
            // An out-of-range fragment addresses nothing; a start above every real path is the
            // honest answer, not a panic.
            .unwrap_or_else(|_| "~".into());
        let end = first
            .checked_add(FRAGMENT_SPAN)
            .and_then(|next| Path::root(next).ok())
            .map(|p| p.into_string());
        (start, end)
    }

    /// The next fragment, for a "continue" link.
    pub const fn next(&self) -> Self {
        Fragment(self.0 + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frag(p: &str) -> u32 {
        Fragment::containing(&Path::parse(p).unwrap()).index()
    }

    #[test]
    fn top_level_ordinals_group_by_span() {
        assert_eq!(frag("0000"), 0);
        assert_eq!(frag("0007"), 0);
        assert_eq!(frag("0008"), 1);
        assert_eq!(frag("000F"), 1);
        assert_eq!(frag("000G"), 2);
    }

    /// The property everything else depends on: a reply lands in the same fragment as the post
    /// it answers, however deep it is, so one write dirties exactly one fragment.
    #[test]
    fn a_subtree_is_never_split_across_fragments() {
        for root in 0..64u32 {
            let r = Path::root(root).unwrap();
            let expected = Fragment::containing(&r);
            let mut child = r.child(1).unwrap();
            for _ in 0..6 {
                assert_eq!(
                    Fragment::containing(&child),
                    expected,
                    "{child} left the fragment of {r}"
                );
                child = child.child(1).unwrap();
            }
            // And a late sibling deep in the subtree, not just the first child.
            let wide = r.child(1000).unwrap();
            assert_eq!(Fragment::containing(&wide), expected);
        }
    }

    /// Bounds must agree with `containing`, or a fragment would render posts that do not belong
    /// to it — or silently drop ones that do.
    #[test]
    fn bounds_contain_exactly_the_paths_that_map_here() {
        for index in 0..8u32 {
            let f = Fragment::new(index);
            let (start, end) = f.bounds();
            for root in 0..64u32 {
                let p = Path::root(root).unwrap();
                let path = p.as_str();
                let in_range = path >= start.as_str() && end.as_deref().is_none_or(|e| path < e);
                assert_eq!(
                    in_range,
                    Fragment::containing(&p) == f,
                    "fragment {index} bounds [{start}, {end:?}) disagree about {path}"
                );
                // Descendants must land the same way, which is what makes the range scan sound.
                let deep = p.child(3).unwrap().child(9).unwrap();
                let deep_in = deep.as_str() >= start.as_str()
                    && end.as_deref().is_none_or(|e| deep.as_str() < e);
                assert_eq!(
                    deep_in,
                    Fragment::containing(&deep) == f,
                    "fragment {index} bounds disagree about descendant {deep}"
                );
            }
        }
    }

    #[test]
    fn bounds_are_ordered_and_adjacent_fragments_do_not_overlap() {
        for index in 0..16u32 {
            let (start, end) = Fragment::new(index).bounds();
            if let Some(e) = &end {
                assert!(start < *e, "fragment {index} bounds inverted");
                let (next_start, _) = Fragment::new(index).next().bounds();
                assert_eq!(*e, next_start, "gap or overlap after fragment {index}");
            }
        }
    }

    /// An index past what the path encoding can express must not panic; it addresses nothing.
    #[test]
    fn an_out_of_range_fragment_is_empty_rather_than_a_panic() {
        let (start, end) = Fragment::new(u32::MAX).bounds();
        assert!(end.is_none());
        assert!(!start.is_empty());
    }
}
