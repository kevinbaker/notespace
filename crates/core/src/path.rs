//! Materialized tree paths: a `.`-separated list of zero-padded base32 ordinals, one per level.
//!
//! ```text
//! 000C             a root post (12th top-level reply in its thread)
//! 000C.0004        its 4th direct child
//! 000C.0004.0001   that child's 1st child
//! ```
//!
//! The separator `0x2E` sorts below every alphabet byte and segments are fixed-width, so
//! bytewise `ORDER BY path` is a depth-first preorder walk and one indexed range scan returns a
//! whole thread page. The property tests in this module are what hold that up.

use core::fmt;
use std::borrow::Cow;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

/// Crockford base32, strictly ascending in ASCII so bytewise comparison is numeric comparison.
pub const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Four base32 digits give 1_048_576 siblings at any one level.
pub const SEGMENT_WIDTH: usize = 4;

/// Largest ordinal representable in a single segment (`32^4 - 1`).
pub const MAX_ORDINAL: u32 = 1_048_575;

/// Hard ceiling on nesting, independent of a space's `depth_cap`. Bounds a stored path to 159
/// bytes, keeping `idx_post_thread_path` compact.
pub const MAX_DEPTH: usize = 32;

const SEPARATOR: u8 = b'.';

/// Reverse of [`ALPHABET`]. A linear scan here measured ~2x slower on the deep-nesting fixture.
const INVALID: u8 = 0xFF;

const DECODE: [u8; 256] = {
    let mut table = [INVALID; 256];
    let mut i = 0;
    while i < ALPHABET.len() {
        table[ALPHABET[i] as usize] = i as u8;
        i += 1;
    }
    table
};

#[inline]
const fn digit_value(b: u8) -> u8 {
    DECODE[b as usize]
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("path ordinal {0} exceeds maximum {MAX_ORDINAL}")]
    OrdinalTooLarge(u32),
    #[error("path depth {0} exceeds maximum {MAX_DEPTH}")]
    TooDeep(usize),
    #[error("malformed path segment {segment:?} in {path:?}")]
    Malformed { path: String, segment: String },
    #[error("path is empty")]
    Empty,
}

/// A validated materialized path: fixed-width numeric segments, depth within [`MAX_DEPTH`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Path(String);

impl Path {
    /// Path of a top-level post with the given ordinal.
    pub fn root(ordinal: u32) -> Result<Self, PathError> {
        Ok(Path(encode_segment(ordinal)?))
    }

    /// Path of the `ordinal`-th direct child of `self`.
    pub fn child(&self, ordinal: u32) -> Result<Self, PathError> {
        let depth = self.depth() + 1;
        if depth >= MAX_DEPTH {
            return Err(PathError::TooDeep(depth));
        }
        let mut s = String::with_capacity(self.0.len() + 1 + SEGMENT_WIDTH);
        s.push_str(&self.0);
        s.push(SEPARATOR as char);
        s.push_str(&encode_segment(ordinal)?);
        Ok(Path(s))
    }

    /// `Err` if `self` is already at [`MAX_ORDINAL`] for its level.
    pub fn next_sibling(&self) -> Result<Self, PathError> {
        let ordinal = self.ordinal();
        let next = ordinal
            .checked_add(1)
            .ok_or(PathError::OrdinalTooLarge(u32::MAX))?;
        match self.parent() {
            Some(parent) => parent.child(next),
            None => Path::root(next),
        }
    }

