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
//! The id widens while staying base32, provided one rule holds: **keep the 48-bit timestamp in
//! the top bits and append whole characters at the bottom — never re-align the payload.** Every
//! shorter id is then a literal prefix of its wider form, and because prefix order is time
//! order, mixed widths sort correctly with no special handling.
//!
//! ```text
//! same timestamp, three widths
//!   16 chars ( 80 bits, 32 random)   06a1yabw00000000
//!   20 chars (100 bits, 52 random)   06a1yabw000000000000
//!   25 chars (125 bits, 77 random)   06a1yabw00000000000000000
//! ```
//!
//! 25 characters is the ceiling, set by [`PublicId::to_u128`]: 125 bits fits a `u128`, so every
//! id has an exact integer form. 26 would be 130 bits and would make that form fallible.
//!
//! [`PublicId::parse`] already accepts any width in [`MIN_CHARS`]`..=`[`MAX_CHARS`], while
//! [`PublicId::new`] only ever generates [`ID_CHARS`]. That asymmetry is the point: a future
//! instance can widen what it generates without touching this parser and without stranding a
//! single existing URL.
//!
//! The one thing to avoid is adopting a *canonical* 26-character ULID. ULID packs 128 bits into
//! 130 bits of base32 space, so it carries two leading padding bits, and that offset shifts every
//! character boundary — the formats then share no prefix even for the same millisecond:
//!
//! ```text
//!   ours     (16)   06a1yabw00000000
//!   ULID     (26)   01jgfjjz000000000000000000   <- 1 char in common; re-aligned
//!   appended (25)   06a1yabw00000000000000000    <- all 16 in common; safe
//! ```
//!
//! That is an alignment difference, not an alphabet one; both are base32. Widening this format
//! is safe, swapping in someone else's 128-bit layout is not.
//! `mixed_width_ids_sort_by_creation_time` keeps the safe path tested rather than assumed.
//!

use core::fmt;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

/// Canonical alphabet, lowercase, in ascending ASCII order.
///
/// Ordering depends on that: `'0'..='9'` is `0x30..=0x39` and `'a'..='z'` is `0x61..=0x7A`, so
/// bytewise string comparison equals numeric comparison, and SQLite's default BINARY collation
/// sorts ids by creation time for free.
pub const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Characters in an id at the default width.
pub const ID_CHARS: usize = 16;

/// Shortest accepted id. Below this, 48 bits of timestamp leaves too little randomness.
pub const MIN_CHARS: usize = 16;

/// Longest accepted id: 25 characters, 125 bits.
///
/// The cap is set by [`PublicId::to_u128`]. 26 characters would be 130 bits and would make the
/// integer form fallible or lossy; 25 fits a `u128` with room to spare, so *every* id has an
/// exact integer representation. What that gives up is three bits of randomness against a
/// ULID's 80 — not a difference any forum can observe.
///
/// Ids are *parsed* anywhere in `MIN_CHARS..=MAX_CHARS` even though this build only ever
/// *generates* [`ID_CHARS`]. That asymmetry is deliberate: it means widening the generated id
/// later (§4.3) needs no change here and cannot strand an existing URL.
pub const MAX_CHARS: usize = 25;

/// Bits in the widest id. Comfortably inside a `u128`.
pub const MAX_BITS: u32 = (MAX_CHARS as u32) * 5;

/// Bits of millisecond timestamp. 48 bits runs to the year 10889.
pub const TIMESTAMP_BITS: u32 = 48;

/// Bits of randomness at the default width. Widening adds 5 more per extra character.
pub const RANDOM_BITS: u32 = (ID_CHARS as u32 * 5) - TIMESTAMP_BITS;

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
    #[error("id must be {MIN_CHARS}-{MAX_CHARS} characters, got {0}")]
    WrongLength(usize),
    #[error("character {0:?} is not in the id alphabet")]
    BadCharacter(char),
    #[error("value {0} does not fit the requested id width")]
    ValueTooWide(u128),
}

