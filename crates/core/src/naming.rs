//! Shared character rules for the two names that resolve something:
//!
//! | | resolves the thing? | mutable? | unique? |
//! |---|---|---|---|
//! | [`crate::username::Username`] | yes, `/u/testuser` | **never** | globally |
//! | [`crate::space_key::SpaceKey`] | yes, `/s/sports/hockey` | yes, with a redirect | per parent |
//! | thread title slug, `/t/{id}/{slug}` | **no** — decorative | freely | never |
//!
//! The third is not modelled: `/t/{id}/anything-at-all` serves the same thread. The first two
//! have policies different enough to be separate types; only the charset check is shared.

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

/// Normalization is case and only case, so `test-user` and `testuser` are two names. The
/// homoglyph defence is the ASCII restriction, which rejects rather than folds.
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

    // An all-digit name reads as an id in a URL.
    if !has_letter {
        return Err(NameError::NoLetter(rules.kind));
    }
    if is_reserved(&out, rules.reserved) {
        return Err(NameError::Reserved(out));
    }
    Ok(out)
}

/// The one place a folding happens: a false collision against a few dozen reserved words costs
/// nothing, whereas folding for user-to-user uniqueness would block real names.
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
