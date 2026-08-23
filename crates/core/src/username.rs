//! Usernames — permanent, global, and never reassigned. `/u/testuser` resolves the account, so
//! unlike a thread's decorative slug the name is load-bearing.
//!
//! There is no rename operation. A reusable name is an impersonation vector: every old link,
//! quote and `@mention` naming `alice` would start pointing at whoever claimed it next, and a
//! forum's four-year-old threads are still cited. Deleting an account leaves a tombstone row
//! rather than freeing the name.
//!
//! [`RESERVED`] is not the space list: it guards authority words and `/u/` sub-routes.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::naming::{self, NameError, Rules};

pub const MIN_CHARS: usize = 2;
pub const MAX_CHARS: usize = 24;

/// Names no account may claim, kept sorted for binary search.
pub const RESERVED: &[&str] = &[
    "about",
    "admin",
    "administrator",
    "anonymous",
    "api",
    "deleted",
    "everyone",
    "ghost",
    "guest",
    "help",
    "me",
    "mod",
    "moderator",
    "notespace",
    "null",
    "official",
    "root",
    "security",
    "settings",
    "staff",
    "support",
    "system",
    "undefined",
    "unknown",
];

const RULES: Rules = Rules {
    kind: "username",
    min: MIN_CHARS,
    max: MAX_CHARS,
    reserved: RESERVED,
};

/// A validated, lowercase username. Permanent for the life of the instance.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Username(String);

impl Username {
    pub fn parse(input: &str) -> Result<Self, NameError> {
        naming::validate(input, &RULES).map(Username)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The canonical URL for this account.
    pub fn url(&self) -> String {
        format!("/u/{}", self.0)
    }
}

impl fmt::Display for Username {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Username {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Username::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::naming::assert_sorted;

    #[test]
    fn accepts_ordinary_names_and_folds_case_only() {
        for (input, want) in [
            ("testuser", "testuser"),
            ("TestUser", "testuser"),
            ("test_user", "test_user"),
            ("test-user", "test-user"),
            ("kb", "kb"),
        ] {
            assert_eq!(Username::parse(input).unwrap().as_str(), want);
        }
        // Case is the only folding: these stay three separate accounts.
        assert_ne!(
            Username::parse("test-user").unwrap(),
            Username::parse("testuser").unwrap()
        );
        assert_ne!(
            Username::parse("test_user").unwrap(),
            Username::parse("testuser").unwrap()
        );
    }

    #[test]
    fn rejects_malformed_names() {
        for input in [
            "a",
            "",
            &"x".repeat(MAX_CHARS + 1),
            "-lead",
            "trail-",
            "double--sep",
            "has space",
            "emoji🎉",
            "slash/es",
            "12345",
        ] {
            assert!(Username::parse(input).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn rejects_non_ascii_lookalikes() {
        for input in ["\u{0430}dmin", "t\u{0435}stuser", "n\u{043E}tespace"] {
            assert!(
                matches!(Username::parse(input), Err(NameError::BadCharacter(..))),
                "accepted {input:?}"
            );
        }
    }

    #[test]
    fn reserved_usernames_cannot_be_claimed() {
        assert_sorted(RESERVED, "username::RESERVED");
        for input in ["admin", "Admin", "moderator", "system", "notespace", "me"] {
            assert!(
                matches!(Username::parse(input), Err(NameError::Reserved(_))),
                "accepted {input:?}"
            );
        }
        for input in ["adm1n", "m0d3rator", "5ystem"] {
            assert!(
                matches!(Username::parse(input), Err(NameError::Reserved(_))),
                "accepted {input:?}"
            );
        }
    }

    #[test]
    fn reserved_list_differs_from_the_space_list() {
        use crate::space_key::RESERVED as SPACE;
        // Fine as a username, reserved as a space name: these are `/s/` sub-routes.
        for w in ["new", "search", "all"] {
            assert!(Username::parse(w).is_ok(), "{w} should be a valid username");
            assert!(SPACE.contains(&w), "{w} should be reserved for spaces");
        }
        // Reserved as a username, fine as a space name: authority words.
        for w in ["moderator", "staff", "support"] {
            assert!(
                RESERVED.contains(&w),
                "{w} should be reserved for usernames"
            );
            assert!(!SPACE.contains(&w), "{w} need not be reserved for spaces");
        }
    }

    #[test]
    fn renders_its_url() {
        assert_eq!(Username::parse("testuser").unwrap().url(), "/u/testuser");
    }
}