/// An opaque, time-sortable public identifier.
///
/// Stored as its canonical rendering rather than as packed integers, so that `Ord` is plain
/// string comparison. That matters once more than one width is in play: a 16-character id is a
/// literal prefix of its widened form, and prefix-order *is* time-order, so mixed-width ids sort
/// correctly with no special handling. Packing into an integer would need a width-aware
/// comparison and could not hold 130 bits in a `u128` anyway.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicId(String);

impl PublicId {
    /// Build an id at the default width.
    ///
    /// Both inputs are parameters rather than fetched here, because `core` takes no I/O and
    /// neither `std::time::SystemTime` nor a system RNG exists on wasm (DESIGN.md §3.2, §9).
    /// The Worker supplies `Date.now()` and `crypto.getRandomValues`; tests supply fixed values.
    pub fn new(timestamp_ms: u64, random: u32) -> Result<Self, IdError> {
        Self::with_width(timestamp_ms, random as u128, ID_CHARS)
    }

    /// Build an id at an explicit width, for a future instance configured to generate wider
    /// ids (DESIGN.md §4.3). Extra bits are appended below the existing ones, so an id built
    /// here is prefix-compatible with one built by [`PublicId::new`].
    pub fn with_width(timestamp_ms: u64, random: u128, chars: usize) -> Result<Self, IdError> {
        if timestamp_ms > MAX_TIMESTAMP_MS {
            return Err(IdError::TimestampOutOfRange(timestamp_ms));
        }
        let random_bits = Self::random_bits(chars)?;
        let mask = (1u128 << random_bits) - 1;
        // Always fits: chars <= MAX_CHARS means at most MAX_BITS = 125 bits.
        let bits = ((timestamp_ms as u128) << random_bits) | (random & mask);
        Ok(PublicId(Self::render(bits, chars)))
    }

    /// Rebuild an id from its integer form, e.g. after storing it as a number elsewhere.
    ///
    /// `bits` must fit the width: `chars * 5` bits.
    pub fn from_u128(bits: u128, chars: usize) -> Result<Self, IdError> {
        let _ = Self::random_bits(chars)?;
        let total = chars as u32 * 5;
        if total < 128 && bits >= (1u128 << total) {
            return Err(IdError::ValueTooWide(bits));
        }
        Ok(PublicId(Self::render(bits, chars)))
    }

    /// The id as a packed integer: 48 bits of timestamp in the high position, randomness below.
    ///
    /// Exact for every id, which is what the [`MAX_CHARS`] cap buys. Useful anywhere an integer
    /// is genuinely wanted — a bitmask, a numeric key in another system, arithmetic on the
    /// timestamp — without having to re-derive the layout.
    ///
    /// Note that this is *width-relative*: the same timestamp at two widths yields different
    /// integers, because the randomness below it is wider. Compare [`PublicId`] values directly
    /// rather than their integer forms unless the widths are known to match.
    pub fn to_u128(&self) -> u128 {
        self.0
            .bytes()
            .fold(0u128, |acc, b| (acc << 5) | DECODE[b as usize] as u128)
    }

