//! Slugs and space paths — the *other* public identifier.
//!
//! DESIGN.md §7 addresses spaces and users by name rather than by opaque id: `/s/sports`,
//! `/u/testuser`. That is the right call for readability, but it buys a different set of
//! problems from [`crate::id::PublicId`], and they are worth stating plainly:
//!
//! - **A slug is mutable.** People rename. An id never changes; a slug does, so every rename
//!   strands existing links unless the old value keeps redirecting.
//! - **A slug is reusable.** Once `alice` is released, someone else can claim it — and inherit
//!   every link, mention and citation that pointed at the previous holder. For a *space* that is
//!   merely confusing; for a *user* it is an impersonation vector.
//! - **A slug is chosen, not generated.** So it can be chosen adversarially: `notespace`,
//!   `moderator`, or something that merely *looks* like an existing name.
//!
//! This module handles what can be handled in pure logic — the third problem, and the shape of
//! the first two. Retention and redirect policy is schema, and lives in `migrations/0003`.
//!
//! # Normalization is case, and only case
//!
//! [`Slug::parse`] lowercases and otherwise rejects rather than rewrites. Two names that differ
//! by anything more than case are two different names:
//!
//! ```text
//!   TestUser  testuser   ->  same name
//!   test-user testuser   ->  DIFFERENT names
//!   n0tespace notespace  ->  DIFFERENT names
//! ```
//!
//! An earlier draft folded separators and `0`/`o`, `1`/`l` into a "skeleton" and enforced
//! uniqueness on that. It is gone. Folding buys a little impersonation resistance and costs
//! real names — `ice-hockey` and `icehockey` become one space, and the person who wanted the
//! second one gets an error they cannot act on. Systems people know behave this way: on GitHub,
//! `foo-bar` and `foobar` are two accounts.
//!
//! What still holds the line:
//!
//! - **ASCII only.** This is the important one, and it is not a fold — it is a rejection. Every
//!   Cyrillic and Greek homoglyph attack dies here, and those are the ones that are genuinely
//!   invisible. `аdmin` with a Cyrillic а does not parse.
//! - **[`RESERVED`]**, for names that imply authority or collide with a route.
//! - Display-time signals — account age, a "new account" marker — which are where the remaining
//!   lookalike cases belong. A naming rule cannot tell `rn` from `m`; a UI can say "created
//!   today".
//!
//! Worth knowing that this direction is one-way. Once `testuser` and `test-user` both exist,
//! deciding later that they collide means renaming somebody. Loosening is easy; tightening is
//! not.
//!
//! # Space paths
//!
//! Space slugs are unique **per parent**, so `sports/hockey` and `music/hockey` can coexist and
//! the URL carries the whole path. [`SpacePath`] is the materialized form of that path, and it
//! exists for the same reason `post.path` does: one indexed lookup instead of one query per
//! level. Measured at depth 3 — 1.6 us and a single query, against 32.9 us and three queries for
//! the walk.
//!
//! **Stored paths carry a trailing separator.** That is not cosmetic. A slug may contain `-`
//! (0x2D), which sorts *below* `/` (0x2F), so the obvious subtree range over untrailed paths
//! silently swallows siblings:
//!
//! ```text
//!   range ['sports', 'sports0')      -> sports, sports-betting, sports/hockey   WRONG
//!   range ['sports/', 'sports0')     -> sports/, sports/hockey/                 right
//! ```
//!
//! With the trailing separator every descendant literally begins with the parent's stored path,
//! and `subtree_range_bug_would_swallow_siblings` keeps that pinned.

use core::fmt;

use serde::{Deserialize, Serialize};

/// Shortest slug. Two characters is enough for a real name (`ai`, `uk`) and long enough that
/// the namespace is not dominated by single letters.
pub const SLUG_MIN: usize = 2;

/// Longest slug. Bounds URL length and the size of the unique index.
pub const SLUG_MAX: usize = 32;

