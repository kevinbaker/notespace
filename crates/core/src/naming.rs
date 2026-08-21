//! Shared validation for the two *identifying* names, and nothing else.
//!
//! # Three different things were all called "slug"
//!
//! They are not the same, and conflating them produced a type that was wrong for all three:
//!
//! | | resolves the thing? | mutable? | unique? |
//! |---|---|---|---|
//! | [`crate::username::Username`] | yes, `/u/testuser` | **never** | globally |
//! | [`crate::space_key::SpaceKey`] | yes, `/s/sports/hockey` | yes, with a redirect | per parent |
//! | thread title slug, `/t/{id}/{slug}` | **no** — decorative | freely | never |
//!
//! The third is not modelled here at all. `/t/{id}/anything-at-all` serves the same thread
//! because the id resolves it; the trailing text exists for readers and search engines. It needs
//! no type, because nothing reads it.
//!
//! The first two do resolve, so they need validating — but their *policies* differ enough that
//! they are separate types with separate reserved lists. What they share is the character rules
//! below, and only because duplicating a charset check is how the two drift apart.

/// Characters allowed in any identifying name, plus the length and reserved rules that vary.
pub(crate) struct Rules {
    /// Noun used in error messages: "username", "space name".
    pub kind: &'static str,
    pub min: usize,
    pub max: usize,
    /// Sorted. Checked against the input and against a leetspeak folding of it.
    pub reserved: &'static [&'static str],
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error("{kind} must be {min}-{max} characters, got {got}")]
    WrongLength {
        kind: &'static str,
        min: usize,
        max: usize,
        got: usize,
    },
    #[error("character {0:?} is not allowed in a {1}")]
    BadCharacter(char, &'static str),
    #[error("a {1} may not begin or end with {0:?}")]
    EdgeSeparator(char, &'static str),
    #[error("a {0} may not contain two separators in a row")]
    DoubleSeparator(&'static str),
    #[error("{0:?} is reserved")]
    Reserved(String),
    #[error("a {0} must contain at least one letter")]
    NoLetter(&'static str),
}

/// Validate and normalize. Normalization is **case, and only case** — anything else is rejected
/// rather than silently rewritten, so what someone typed is what they get or a clear error.
///
/// `test-user` and `testuser` are therefore two different names, as they are on GitHub. An
/// earlier draft folded separators and `0`/`o` into a "skeleton" and enforced uniqueness on
/// that; it cost real names (`ice-hockey` and `icehockey` collapsed into one) for a little
/// impersonation resistance, and is gone.
///
/// The defence that remains is the ASCII restriction, which is a rejection rather than a fold
/// and kills every Cyrillic and Greek homoglyph outright — those being the invisible ones.
pub(crate) fn validate(input: &str, rules: &Rules) -> Result<String, NameError> {
    let n = input.chars().count();
    if !(rules.min..=rules.max).contains(&n) {
        return Err(NameError::WrongLength {
            kind: rules.kind,
            min: rules.min,
            max: rules.max,
            got: n,
        });
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
                    return Err(NameError::EdgeSeparator(ch, rules.kind));
                }
                if prev_sep {
                    return Err(NameError::DoubleSeparator(rules.kind));
                }
            }
            _ => return Err(NameError::BadCharacter(ch, rules.kind)),
        }
        prev_sep = is_sep;
        out.push(c);
    }

    // An all-digit name is not a name, and reads as an id in a URL.
    if !has_letter {
        return Err(NameError::NoLetter(rules.kind));
    }
    if is_reserved(&out, rules.reserved) {
        return Err(NameError::Reserved(out));
    }
    Ok(out)
}

/// Whether `s` is on `list`, directly or through a leetspeak reading of it.
///
/// This is the one place a folding still happens, and it is a different trade from the one
/// [`validate`] rejects. Uniqueness between *users* must not fold, because a false collision
/// blocks a real name with no recourse. A reserved list is a few dozen words nobody legitimately
/// needs, so blocking `adm1n` alongside `admin` costs nothing and closes the way reserved names
/// actually get claimed.
pub(crate) fn is_reserved(s: &str, list: &[&str]) -> bool {
    list.binary_search(&s).is_ok() || list.binary_search(&leet(s).as_str()).is_ok()
}

fn leet(s: &str) -> String {
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

#[cfg(test)]
pub(crate) fn assert_sorted(list: &[&str], what: &str) {
    assert!(
        list.windows(2).all(|w| w[0] < w[1]),
        "{what} must stay sorted for binary_search"
    );
}
