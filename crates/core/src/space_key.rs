//! Space names and the paths built from them: `/s/sports/hockey`, one [`SpaceKey`] per segment,
//! the whole thing a [`SpacePath`].
//!
//! Keys are unique per parent, not globally, so the path is the identifier and resolution is one
//! indexed lookup rather than a walk. Renames rewrite the path and may leave a tombstone row
//! carrying `moved_to`, which the same lookup finds, so a redirect costs no extra query.
//!
//! [`RESERVED`] is not the username list: it guards sub-routes under `/s/`.

use core::fmt;

use crate::naming::{self, NameError, Rules};

pub const MIN_CHARS: usize = 2;
pub const MAX_CHARS: usize = 32;

/// Separator between path segments, in both stored and displayed form.
pub const PATH_SEP: char = '/';

/// Deepest nesting, counting the top level as depth 1.
pub const MAX_DEPTH: usize = 3;

/// Names no space may take, kept sorted for binary search. Mostly ambiguous sub-routes.
pub const RESERVED: &[&str] = &[
    "about",
    "admin",
    "all",
    "api",
    "edit",
    "feed",
    "help",
    "new",
    "notespace",
    "null",
    "rss",
    "search",
    "settings",
    "undefined",
];

const RULES: Rules = Rules {
    kind: "space name",
    min: MIN_CHARS,
    max: MAX_CHARS,
    reserved: RESERVED,
};

/// One segment of a space path — `sports`, or `hockey`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpaceKey(String);

impl SpaceKey {
    pub fn parse(input: &str) -> Result<Self, NameError> {
        naming::validate(input, &RULES).map(SpaceKey)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SpaceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stored with a trailing separator (`"sports/hockey/"`), rendered without one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpacePath(String);

impl SpacePath {
    /// A top-level space.
    pub fn root(key: &SpaceKey) -> Self {
        SpacePath(format!("{key}{PATH_SEP}"))
    }

    /// Fails past [`MAX_DEPTH`].
    pub fn child(&self, key: &SpaceKey) -> Result<Self, SpacePathError> {
        let depth = self.depth() + 1;
        if depth > MAX_DEPTH {
            return Err(SpacePathError::TooDeep(depth));
        }
        Ok(SpacePath(format!("{}{key}{PATH_SEP}", self.0)))
    }

    /// The portion of a `/s/...` URL after the prefix, with or without a trailing slash.
    pub fn parse(url_path: &str) -> Result<Self, SpacePathError> {
        let mut path: Option<SpacePath> = None;
        for seg in url_path.split(PATH_SEP).filter(|s| !s.is_empty()) {
            let key = SpaceKey::parse(seg).map_err(SpacePathError::Segment)?;
            path = Some(match path {
                None => SpacePath::root(&key),
                Some(p) => p.child(&key)?,
            });
        }
        path.ok_or(SpacePathError::Empty)
    }

    /// Stored form, trailing separator included.
    pub fn as_stored(&self) -> &str {
        &self.0
    }

    /// URL form, trailing separator stripped.
    pub fn as_url(&self) -> &str {
        self.0.strip_suffix(PATH_SEP).unwrap_or(&self.0)
    }

    /// The canonical URL for this space.
    pub fn url(&self) -> String {
        format!("/s/{}", self.as_url())
    }

    pub fn segments(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.0.split(PATH_SEP).filter(|s| !s.is_empty())
    }

    pub fn depth(&self) -> usize {
        self.segments().count()
    }

    /// This space's own key.
    pub fn key(&self) -> &str {
        self.segments().next_back().unwrap_or("")
    }

    /// The parent path, or `None` at the top level.
    pub fn parent(&self) -> Option<SpacePath> {
        let cut = self.as_url().rfind(PATH_SEP)?;
        Some(SpacePath(self.as_url()[..=cut].to_string()))
    }

    /// Half-open range `[lo, hi)` selecting this space and every descendant. The trailing
    /// separator is load-bearing: a key may contain `-` (0x2D), which sorts below `/` (0x2F), so
    /// an untrailed range would swallow `sports-betting` into `sports`.
    pub fn subtree_range(&self) -> (String, String) {
        let mut hi = self.0.clone();
        hi.pop();
        hi.push((PATH_SEP as u8 + 1) as char);
        (self.0.clone(), hi)
    }
}

impl fmt::Display for SpacePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_url())
    }
}