/// Separator between path segments, in both stored and displayed form.
pub const PATH_SEP: char = '/';

/// Deepest space nesting, counting the top level as depth 1.
///
/// Discourse allows two levels and is the most-used hierarchical forum; three leaves room
/// without producing URLs nobody can read. The cap also bounds stored path length to roughly
/// `3 * (SLUG_MAX + 1)` bytes, which keeps the unique index small.
pub const MAX_SPACE_DEPTH: usize = 3;

/// Names that may never be claimed.
///
/// Three groups: things that collide with a route, things that imply authority, and things that
/// read as a system state. Kept sorted — [`is_reserved`] binary-searches it.
pub const RESERVED: &[&str] = &[
    "about",
    "account",
    "admin",
    "administrator",
    "all",
    "anonymous",
    "api",
    "assets",
    "auth",
    "delete",
    "deleted",
    "edit",
    "everyone",
    "feed",
    "healthz",
    "help",
    "login",
    "logout",
    "me",
    "mod",
    "moderator",
    "modlog",
    "new",
    "notespace",
    "null",
    "official",
    "p",
    "password",
    "privacy",
    "register",
    "reset",
    "root",
    "rss",
    "s",
    "search",
    "security",
    "settings",
    "staff",
    "static",
    "support",
    "system",
    "t",
    "terms",
    "u",
    "undefined",
    "uploads",
];

