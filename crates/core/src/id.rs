//! Public ids — opaque, time-sortable, and safe to read aloud or type.
//!
//! DESIGN.md §4.2: ids are two-tier. `INTEGER PRIMARY KEY` stays the internal identity and
//! carries every foreign key; this type is what appears in URLs.
//!
//! # Layout
//!
//! 80 bits, rendered as 16 Crockford base32 characters:
//!
//! ```text
//!   48 bits          32 bits
//! ┌───────────────┬─────────────┐
//! │ unix ms       │ random      │
//! └───────────────┴─────────────┘
//!   0dwjnnyq         tsdkr5wm       ->  "0dwjnnyqtsdkr5wm"
//! ```
//!
//! This is a ULID truncated from 26 characters to 16: same 48-bit millisecond prefix, 32 bits
//! of randomness instead of 80. M0 measured why the prefix matters — a *time-sortable* text id
//! costs the same as an integer to look up (2 ms per 5000), while a random one costs double,
//! because random keys scatter across the index instead of appending at its right edge.
//!
//! # Why 32 random bits is enough
//!
//! Two ids can only collide if they share a millisecond, so the birthday bound applies
//! per-millisecond, not over the database's lifetime. At 32 bits that is ~65,536 ids in the
//! same millisecond for a 50% chance — six orders of magnitude beyond what a forum generates.
//! For ten threads in one millisecond the probability is about 1 in 10^8.
//!
//! Collisions are also *caught, not silent*: `public_id` carries a UNIQUE index, so a
//! collision is a failed insert to retry with fresh randomness, never a wrong row.
//!
//! # Typing and reading aloud
//!
//! The alphabet omits `I`, `L`, `O` and `U`. The first three are visually confusable with `1`
//! and `0`; `U` is dropped so that no id accidentally spells something unfortunate. On the way
//! in, [`PublicId::parse`] is deliberately forgiving in the ways humans actually get it wrong:
//!
//! - either case (canonical output is lowercase)
//! - `I`/`l` read back as `1`, `O` as `0`
//! - hyphens ignored, so `0dwj-nnyq-tsdk-r5wm` parses
//!
//! It is *not* forgiving about anything else: an unknown character is an error, not a guess.
//!
//! # If 32 random bits ever stops being enough
//!
//! Widening this id later is possible, but **only by appending**, and the distinction is not
//! obvious. A standard 26-character ULID is *not* a drop-in successor: ULID packs 128 bits into
//! 130 bits of base32 space, so it carries two leading padding bits, and that offset shifts
//! every character boundary. Ids in the two formats share no prefix even for the same
//! millisecond, and sorting a mixed set no longer gives creation order:
//!
//! ```text
//! same timestamp, three encodings
//!   ours (16)         06a1yabw00000000
//!   ULID (26)         01jgfjjz000000000000000000   <- 1 char in common. different alignment.
//!   appended (26)     06a1yabw000000000000000000   <- all 16 in common. same alignment.
//! ```
//!
//! The safe widening keeps these 80 bits exactly where they are and appends 50 more, giving
//! 130 bits in 26 characters, of which the first 16 are byte-identical to the id this type
//! produces today. Old and new then interleave correctly in one index, old URLs keep resolving,
//! and no backfill is needed. `extension_by_appending_preserves_order` below is the executable
//! form of that claim.
//!
//! Worth knowing, though: nothing in notespace actually *needs* cross-format sort order.
//! Feeds order by `created_at`/`bumped_at`, never by id. What the time prefix buys is index
//! insert locality, and both formats keep that, because both put the same timestamp in the
//! high bits. A hard switch to standard ULID would therefore still work operationally — it
//! would only forfeit a property nothing currently reads.

use core::fmt;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

/// Canonical alphabet, lowercase, in ascending ASCII order.
///
/// Ordering depends on that: `'0'..='9'` is `0x30..=0x39` and `'a'..='z'` is `0x61..=0x7A`, so
/// bytewise string comparison equals numeric comparison, and SQLite's default BINARY collation
/// sorts ids by creation time for free.
pub const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Characters in a rendered id.
pub const ID_CHARS: usize = 16;

/// Bits of millisecond timestamp. 48 bits runs to the year 10889.
pub const TIMESTAMP_BITS: u32 = 48;

/// Bits of randomness.
pub const RANDOM_BITS: u32 = 32;

/// Largest representable timestamp, in unix milliseconds.
pub const MAX_TIMESTAMP_MS: u64 = (1 << TIMESTAMP_BITS) - 1;

const INVALID: u8 = 0xFF;
const SKIP: u8 = 0xFE;