    /// Parse a rendered id. Accepts either case, `I`/`L`/`O` confusions, grouping hyphens, and
    /// any width in `MIN_CHARS..=MAX_CHARS`. Always yields the canonical lowercase form.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let mut out = String::with_capacity(s.len());
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
                    if out.len() >= MAX_CHARS {
                        return Err(IdError::WrongLength(out.len() + 1));
                    }
                    out.push(ALPHABET[digit as usize] as char);
                }
            }
        }
        if out.len() < MIN_CHARS {
            return Err(IdError::WrongLength(out.len()));
        }
        Ok(PublicId(out))
    }

    /// Creation time, in unix milliseconds. Read from the top 48 bits, which sit in the same
    /// place at every width — that invariance is exactly what makes widening safe.
    pub fn timestamp_ms(&self) -> u64 {
        let random_bits = self.0.len() as u32 * 5 - TIMESTAMP_BITS;
        (self.to_u128() >> random_bits) as u64
    }

    /// The random component, as an integer.
    pub fn random(&self) -> u128 {
        let random_bits = self.0.len() as u32 * 5 - TIMESTAMP_BITS;
        self.to_u128() & ((1u128 << random_bits) - 1)
    }

    /// Characters in this id.
    pub fn width(&self) -> usize {
        self.0.len()
    }

    /// Canonical rendering: lowercase, no hyphens.
    ///
    /// Allocates. Prefer [`PublicId::as_str`] where a borrow will do — the page template
    /// renders through `Display`, which borrows, and that is why the string representation
    /// outperforms a packed integer on the read path.
    pub fn encode(&self) -> String {
        self.0.clone()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Rendering grouped into fours, for display where someone may read or type it back.
    /// Parses again unchanged; the hyphens are ignored on input.
    pub fn encode_grouped(&self) -> String {
        let mut out = String::with_capacity(self.0.len() + self.0.len() / 4);
        for (i, c) in self.0.chars().enumerate() {
            if i > 0 && i % 4 == 0 {
                out.push('-');
            }
            out.push(c);
        }
        out
    }

    fn random_bits(chars: usize) -> Result<u32, IdError> {
        if !(MIN_CHARS..=MAX_CHARS).contains(&chars) {
            return Err(IdError::WrongLength(chars));
        }
        Ok(chars as u32 * 5 - TIMESTAMP_BITS)
    }

    fn render(bits: u128, chars: usize) -> String {
        let mut out = String::with_capacity(chars);
        for c in 0..chars {
            let shift = 5 * (chars - 1 - c) as u32;
            out.push(ALPHABET[((bits >> shift) & 31) as usize] as char);
        }
        out
    }
}