/// Whether `s` is reserved.
///
/// This is the one place a folding still happens, and it is a different trade from the one the
/// module docs reject. Uniqueness between *users* must not fold, because a false collision
/// blocks a real name with no recourse. This list is forty-odd words nobody legitimately needs,
/// so blocking `adm1n` and `m0d3rator` alongside `admin` costs nothing and closes the way
/// reserved names actually get claimed.
///
/// If that still feels like too much, deleting the `leet_of` line leaves an exact-match check
/// and nothing else breaks.
pub fn is_reserved(s: &str) -> bool {
    let hit = |c: &str| RESERVED.binary_search(&c).is_ok();
    hit(s) || hit(&leet_of(s))
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SlugError {
    #[error("slug must be {SLUG_MIN}-{SLUG_MAX} characters, got {0}")]
    WrongLength(usize),
    #[error("character {0:?} is not allowed in a slug")]
    BadCharacter(char),
    #[error("a slug may not begin or end with {0:?}")]
    EdgeSeparator(char),
    #[error("a slug may not contain two separators in a row")]
    DoubleSeparator,
    #[error("{0:?} is reserved")]
    Reserved(String),
    #[error("a slug must contain at least one letter")]
    NoLetter,
    #[error("space nesting is limited to {MAX_SPACE_DEPTH} levels, got {0}")]
    TooDeep(usize),
    #[error("a space path must have at least one segment")]
    Empty,
}

/// A validated, normalized public name for a space or a user.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Slug(String);

impl Slug {
    /// Validate and normalize. Normalization is only ever case-folding: anything else is
    /// rejected rather than silently rewritten, so what someone typed is what they get or a
    /// clear error explaining why not.
    pub fn parse(input: &str) -> Result<Self, SlugError> {
        let n = input.chars().count();
        if !(SLUG_MIN..=SLUG_MAX).contains(&n) {
            return Err(SlugError::WrongLength(n));
        }

        let mut out = String::with_capacity(input.len());
        let mut prev_sep = false;
        let mut has_letter = false;
        for (i, ch) in input.chars().enumerate() {
            let c = ch.to_ascii_lowercase();
            let is_sep = c == '-' || c == '_';
            match c {
                'a'..='z' => has_letter = true,
                '0'..='9' => {}
                _ if is_sep => {
                    if i == 0 || i == n - 1 {
                        return Err(SlugError::EdgeSeparator(ch));
                    }
                    if prev_sep {
                        return Err(SlugError::DoubleSeparator);
                    }
                }
                _ => return Err(SlugError::BadCharacter(ch)),
            }
            prev_sep = is_sep;
            out.push(c);
        }

        // An all-digit slug is not a name; it also reads as an id in a URL.
        if !has_letter {
            return Err(SlugError::NoLetter);
        }
        if is_reserved(&out) {
            return Err(SlugError::Reserved(out));
        }
        Ok(Slug(out))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Leetspeak folding. Used by [`is_reserved`] and nowhere else — see the note there.
fn leet_of(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '-' && *c != '_')
        .map(|c| match c.to_ascii_lowercase() {
            '0' => 'o',
            '1' => 'i',
            '3' => 'e',
            '4' => 'a',
            '5' => 's',
            '7' => 't',
            '8' => 'b',
            other => other,
        })
        .collect()
}

impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Slug {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Slug::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// The materialized path of a space: its slug and every ancestor's, `/`-separated.
///
/// Stored **with a trailing separator** (`"sports/hockey/"`) so that subtree queries are a
/// single range scan that cannot swallow a sibling. Rendered **without** one, since that is
/// what belongs in a URL.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpacePath(String);

impl SpacePath {
    /// A top-level space.
    pub fn root(slug: &Slug) -> Self {
        SpacePath(format!("{slug}{PATH_SEP}"))
    }

    /// A child of this space. Fails past [`MAX_SPACE_DEPTH`].
    pub fn child(&self, slug: &Slug) -> Result<Self, SlugError> {
        let depth = self.depth() + 1;
        if depth > MAX_SPACE_DEPTH {
            return Err(SlugError::TooDeep(depth));
        }
        Ok(SpacePath(format!("{}{slug}{PATH_SEP}", self.0)))
    }

    /// Parse the path portion of a `/s/...` URL, with or without a trailing separator.
    pub fn parse(url_path: &str) -> Result<Self, SlugError> {
        let mut path: Option<SpacePath> = None;
        for seg in url_path.split(PATH_SEP).filter(|s| !s.is_empty()) {
            let slug = Slug::parse(seg)?;
            path = Some(match path {
                None => SpacePath::root(&slug),
                Some(p) => p.child(&slug)?,
            });
        }
        path.ok_or(SlugError::Empty)
    }

    /// Stored form, trailing separator included. This is what goes in the database.
    pub fn as_stored(&self) -> &str {
        &self.0
    }

    /// URL form, trailing separator stripped.
    pub fn as_url(&self) -> &str {
        self.0.strip_suffix(PATH_SEP).unwrap_or(&self.0)
    }

    pub fn segments(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.0.split(PATH_SEP).filter(|s| !s.is_empty())
    }

    pub fn depth(&self) -> usize {
        self.segments().count()
    }

    /// This space's own slug.
    pub fn slug(&self) -> &str {
        self.segments().next_back().unwrap_or("")
    }

    /// The parent path, or `None` for a top-level space.
    pub fn parent(&self) -> Option<SpacePath> {
        let trimmed = self.as_url();
        let cut = trimmed.rfind(PATH_SEP)?;
        Some(SpacePath(trimmed[..=cut].to_string()))
    }

    /// Half-open range `[lo, hi)` selecting this space and every descendant.
    ///
    /// The upper bound increments the trailing separator: `'/'` is 0x2F, so `'0'` (0x30) is the
    /// next code point and no path under this one can reach it. Because every stored path ends
    /// in the separator, a sibling like `sports-betting/` sorts *below* `sports/` and cannot
    /// leak in — which it does without the trailing separator.
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn slug(s: &str) -> Slug {
        Slug::parse(s).expect("valid slug")
    }

    #[test]
    fn accepts_ordinary_names_and_folds_case() {
        for (input, want) in [
            ("sports", "sports"),
            ("TestUser", "testuser"),
            ("ice-hockey", "ice-hockey"),
            ("test_user", "test_user"),
            ("web3", "web3"),
            ("ai", "ai"),
        ] {
            assert_eq!(slug(input).as_str(), want);
        }
    }

    #[test]
    fn rejects_malformed_names() {
        use SlugError::*;
        for (input, want) in [
            ("a", WrongLength(1)),
            ("", WrongLength(0)),
            (&"x".repeat(SLUG_MAX + 1), WrongLength(SLUG_MAX + 1)),
            ("-sports", EdgeSeparator('-')),
            ("sports-", EdgeSeparator('-')),
            ("_x_", EdgeSeparator('_')),
            ("ice--hockey", DoubleSeparator),
            ("ice-_hockey", DoubleSeparator),
            ("has space", BadCharacter(' ')),
            ("emoji🎉", BadCharacter('🎉')),
            ("slash/es", BadCharacter('/')),
            ("dot.ted", BadCharacter('.')),
            ("12345", NoLetter),
        ] {
            assert_eq!(Slug::parse(input), Err(want), "input {input:?}");
        }
    }

    /// Cyrillic homoglyphs are the classic impersonation trick. An ASCII-only charset kills
    /// the entire class before any lookalike analysis is needed.
    #[test]
    fn rejects_non_ascii_lookalikes() {
        // "аdmin" and "tеstuser" with Cyrillic а / е.
        for input in ["\u{0430}dmin", "t\u{0435}stuser", "n\u{043E}tespace"] {
            assert!(
                matches!(Slug::parse(input), Err(SlugError::BadCharacter(_))),
                "accepted {input:?}"
            );
        }
    }

    #[test]
    fn reserved_names_cannot_be_claimed() {
        for input in [
            "admin",
            "Admin",
            "moderator",
            "notespace",
            "system",
            "api",
            "modlog",
        ] {
            assert!(
                matches!(Slug::parse(input), Err(SlugError::Reserved(_))),
                "accepted {input:?}"
            );
        }
        // The single-character route prefixes are unreachable on length alone, which is why
        // /s/, /u/, /t/ and /p/ can never be shadowed by a space or a user.
        for input in ["s", "u", "t", "p"] {
            assert_eq!(Slug::parse(input), Err(SlugError::WrongLength(1)));
        }
        // ...including through both foldings, so digit substitution does not get around it.
        for input in [
            "m0derator",
            "n0tespace",
            "adm1n",
            "m0d3rator",
            "5ystem",
            "4dmin",
        ] {
            assert!(
                matches!(Slug::parse(input), Err(SlugError::Reserved(_))),
                "accepted {input:?}"
            );
        }
        assert!(
            RESERVED.windows(2).all(|w| w[0] < w[1]),
            "RESERVED must stay sorted"
        );
    }

    /// Case is folded; nothing else is. Uniqueness is plain equality on the parsed slug, so
    /// this is also the whole of the collision rule.
    #[test]
    fn only_case_is_folded() {
        assert_eq!(slug("TestUser"), slug("testuser"));
        assert_eq!(slug("ICE-HOCKEY"), slug("ice-hockey"));

        // Everything else stays distinct -- these are separate names, and both may be claimed.
        for (a, b) in [
            ("testuser", "test-user"),
            ("testuser", "test_user"),
            ("ice-hockey", "icehockey"),
            ("notespace-team", "n0tespace-team"),
            ("well", "we11"),
            ("web3", "webe"),
        ] {
            assert_ne!(slug(a), slug(b), "{a} and {b} must stay distinct");
        }
    }

    #[test]
    fn builds_and_renders_space_paths() {
        let sports = SpacePath::root(&slug("sports"));
        let hockey = sports.child(&slug("hockey")).unwrap();
        let nhl = hockey.child(&slug("nhl")).unwrap();

        assert_eq!(sports.as_stored(), "sports/");
        assert_eq!(hockey.as_stored(), "sports/hockey/");
        assert_eq!(nhl.as_url(), "sports/hockey/nhl");
        assert_eq!(nhl.to_string(), "sports/hockey/nhl");
        assert_eq!(nhl.depth(), 3);
        assert_eq!(nhl.slug(), "nhl");
        assert_eq!(nhl.parent().unwrap(), hockey);
        assert_eq!(sports.parent(), None);

        assert_eq!(nhl.child(&slug("east")), Err(SlugError::TooDeep(4)));
    }

    #[test]
    fn parses_url_paths_in_both_forms() {
        let want = SpacePath::root(&slug("sports"))
            .child(&slug("hockey"))
            .unwrap();
        for input in [
            "sports/hockey",
            "sports/hockey/",
            "/sports/hockey",
            "//sports//hockey//",
        ] {
            assert_eq!(SpacePath::parse(input).unwrap(), want, "input {input:?}");
        }
        assert_eq!(SpacePath::parse(""), Err(SlugError::Empty));
        assert!(SpacePath::parse("sports/hockey/nhl/east").is_err());
        assert!(SpacePath::parse("sports/-bad").is_err());
    }

    /// Per-parent uniqueness is the point of the path: the same slug under two parents is two
    /// different spaces, and they must not collide.
    #[test]
    fn same_slug_under_different_parents_is_distinct() {
        let a = SpacePath::root(&slug("sports"))
            .child(&slug("general"))
            .unwrap();
        let b = SpacePath::root(&slug("music"))
            .child(&slug("general"))
            .unwrap();
        assert_ne!(a, b);
        assert_eq!(a.slug(), b.slug());
    }

    /// The bug that motivated the trailing separator: `-` is 0x2D and sorts below `/` (0x2F),
    /// so an untrailed range over `['sports', 'sports0')` swallows `sports-betting`.
    #[test]
    fn subtree_range_bug_would_swallow_siblings() {
        let sports = SpacePath::root(&slug("sports"));
        let (lo, hi) = sports.subtree_range();
        assert_eq!((lo.as_str(), hi.as_str()), ("sports/", "sports0"));

        let inside = ["sports/", "sports/hockey/", "sports/hockey/nhl/"];
        let outside = ["sports-betting/", "sportswear/", "music/", "sport/"];
        for p in inside {
            assert!(
                lo.as_str() <= p && p < hi.as_str(),
                "{p} should be in the subtree"
            );
        }
        for p in outside {
            assert!(
                !(lo.as_str() <= p && p < hi.as_str()),
                "{p} leaked into the subtree"
            );
        }

        // Without the trailing separator the sibling does leak -- this is the failure mode.
        assert!("sports" <= "sports-betting" && "sports-betting" < "sports0");
    }

    proptest! {
        /// Anything that parses renders back and re-parses unchanged.
        #[test]
        fn slugs_round_trip(s in "[a-z][a-z0-9]{0,30}[a-z]") {
            prop_assume!(!is_reserved(&s));
            let parsed = Slug::parse(&s)?;
            prop_assert_eq!(parsed.as_str(), s.as_str());
            prop_assert_eq!(Slug::parse(parsed.as_str())?, parsed);
        }

        /// A path round-trips through its URL form, and its subtree range always contains it.
        #[test]
        fn paths_round_trip_and_contain_themselves(
            a in "[a-z]{2,8}", b in "[a-z]{2,8}", nest in any::<bool>(),
        ) {
            prop_assume!(!is_reserved(&a) && !is_reserved(&b));
            let mut p = SpacePath::root(&Slug::parse(&a)?);
            if nest {
                p = p.child(&Slug::parse(&b)?)?;
            }
            prop_assert_eq!(SpacePath::parse(p.as_url())?, p.clone());
            let (lo, hi) = p.subtree_range();
            prop_assert!(lo.as_str() <= p.as_stored() && p.as_stored() < hi.as_str());
            // And a child is always inside its parent's range.
            if let Ok(kid) = p.child(&Slug::parse(&b)?) {
                prop_assert!(lo.as_str() <= kid.as_stored() && kid.as_stored() < hi.as_str());
            }
        }
    }
}