/// Byte -> digit value. Built at compile time; see the parsing notes in the module docs.
const DECODE: [u8; 256] = {
    let mut t = [INVALID; 256];
    let mut i = 0;
    while i < ALPHABET.len() {
        let c = ALPHABET[i];
        t[c as usize] = i as u8;
        // Uppercase accepted on input, never produced on output.
        if c >= b'a' && c <= b'z' {
            t[(c - 32) as usize] = i as u8;
        }
        i += 1;
    }
    // Transcription rescues, per the Crockford base32 spec.
    t[b'i' as usize] = 1;
    t[b'I' as usize] = 1;
    t[b'l' as usize] = 1;
    t[b'L' as usize] = 1;
    t[b'o' as usize] = 0;
    t[b'O' as usize] = 0;
    // Grouping hyphens are ignored so a chunked id can be typed back in.
    t[b'-' as usize] = SKIP;
    t
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("timestamp {0} ms exceeds the {TIMESTAMP_BITS}-bit range")]
    TimestampOutOfRange(u64),
    #[error("id must be {ID_CHARS} characters, got {0}")]
    WrongLength(usize),
    #[error("character {0:?} is not in the id alphabet")]
    BadCharacter(char),
}

/// An opaque, time-sortable public identifier.
///
/// `Ord` is numeric on the underlying 80 bits, which is the same order as the rendered string,
/// which is the same order as creation time. All three agree by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicId(u128);

impl PublicId {
    /// Build an id from a clock reading and randomness.
    ///
    /// Both are parameters rather than fetched here, because `core` takes no I/O and neither
    /// `std::time::SystemTime` nor a system RNG exists on wasm (DESIGN.md §3.2, §9). The
    /// Worker supplies `Date.now()` and `crypto.getRandomValues`; tests supply fixed values.
    pub fn new(timestamp_ms: u64, random: u32) -> Result<Self, IdError> {
        if timestamp_ms > MAX_TIMESTAMP_MS {
            return Err(IdError::TimestampOutOfRange(timestamp_ms));
        }
        Ok(PublicId(
            ((timestamp_ms as u128) << RANDOM_BITS) | random as u128,
        ))
    }

    /// Parse a rendered id. Accepts either case, `I`/`L`/`O` confusions, and grouping hyphens.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let mut bits: u128 = 0;
        let mut digits = 0usize;
        for ch in s.chars() {
            // Non-ASCII cannot be in the alphabet, and indexing DECODE by a multi-byte char
            // would be wrong, so reject it before the table lookup.
            if !ch.is_ascii() {
                return Err(IdError::BadCharacter(ch));
            }
            match DECODE[ch as usize] {
                SKIP => continue,
                INVALID => return Err(IdError::BadCharacter(ch)),
                digit => {
                    digits += 1;
                    if digits > ID_CHARS {
                        return Err(IdError::WrongLength(digits));
                    }
                    bits = (bits << 5) | digit as u128;
                }
            }
        }
        if digits != ID_CHARS {
            return Err(IdError::WrongLength(digits));
        }
        Ok(PublicId(bits))
    }

    /// Creation time, in unix milliseconds.
    pub fn timestamp_ms(&self) -> u64 {
        (self.0 >> RANDOM_BITS) as u64
    }

    /// The random component. Exposed for tests and diagnostics.
    pub fn random(&self) -> u32 {
        (self.0 & u32::MAX as u128) as u32
    }

    /// Canonical rendering: 16 lowercase characters, no hyphens.
    pub fn encode(&self) -> String {
        let mut buf = [b'0'; ID_CHARS];
        let mut n = self.0;
        for slot in buf.iter_mut().rev() {
            slot.clone_from(&ALPHABET[(n & 31) as usize]);
            n >>= 5;
        }
        // Every ALPHABET byte is ASCII, so `as char` is exact and no fallible UTF-8 step is
        // needed (DESIGN.md §9: no unwrap outside tests).
        let mut out = String::with_capacity(ID_CHARS);
        for b in buf {
            out.push(b as char);
        }
        out
    }

    /// Rendering grouped into fours, for display where someone may read or type it back.
    /// Parses again unchanged; the hyphens are ignored on input.
    pub fn encode_grouped(&self) -> String {
        let raw = self.encode();
        let mut out = String::with_capacity(ID_CHARS + 3);
        for (i, c) in raw.chars().enumerate() {
            if i > 0 && i % 4 == 0 {
                out.push('-');
            }
            out.push(c);
        }
        out
    }
}

impl fmt::Display for PublicId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

impl Serialize for PublicId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.encode())
    }
}

