//! Candidate representation for `PublicId`: a packed integer instead of a canonical `String`.
//!
//! Exists only to be benchmarked against the shipping type. See `docs/M0-findings.md`.
//!
//! # The 128-bit cap
//!
//! A width of 26 characters is 130 bits, which does not fit a `u128`. Capping at 25 characters
//! (125 bits: 48 of timestamp, 77 random) does fit, and gives up essentially nothing — 77
//! random bits against ULID's 80 is not a difference any forum can observe. That cap is what
//! makes this representation possible at all.

pub const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
pub const MIN_CHARS: usize = 16;
pub const MAX_CHARS: usize = 25;
pub const MAX_BITS: u32 = (MAX_CHARS as u32) * 5; // 125
pub const TIMESTAMP_BITS: u32 = 48;

const INVALID: u8 = 0xFF;
const SKIP: u8 = 0xFE;

const DECODE: [u8; 256] = {
    let mut t = [INVALID; 256];
    let mut i = 0;
    while i < 32 {
        let c = ALPHABET[i];
        t[c as usize] = i as u8;
        if c >= b'a' && c <= b'z' {
            t[(c - 32) as usize] = i as u8;
        }
        i += 1;
    }
    t[b'i' as usize] = 1;
    t[b'I' as usize] = 1;
    t[b'l' as usize] = 1;
    t[b'L' as usize] = 1;
    t[b'o' as usize] = 0;
    t[b'O' as usize] = 0;
    t[b'-' as usize] = SKIP;
    t
};

/// 125 bits of payload plus the width it was rendered at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackedId {
    bits: u128,
    width: u8,
}

impl PackedId {
    pub fn new(timestamp_ms: u64, random: u128, width: usize) -> Option<Self> {
        if !(MIN_CHARS..=MAX_CHARS).contains(&width) {
            return None;
        }
        let total = width as u32 * 5;
        let random_bits = total - TIMESTAMP_BITS;
        let mask = if random_bits >= 128 {
            u128::MAX
        } else {
            (1u128 << random_bits) - 1
        };
        Some(PackedId {
            bits: ((timestamp_ms as u128) << random_bits) | (random & mask),
            width: width as u8,
        })
    }

    pub fn parse(s: &str) -> Option<Self> {
        let mut bits: u128 = 0;
        let mut width = 0usize;
        for ch in s.chars() {
            if !ch.is_ascii() {
                return None;
            }
            match DECODE[ch as usize] {
                SKIP => continue,
                INVALID => return None,
                d => {
                    width += 1;
                    if width > MAX_CHARS {
                        return None;
                    }
                    bits = (bits << 5) | d as u128;
                }
            }
        }
        if width < MIN_CHARS {
            return None;
        }
        Some(PackedId {
            bits,
            width: width as u8,
        })
    }

    pub fn encode(&self) -> String {
        let w = self.width as usize;
        let mut out = String::with_capacity(w);
        for c in 0..w {
            let shift = 5 * (w - 1 - c) as u32;
            out.push(ALPHABET[((self.bits >> shift) & 31) as usize] as char);
        }
        out
    }

    pub fn timestamp_ms(&self) -> u64 {
        (self.bits >> (self.width as u32 * 5 - TIMESTAMP_BITS)) as u64
    }

    pub fn width(&self) -> usize {
        self.width as usize
    }

    /// Left-align to a common width so numeric order matches string order across widths, then
    /// break ties by width: a shorter id is a prefix of a longer one, and a prefix sorts first.
    fn normalized(&self) -> u128 {
        self.bits << (MAX_BITS - self.width as u32 * 5)
    }
}

impl PartialOrd for PackedId {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PackedId {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.normalized()
            .cmp(&other.normalized())
            .then(self.width.cmp(&other.width))
    }
}