    pub fn parse(s: &str) -> Result<Self, PathError> {
        if s.is_empty() {
            return Err(PathError::Empty);
        }
        let mut depth = 0usize;
        for segment in s.split(SEPARATOR as char) {
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(PathError::TooDeep(depth));
            }
            let valid = segment.len() == SEGMENT_WIDTH
                && segment.bytes().all(|b| digit_value(b) != INVALID);
            if !valid {
                return Err(PathError::Malformed {
                    path: s.to_owned(),
                    segment: segment.to_owned(),
                });
            }
        }
        Ok(Path(s.to_owned()))
    }

    /// Depth in the tree. Top-level posts are depth 0.
    pub fn depth(&self) -> usize {
        self.0.bytes().filter(|&b| b == SEPARATOR).count()
    }

    /// This path's own ordinal at its level.
    pub fn ordinal(&self) -> u32 {
        let last = match self.0.rfind(SEPARATOR as char) {
            Some(i) => &self.0[i + 1..],
            None => &self.0[..],
        };
        decode_segment(last)
    }

    /// The ancestor at `depth`, using the [`Path::depth`] convention: a root path is depth 0.
    /// Truncating the deepest descendant of P to `P.depth() + 1` is how the next child ordinal
    /// is found without scanning P's children.
    pub fn ancestor_at_depth(&self, depth: usize) -> Option<Self> {
        if depth > self.depth() {
            return None;
        }
        let mut p = self.clone();
        while p.depth() > depth {
            p = p.parent()?;
        }
        Some(p)
    }

    pub fn parent(&self) -> Option<Self> {
        self.0
            .rfind(SEPARATOR as char)
            .map(|i| Path(self.0[..i].to_owned()))
    }

    /// Every ordinal from root to this node.
    pub fn segments(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.split(SEPARATOR as char).map(decode_segment)
    }

    /// True if `self` is `other` or lies beneath it.
    pub fn is_descendant_of(&self, other: &Path) -> bool {
        self.0 == other.0
            || (self.0.len() > other.0.len()
                && self.0.as_bytes()[other.0.len()] == SEPARATOR
                && self.0.starts_with(&other.0))
    }

    /// Exclusive upper bound for `WHERE path >= :path AND path < :bound` over the subtree.
    pub fn subtree_end(&self) -> String {
        // '/' (0x2F) is one above the separator and still below every alphabet byte, so it is
        // the tight bound on `<self>.` descendants and sorts below the next sibling.
        let mut s = String::with_capacity(self.0.len() + 1);
        s.push_str(&self.0);
        s.push('/');
        s
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Path {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<Path> for Cow<'static, str> {
    fn from(p: Path) -> Self {
        Cow::Owned(p.0)
    }
}

impl Serialize for Path {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

/// Validates rather than trusting input; there is no unchecked constructor.
impl<'de> Deserialize<'de> for Path {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Path::parse(&s).map_err(de::Error::custom)
    }
}

fn encode_segment(ordinal: u32) -> Result<String, PathError> {
    if ordinal > MAX_ORDINAL {
        return Err(PathError::OrdinalTooLarge(ordinal));
    }
    // Most-significant digit first, so lexicographic order matches numeric order.
    let mut buf = [b'0'; SEGMENT_WIDTH];
    let mut n = ordinal;
    for slot in buf.iter_mut().rev() {
        slot.clone_from(&ALPHABET[(n % 32) as usize]);
        n /= 32;
    }
    // Pushing `char`s avoids a fallible UTF-8 step; every ALPHABET byte is ASCII.
    let mut out = String::with_capacity(SEGMENT_WIDTH);
    for b in buf {
        out.push(b as char);
    }
    Ok(out)
}

/// Only called on strings that passed [`Path::parse`]; an unknown byte maps to 0 rather than
/// panicking, because a wasm panic is an unrecoverable trap.
fn decode_segment(s: &str) -> u32 {
    s.bytes().fold(0u32, |acc, b| {
        let digit = digit_value(b);
        acc * 32 + if digit == INVALID { 0 } else { digit as u32 }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn root_is_zero_padded_base32() {
        assert_eq!(Path::root(0).unwrap().as_str(), "0000");
        assert_eq!(Path::root(1).unwrap().as_str(), "0001");
        assert_eq!(Path::root(12).unwrap().as_str(), "000C");
        assert_eq!(Path::root(31).unwrap().as_str(), "000Z");
        assert_eq!(Path::root(32).unwrap().as_str(), "0010");
        assert_eq!(Path::root(MAX_ORDINAL).unwrap().as_str(), "ZZZZ");
    }

    #[test]
    fn matches_design_doc_example() {
        let p = Path::root(12).unwrap().child(4).unwrap().child(1).unwrap();
        assert_eq!(p.as_str(), "000C.0004.0001");
        assert_eq!(p.depth(), 2);
    }

    #[test]
    fn decode_table_agrees_with_alphabet() {
        for (i, &b) in ALPHABET.iter().enumerate() {
            assert_eq!(digit_value(b), i as u8, "table wrong for {}", b as char);
        }
        for b in [b'.', b'/', b'I', b'L', b'O', b'U', b'a', b'z', 0u8, 255u8] {
            assert_eq!(digit_value(b), INVALID, "{} should be invalid", b as char);
        }
    }

    #[test]
    fn ancestor_at_depth_uses_the_same_convention_as_depth() {
        let root = Path::parse("0006").unwrap();
        assert_eq!(root.depth(), 0, "a root path is depth 0");
        assert_eq!(root.ancestor_at_depth(0).as_ref(), Some(&root));
        assert_eq!(root.ancestor_at_depth(1), None, "nothing below a root");

        let deep = Path::parse("0001.0002.0003").unwrap();
        assert_eq!(deep.depth(), 2);
        assert_eq!(deep.ancestor_at_depth(0).unwrap().as_str(), "0001");
        assert_eq!(deep.ancestor_at_depth(1).unwrap().as_str(), "0001.0002");
        assert_eq!(
            deep.ancestor_at_depth(2).unwrap().as_str(),
            "0001.0002.0003"
        );
        assert_eq!(deep.ancestor_at_depth(3), None);
    }

    #[test]
    fn alphabet_is_ascending_and_unambiguous() {
        for w in ALPHABET.windows(2) {
            assert!(w[0] < w[1], "alphabet not ascending at {:?}", w);
        }
        assert_eq!(ALPHABET.len(), 32);
        for c in [b'I', b'L', b'O', b'U'] {
            assert!(
                !ALPHABET.contains(&c),
                "ambiguous char {} in alphabet",
                c as char
            );
        }
        assert!(SEPARATOR < ALPHABET[0]);
        assert!(b'/' < ALPHABET[0]);
    }

    #[test]
    fn encode_decode_round_trips_across_the_range() {
        for ord in [0, 1, 31, 32, 1023, 1024, 65_535, 999_999, MAX_ORDINAL] {
            let p = Path::root(ord).unwrap();
            assert_eq!(p.ordinal(), ord, "round trip failed for {ord}");
        }
    }

    #[test]
    fn encoding_order_matches_numeric_order() {
        let mut prev = Path::root(0).unwrap();
        for ord in 1..5000u32 {
            let cur = Path::root(ord).unwrap();
            assert!(prev.as_str() < cur.as_str(), "order broke at {ord}");
            prev = cur;
        }
    }

    #[test]
    fn ordinal_overflows_are_rejected_not_wrapped() {
        assert_eq!(
            Path::root(MAX_ORDINAL + 1),
            Err(PathError::OrdinalTooLarge(MAX_ORDINAL + 1))
        );
        let last = Path::root(MAX_ORDINAL).unwrap();
        assert!(last.next_sibling().is_err());
    }

    #[test]
    fn depth_is_capped() {
        let mut p = Path::root(1).unwrap();
        for _ in 0..MAX_DEPTH - 1 {
            p = p.child(1).unwrap();
        }
        assert_eq!(p.depth(), MAX_DEPTH - 1);
        assert_eq!(p.child(1), Err(PathError::TooDeep(MAX_DEPTH)));
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(Path::parse("").is_err());
        assert!(Path::parse("12").is_err(), "unpadded segment");
        assert!(Path::parse("00012").is_err(), "overlong segment");
        assert!(
            Path::parse("000c").is_err(),
            "lowercase is not in the alphabet"
        );
        assert!(Path::parse("000I").is_err(), "excluded ambiguous char");
        assert!(Path::parse("000-").is_err(), "non-alphabet byte");
        assert!(Path::parse("000C.").is_err(), "trailing separator");
        assert!(Path::parse("000C.0004").is_ok());
    }

    #[test]
    fn parent_and_descendant() {
        let root = Path::root(12).unwrap();
        let child = root.child(4).unwrap();
        let grandchild = child.child(1).unwrap();

        assert_eq!(root.parent(), None);
        assert_eq!(child.parent(), Some(root.clone()));
        assert_eq!(grandchild.parent(), Some(child.clone()));

        assert!(grandchild.is_descendant_of(&root));
        assert!(child.is_descendant_of(&root));
        assert!(root.is_descendant_of(&root), "reflexive");
        assert!(!root.is_descendant_of(&child));
    }

    #[test]
    fn descendant_check_is_not_fooled_by_shared_prefix() {
        let a = Path::parse("0001").unwrap();
        let b = Path::parse("0012").unwrap();
        assert!(!b.is_descendant_of(&a));
        assert!(!a.is_descendant_of(&b));
    }

    #[test]
    fn subtree_end_bounds_exactly_the_subtree() {
        let root = Path::root(12).unwrap();
        let end = root.subtree_end();
        let inside = root.child(4).unwrap();
        let deep = inside.child(MAX_ORDINAL).unwrap();
        let next = Path::root(13).unwrap();

        assert!(root.as_str() >= root.as_str() && root.as_str() < end.as_str());
        assert!(inside.as_str() < end.as_str());
        assert!(deep.as_str() < end.as_str());
        assert!(next.as_str() > end.as_str(), "next sibling excluded");
        assert_eq!(end, "000C/");
    }

    /// A list of paths built by random walks, plus their preorder.
    fn arb_tree() -> impl Strategy<Value = Vec<Path>> {
        prop::collection::vec(prop::collection::vec(0u32..6, 1..5), 1..40).prop_map(|walks| {
            let mut paths: Vec<Path> = walks
                .into_iter()
                .filter_map(|walk| {
                    let mut it = walk.into_iter();
                    let mut p = Path::root(it.next()?).ok()?;
                    for step in it {
                        p = p.child(step).ok()?;
                    }
                    Some(p)
                })
                .collect();
            paths.sort();
            paths.dedup();
            paths
        })
    }

    proptest! {
        #[test]
        fn lexicographic_order_equals_preorder(paths in arb_tree()) {
            let mut by_string = paths.clone();
            by_string.sort_by(|a, b| a.as_str().cmp(b.as_str()));

            let mut by_tree = paths.clone();
            by_tree.sort_by(|a, b| {
                // Compare ordinal-by-ordinal; a prefix (ancestor) comes first.
                let (mut x, mut y) = (a.segments(), b.segments());
                loop {
                    match (x.next(), y.next()) {
                        (Some(i), Some(j)) if i == j => continue,
                        (Some(i), Some(j)) => break i.cmp(&j),
                        (None, Some(_)) => break std::cmp::Ordering::Less,
                        (Some(_), None) => break std::cmp::Ordering::Greater,
                        (None, None) => break std::cmp::Ordering::Equal,
                    }
                }
            });

            prop_assert_eq!(by_string, by_tree);
        }

        #[test]
        fn parent_precedes_children(paths in arb_tree()) {
            for p in &paths {
                if let Some(parent) = p.parent() {
                    prop_assert!(parent.as_str() < p.as_str());
                }
            }
        }

        #[test]
        fn subtree_range_is_exact(paths in arb_tree()) {
            for anchor in &paths {
                let end = anchor.subtree_end();
                for p in &paths {
                    let in_range = p.as_str() >= anchor.as_str() && p.as_str() < end.as_str();
                    prop_assert_eq!(
                        in_range,
                        p.is_descendant_of(anchor),
                        "path {:?} vs anchor {:?}", p.as_str(), anchor.as_str()
                    );
                }
            }
        }

        #[test]
        fn parse_round_trips(paths in arb_tree()) {
            for p in &paths {
                prop_assert_eq!(&Path::parse(p.as_str()).unwrap(), p);
            }
        }

        #[test]
        fn segment_round_trips(ord in 0u32..=MAX_ORDINAL) {
            prop_assert_eq!(Path::root(ord)?.ordinal(), ord);
        }

        #[test]
        fn segment_order_matches_numeric(a in 0u32..=MAX_ORDINAL, b in 0u32..=MAX_ORDINAL) {
            let (pa, pb) = (Path::root(a)?, Path::root(b)?);
            prop_assert_eq!(pa.as_str().cmp(pb.as_str()), a.cmp(&b));
        }

        #[test]
        fn next_sibling_follows_whole_subtree(depth in 1usize..6, ord in 0u32..900) {
            let mut p = Path::root(ord)?;
            for _ in 0..depth {
                p = p.child(ord)?;
            }
            let anchor = Path::root(ord)?;
            let sibling = anchor.next_sibling()?;
            prop_assert!(p.as_str() < sibling.as_str());
        }
    }
}
