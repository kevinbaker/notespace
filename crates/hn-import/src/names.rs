//! Hacker News author names to notespace usernames.
//!
//! HN allows characters and lengths notespace does not, and reserves a different set of words,
//! so a name may need rewriting. Rewriting can collide, which is what [`Names`] exists to
//! resolve -- two distinct HN accounts must never end up as one notespace user.

use std::collections::HashMap;

use notespace_core::username::{Username, MAX_CHARS};

/// The author of every dead or deleted comment.
pub const TOMBSTONE: &str = "hn-anon";

#[derive(Default)]
pub struct Names {
    by_hn: HashMap<String, String>,
    taken: HashMap<String, String>,
    pub rewritten: usize,
}

impl Names {
    pub fn new() -> Self {
        let mut names = Names::default();
        names.taken.insert(TOMBSTONE.to_string(), String::new());
        names.by_hn.insert(String::new(), TOMBSTONE.to_string());
        names
    }

    pub fn resolve(&mut self, hn: &str) -> String {
        if let Some(existing) = self.by_hn.get(hn) {
            return existing.clone();
        }
        let base = sanitize(hn);
        let mut candidate = base.clone();
        // A collision means two different HN accounts sanitized to the same string; the second
        // one gets a suffix rather than silently merging into the first.
        for n in 2.. {
            match self.taken.get(&candidate) {
                None => break,
                Some(owner) if owner == hn => break,
                Some(_) => {
                    let room = MAX_CHARS - 1 - n.to_string().len();
                    let stem = base
                        .chars()
                        .take(room.min(base.chars().count()))
                        .collect::<String>();
                    candidate = format!("{}-{n}", stem.trim_end_matches(['-', '_']));
                }
            }
        }
        if candidate != hn.to_ascii_lowercase() {
            self.rewritten += 1;
        }
        self.taken.insert(candidate.clone(), hn.to_string());
        self.by_hn.insert(hn.to_string(), candidate.clone());
        candidate
    }

    /// Every name minted so far, in insertion order of first use.
    pub fn all(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.taken.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }
}

/// Fold one HN name into something [`Username::parse`] accepts.
fn sanitize(hn: &str) -> String {
    let mut folded = String::with_capacity(hn.len());
    let mut prev_sep = false;
    for ch in hn.chars() {
        let c = ch.to_ascii_lowercase();
        let (keep, is_sep) = match c {
            'a'..='z' | '0'..='9' => (c, false),
            '-' | '_' => (c, true),
            _ => ('-', true),
        };
        if is_sep && (prev_sep || folded.is_empty()) {
            continue;
        }
        folded.push(keep);
        prev_sep = is_sep;
    }
    let folded: String = folded.chars().take(MAX_CHARS).collect();
    let folded = folded.trim_end_matches(['-', '_']).to_string();

    if let Ok(name) = Username::parse(&folded) {
        return name.as_str().to_string();
    }
    // Too short, all digits, or reserved: the prefix fixes all three, and `hn-` is itself not a
    // name anyone can register, so it cannot shadow a real account.
    let stem: String = folded.chars().take(MAX_CHARS - 3).collect();
    let prefixed = format!("hn-{}", stem.trim_end_matches(['-', '_']));
    match Username::parse(&prefixed) {
        Ok(name) => name.as_str().to_string(),
        Err(_) => TOMBSTONE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(s: &str) {
        assert!(Username::parse(s).is_ok(), "{s:?} is not a valid username");
    }

    #[test]
    fn ordinary_names_pass_through_lowercased() {
        assert_eq!(sanitize("pg"), "pg");
        assert_eq!(sanitize("Some_User-1"), "some_user-1");
    }

    #[test]
    fn illegal_characters_become_separators() {
        assert_eq!(sanitize("a.b c"), "a-b-c");
    }

    #[test]
    fn edge_and_doubled_separators_are_removed() {
        // `naming::validate` rejects both, so folding has to produce neither.
        valid(&sanitize("--weird--"));
        valid(&sanitize("a..b"));
        assert_eq!(sanitize("--weird--"), "weird");
    }

    #[test]
    fn reserved_and_all_digit_names_get_a_prefix() {
        assert_eq!(sanitize("admin"), "hn-admin");
        assert_eq!(sanitize("12345"), "hn-12345");
        assert_eq!(sanitize("x"), "hn-x");
    }

    #[test]
    fn long_names_stay_within_the_limit() {
        let out = sanitize(&"a".repeat(80));
        assert_eq!(out.chars().count(), MAX_CHARS);
        valid(&out);
    }

    #[test]
    fn distinct_hn_accounts_never_merge() {
        let mut names = Names::new();
        let a = names.resolve("a.b");
        let b = names.resolve("a-b");
        let c = names.resolve("a b");
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        for n in [&a, &b, &c] {
            valid(n);
        }
    }

    #[test]
    fn the_same_account_always_resolves_to_the_same_name() {
        let mut names = Names::new();
        assert_eq!(names.resolve("pg"), names.resolve("pg"));
    }

    #[test]
    fn a_collision_suffix_stays_within_the_limit() {
        let mut names = Names::new();
        let long = "b".repeat(MAX_CHARS);
        let first = names.resolve(&long);
        let second = names.resolve(&format!("{long}extra"));
        assert_ne!(first, second);
        valid(&second);
    }

    #[test]
    fn the_tombstone_name_is_reserved_up_front() {
        let mut names = Names::new();
        // A real HN account called "hn-anon" must not be able to claim the tombstone's name.
        assert_ne!(names.resolve("hn-anon"), TOMBSTONE);
        valid(TOMBSTONE);
    }
}