impl fmt::Display for PublicId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for PublicId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
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
            assert_eq!(PublicId::parse(&id.encode()), Ok(id.clone()));
            assert_eq!(id.timestamp_ms(), ms);
            assert_eq!(id.random(), rand as u128);
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

    // --- widening (DESIGN.md §4.3) ---

    #[test]
    fn wider_ids_keep_the_narrow_one_as_a_literal_prefix() {
        for chars in MIN_CHARS..=MAX_CHARS {
            let narrow = PublicId::new(T, 0).unwrap();
            let wide = PublicId::with_width(T, 0, chars).unwrap();
            assert_eq!(wide.width(), chars);
            assert!(
                wide.as_str().starts_with(narrow.as_str()),
                "{chars}-char {} does not extend {}",
                wide.as_str(),
                narrow.as_str()
            );
            // The timestamp lives in the same bits regardless of width.
            assert_eq!(wide.timestamp_ms(), T);
        }
    }

    #[test]
    fn mixed_width_ids_sort_by_creation_time() {
        // The realistic rollout: widths interleaved per id, not in clean eras, because during
        // a deploy some requests still generate the narrow form.
        let widths = [16usize, 20, 25, 18, 22];
        let mut rows: Vec<(u64, PublicId)> = Vec::new();
        for i in 0..600u64 {
            let w = widths[i as usize % widths.len()];
            let rand = (i as u128).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            rows.push((T + i, PublicId::with_width(T + i, rand, w).unwrap()));
        }
        let by_time: Vec<&PublicId> = {
            let mut v: Vec<&(u64, PublicId)> = rows.iter().collect();
            v.sort_by_key(|(t, _)| *t);
            v.into_iter().map(|(_, id)| id).collect()
        };
        let by_string: Vec<&PublicId> = {
            let mut v: Vec<&PublicId> = rows.iter().map(|(_, id)| id).collect();
            v.sort();
            v
        };
        assert_eq!(
            by_time, by_string,
            "mixed-width sort must equal creation order"
        );
    }

    #[test]
    fn integer_form_round_trips_at_every_width() {
        // The point of capping MAX_CHARS at 25: every id has an exact u128 form.
        for chars in MIN_CHARS..=MAX_CHARS {
            let id = PublicId::with_width(T, 0xDEAD_BEEF_CAFE, chars).unwrap();
            let bits = id.to_u128();
            assert_eq!(PublicId::from_u128(bits, chars).unwrap(), id);
            assert!(
                bits < (1u128 << (chars as u32 * 5)),
                "value exceeds its width"
            );
            // Timestamp sits in the top 48 bits at every width.
            assert_eq!(id.timestamp_ms(), T);
        }
        assert_eq!(MAX_BITS, 125, "must stay inside a u128");
    }

    #[test]
    fn from_u128_rejects_overwide_values() {
        assert!(matches!(
            PublicId::from_u128(1u128 << 81, MIN_CHARS),
            Err(IdError::ValueTooWide(_))
        ));
        assert!(PublicId::from_u128(0, MIN_CHARS - 1).is_err());
        assert!(PublicId::from_u128(0, MAX_CHARS + 1).is_err());
    }

    #[test]
    fn parses_any_supported_width_but_rejects_others() {
        for chars in MIN_CHARS..=MAX_CHARS {
            let id = PublicId::with_width(T, 12345, chars).unwrap();
            assert_eq!(PublicId::parse(id.as_str()), Ok(id));
        }
        assert!(PublicId::with_width(T, 0, MIN_CHARS - 1).is_err());
        assert!(PublicId::with_width(T, 0, MAX_CHARS + 1).is_err());
        assert!(PublicId::parse(&"0".repeat(MIN_CHARS - 1)).is_err());
        assert!(PublicId::parse(&"0".repeat(MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn rescues_confusable_characters() {
        let id = PublicId::new(T, 0xABCD_1234).unwrap();
        let canonical = id.encode();
        // Someone typing the id back may render 1 as I or l, and 0 as O.
        let typed = canonical.replace('1', "I").replace('0', "O");
        assert_eq!(
            PublicId::parse(&typed),
            Ok(id.clone()),
            "canonical was {canonical}"
        );
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
        // 17 chars is *not* an error: any width in MIN_CHARS..=MAX_CHARS parses, so that a
        // future wider id resolves against today's build.
        assert!(
            PublicId::parse("00000000000000000").is_ok(),
            "17 chars is a valid width"
        );
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
            prop_assert_eq!(PublicId::parse(&id.encode())?, id.clone());
            prop_assert_eq!(PublicId::parse(&id.encode_grouped())?, id.clone());
            prop_assert_eq!(PublicId::parse(&id.encode().to_uppercase())?, id);
        }

        /// The integer form is an exact, lossless view of the id.
        #[test]
        fn integer_form_is_lossless(
            ms in 0u64..=MAX_TIMESTAMP_MS,
            r in any::<u64>(),
            chars in MIN_CHARS..=MAX_CHARS,
        ) {
            let id = PublicId::with_width(ms, r as u128, chars)?;
            prop_assert_eq!(PublicId::from_u128(id.to_u128(), chars)?, id.clone());
            prop_assert_eq!(id.timestamp_ms(), ms);
        }

        /// Widening preserves both the prefix relationship and the timestamp, at every width.
        #[test]
        fn widening_preserves_prefix_and_timestamp(
            ms in 0u64..=MAX_TIMESTAMP_MS,
            r in any::<u32>(),
            chars in MIN_CHARS..=MAX_CHARS,
        ) {
            let narrow = PublicId::new(ms, r)?;
            // Widen by appending: shift the existing randomness up and fill below it.
            let extra_bits = (chars as u32 * 5) - (ID_CHARS as u32 * 5);
            let wide = PublicId::with_width(ms, (r as u128) << extra_bits, chars)?;
            prop_assert!(wide.as_str().starts_with(narrow.as_str()));
            prop_assert_eq!(wide.timestamp_ms(), ms);
            prop_assert_eq!(narrow.timestamp_ms(), ms);
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