/// Validates on the way in, like `Path`: an id that skipped validation could not round-trip.
impl<'de> Deserialize<'de> for PublicId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        PublicId::parse(&s).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const T: u64 = 1_735_689_600_000; // 2025-01-01T00:00:00Z

    #[test]
    fn alphabet_is_ascending_lowercase_and_unambiguous() {
        // The ordering guarantee rests on this, so assert it rather than trusting the literal.
        for w in ALPHABET.windows(2) {
            assert!(w[0] < w[1], "alphabet not ascending at {w:?}");
        }
        assert_eq!(ALPHABET.len(), 32);
        for c in [b'i', b'l', b'o', b'u'] {
            assert!(
                !ALPHABET.contains(&c),
                "ambiguous {} in alphabet",
                c as char
            );
        }
        assert!(ALPHABET.iter().all(|c| !c.is_ascii_uppercase()));
    }

    #[test]
    fn renders_sixteen_lowercase_chars() {
        let id = PublicId::new(T, 0x1234_5678).unwrap();
        let s = id.encode();
        assert_eq!(s.len(), ID_CHARS);
        assert_eq!(s, s.to_lowercase(), "canonical form must be lowercase");
        assert!(s.chars().all(|c| ALPHABET.contains(&(c as u8))));
    }

    #[test]
    fn round_trips_through_string() {
        for (ms, rand) in [(0, 0), (T, 0), (T, u32::MAX), (MAX_TIMESTAMP_MS, 12345)] {
            let id = PublicId::new(ms, rand).unwrap();
            assert_eq!(PublicId::parse(&id.encode()), Ok(id));
            assert_eq!(id.timestamp_ms(), ms);
            assert_eq!(id.random(), rand);
        }
    }

    #[test]
    fn timestamp_is_recoverable() {
        let id = PublicId::new(T, 999).unwrap();
        assert_eq!(id.timestamp_ms(), T);
    }

    #[test]
    fn rejects_out_of_range_timestamp() {
        assert_eq!(
            PublicId::new(MAX_TIMESTAMP_MS + 1, 0),
            Err(IdError::TimestampOutOfRange(MAX_TIMESTAMP_MS + 1))
        );
    }

    // --- the transcription affordances, which are the point of Crockford base32 ---

    #[test]
    fn accepts_uppercase() {
        let id = PublicId::new(T, 0xDEAD_BEEF).unwrap();
        let s = id.encode();
        assert_eq!(PublicId::parse(&s.to_uppercase()), Ok(id));
    }

    #[test]
    fn rescues_confusable_characters() {
        let id = PublicId::new(T, 0xABCD_1234).unwrap();
        let canonical = id.encode();
        // Someone typing the id back may render 1 as I or l, and 0 as O.
        let typed = canonical.replace('1', "I").replace('0', "O");
        assert_eq!(PublicId::parse(&typed), Ok(id), "canonical was {canonical}");
        let typed_lower = canonical.replace('1', "l").replace('0', "o");
        assert_eq!(PublicId::parse(&typed_lower), Ok(id));
    }

    #[test]
    fn ignores_grouping_hyphens() {
        let id = PublicId::new(T, 0x5555_AAAA).unwrap();
        let grouped = id.encode_grouped();
        assert_eq!(grouped.len(), ID_CHARS + 3);
        assert_eq!(grouped.matches('-').count(), 3);
        assert_eq!(PublicId::parse(&grouped), Ok(id));
    }

    #[test]
    fn rejects_genuinely_bad_input() {
        assert!(matches!(
            PublicId::parse("u000000000000000"),
            Err(IdError::BadCharacter('u'))
        ));
        assert!(matches!(
            PublicId::parse("0000000000000 00"),
            Err(IdError::BadCharacter(' '))
        ));
        assert!(matches!(
            PublicId::parse("0000000000000!00"),
            Err(IdError::BadCharacter('!'))
        ));
        assert!(matches!(
            PublicId::parse("00000000000000é0"),
            Err(IdError::BadCharacter('é'))
        ));
        assert!(matches!(
            PublicId::parse("abc"),
            Err(IdError::WrongLength(3))
        ));
        assert!(matches!(PublicId::parse(""), Err(IdError::WrongLength(0))));
        assert!(PublicId::parse("00000000000000000").is_err(), "17 chars");
    }

    #[test]
    fn a_typo_that_lands_on_a_valid_id_is_simply_a_different_id() {
        // No check digit: a wrong-but-well-formed id must resolve to a different id (and so
        // a 404), never silently to the intended one.
        let a = PublicId::new(T, 1).unwrap();
        let b = PublicId::new(T, 2).unwrap();
        assert_ne!(a, b);
        assert_ne!(a.encode(), b.encode());
    }

    // --- ordering: the property the index locality measurement depends on ---

    #[test]
    fn sorts_by_creation_time() {
        let ids: Vec<String> = (0..2000)
            .map(|i| PublicId::new(T + i, (i as u32).wrapping_mul(2_654_435_761)).unwrap())
            .map(|id| id.encode())
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "lexicographic order must equal creation order");
    }

    #[test]
    fn same_millisecond_ids_stay_adjacent() {
        // Ids from one millisecond share a 48-bit prefix, so they cluster in the index
        // instead of scattering. That clustering is why this costs what an integer costs.
        let a = PublicId::new(T, 0).unwrap().encode();
        let b = PublicId::new(T, u32::MAX).unwrap().encode();
        let common = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
        assert!(
            common >= 9,
            "only {common} chars of shared prefix: {a} vs {b}"
        );
    }

    /// The documented widening path: appending bits keeps the old id as a literal prefix, so
    /// old and new ids sort together correctly. This is what makes a future format change a
    /// migration rather than a rewrite.
    #[test]
    fn extension_by_appending_preserves_order() {
        // A hypothetical 130-bit successor: the same [48 ts][32 rand] head, plus 50 bits.
        fn appended(ms: u64, rand: u32, extra: u64) -> String {
            let head = ((ms as u128) << RANDOM_BITS) | rand as u128;
            let bits = (head << 50) | (extra as u128 & ((1 << 50) - 1));
            let mut buf = [b'0'; 26];
            let mut n = bits;
            for slot in buf.iter_mut().rev() {
                slot.clone_from(&ALPHABET[(n & 31) as usize]);
                n >>= 5;
            }
            buf.iter().map(|&b| b as char).collect()
        }

        // The old id is a literal prefix of its widened form.
        let old = PublicId::new(T, 0xDEAD_BEEF).unwrap().encode();
        let new = appended(T, 0xDEAD_BEEF, 0x3_FFFF_FFFF_FFFF);
        assert!(new.starts_with(&old), "{new} should start with {old}");

        // Mixing formats across a cutover still sorts by creation time.
        let mut mixed: Vec<(u64, String)> = Vec::new();
        for i in 0..200u64 {
            mixed.push((
                T + i,
                PublicId::new(T + i, i as u32 * 7919).unwrap().encode(),
            ));
        }
        for i in 200..400u64 {
            mixed.push((T + i, appended(T + i, i as u32 * 7919, i * 104_729)));
        }
        let by_time: Vec<&String> = {
            let mut v: Vec<&(u64, String)> = mixed.iter().collect();
            v.sort_by_key(|(t, _)| *t);
            v.into_iter().map(|(_, s)| s).collect()
        };
        let by_string: Vec<&String> = {
            let mut v: Vec<&String> = mixed.iter().map(|(_, s)| s).collect();
            v.sort();
            v
        };
        assert_eq!(
            by_time, by_string,
            "mixed-format sort must equal creation order"
        );
    }

    proptest! {
        #[test]
        fn parse_encode_round_trips(ms in 0u64..=MAX_TIMESTAMP_MS, r in any::<u32>()) {
            let id = PublicId::new(ms, r)?;
            prop_assert_eq!(PublicId::parse(&id.encode())?, id);
            prop_assert_eq!(PublicId::parse(&id.encode_grouped())?, id);
            prop_assert_eq!(PublicId::parse(&id.encode().to_uppercase())?, id);
        }

        /// String order, numeric order and (timestamp, random) order all agree.
        #[test]
        fn string_order_matches_time_order(
            a_ms in 0u64..=MAX_TIMESTAMP_MS, a_r in any::<u32>(),
            b_ms in 0u64..=MAX_TIMESTAMP_MS, b_r in any::<u32>(),
        ) {
            let a = PublicId::new(a_ms, a_r)?;
            let b = PublicId::new(b_ms, b_r)?;
            prop_assert_eq!(a.encode().cmp(&b.encode()), (a_ms, a_r).cmp(&(b_ms, b_r)));
            prop_assert_eq!(a.cmp(&b), (a_ms, a_r).cmp(&(b_ms, b_r)));
        }

        /// Every rendered id is exactly 16 characters from the alphabet.
        #[test]
        fn encoding_is_well_formed(ms in 0u64..=MAX_TIMESTAMP_MS, r in any::<u32>()) {
            let s = PublicId::new(ms, r)?.encode();
            prop_assert_eq!(s.len(), ID_CHARS);
            prop_assert!(s.bytes().all(|b| ALPHABET.contains(&b)));
        }
    }
}