/// Distinct from [`crate::path::PathError`]: same technique, different tree.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SpacePathError {
    #[error(transparent)]
    Segment(NameError),
    #[error("space nesting is limited to {MAX_DEPTH} levels, got {0}")]
    TooDeep(usize),
    #[error("a space path must have at least one segment")]
    Empty,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::naming::assert_sorted;
    use proptest::prelude::*;

    fn key(s: &str) -> SpaceKey {
        SpaceKey::parse(s).expect("valid space key")
    }

    #[test]
    fn accepts_ordinary_names_and_folds_case_only() {
        assert_eq!(key("sports").as_str(), "sports");
        assert_eq!(key("ICE-HOCKEY").as_str(), "ice-hockey");
        // Case is the only folding: these stay two spaces.
        assert_ne!(key("ice-hockey"), key("icehockey"));
    }

    #[test]
    fn reserved_space_names_cannot_be_claimed() {
        assert_sorted(RESERVED, "space_key::RESERVED");
        for input in ["new", "edit", "search", "all", "rss", "notespace"] {
            assert!(
                matches!(SpaceKey::parse(input), Err(NameError::Reserved(_))),
                "accepted {input:?}"
            );
        }
        assert!(SpacePath::parse("sports/new").is_err());
    }

    #[test]
    fn builds_and_renders_paths() {
        let sports = SpacePath::root(&key("sports"));
        let hockey = sports.child(&key("hockey")).unwrap();
        let nhl = hockey.child(&key("nhl")).unwrap();

        assert_eq!(sports.as_stored(), "sports/");
        assert_eq!(hockey.as_stored(), "sports/hockey/");
        assert_eq!(nhl.as_url(), "sports/hockey/nhl");
        assert_eq!(nhl.url(), "/s/sports/hockey/nhl");
        assert_eq!(nhl.depth(), 3);
        assert_eq!(nhl.key(), "nhl");
        assert_eq!(nhl.parent().unwrap(), hockey);
        assert_eq!(sports.parent(), None);
        assert_eq!(nhl.child(&key("east")), Err(SpacePathError::TooDeep(4)));
    }

    #[test]
    fn parses_url_paths_in_both_forms() {
        let want = SpacePath::root(&key("sports"))
            .child(&key("hockey"))
            .unwrap();
        for input in [
            "sports/hockey",
            "sports/hockey/",
            "/sports/hockey",
            "//sports//hockey//",
        ] {
            assert_eq!(SpacePath::parse(input).unwrap(), want, "input {input:?}");
        }
        assert_eq!(SpacePath::parse(""), Err(SpacePathError::Empty));
        assert!(SpacePath::parse("sports/hockey/nhl/east").is_err());
        assert!(SpacePath::parse("sports/-bad").is_err());
    }

    #[test]
    fn same_key_under_different_parents_is_distinct() {
        let a = SpacePath::root(&key("sports"))
            .child(&key("general"))
            .unwrap();
        let b = SpacePath::root(&key("music"))
            .child(&key("general"))
            .unwrap();
        assert_ne!(a, b);
        assert_eq!(a.key(), b.key());
    }

    /// `-` is 0x2D and sorts below `/` (0x2F), so an untrailed range swallows `sports-betting`.
    #[test]
    fn subtree_range_does_not_swallow_siblings() {
        let (lo, hi) = SpacePath::root(&key("sports")).subtree_range();
        assert_eq!((lo.as_str(), hi.as_str()), ("sports/", "sports0"));

        for p in ["sports/", "sports/hockey/", "sports/hockey/nhl/"] {
            assert!(lo.as_str() <= p && p < hi.as_str(), "{p} should be inside");
        }
        for p in ["sports-betting/", "sportswear/", "music/", "sport/"] {
            assert!(!(lo.as_str() <= p && p < hi.as_str()), "{p} leaked in");
        }
        // Without the trailing separator the sibling leaks.
        assert!("sports" <= "sports-betting" && "sports-betting" < "sports0");
    }

    proptest! {
        #[test]
        fn paths_round_trip_and_contain_their_children(
            a in "[a-z]{2,8}", b in "[a-z]{2,8}", nest in any::<bool>(),
        ) {
            prop_assume!(!RESERVED.contains(&a.as_str()) && !RESERVED.contains(&b.as_str()));
            let mut p = SpacePath::root(&SpaceKey::parse(&a)?);
            if nest {
                p = p.child(&SpaceKey::parse(&b)?)?;
            }
            prop_assert_eq!(SpacePath::parse(p.as_url())?, p.clone());
            let (lo, hi) = p.subtree_range();
            prop_assert!(lo.as_str() <= p.as_stored() && p.as_stored() < hi.as_str());
            if let Ok(kid) = p.child(&SpaceKey::parse(&b)?) {
                prop_assert!(lo.as_str() <= kid.as_stored() && kid.as_stored() < hi.as_str());
            }
        }
    }
}
