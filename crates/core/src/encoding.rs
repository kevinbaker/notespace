//! Base64 and base64url, dependency-free: JWTs, basic auth and sealed cookies need them, and
//! nothing else in `core` does.

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn encode_with(table: &[u8; 64], bytes: &[u8], pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(table[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

/// Standard alphabet, padded.
pub fn base64(bytes: &[u8]) -> String {
    encode_with(STD, bytes, true)
}

/// URL-safe alphabet, unpadded: the JWT and cookie form.
pub fn base64url(bytes: &[u8]) -> String {
    encode_with(URL, bytes, false)
}

/// Decodes either alphabet, padded or not. `None` on any character outside them.
pub fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_base64_matches_the_rfc_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"api:key-1"), "YXBpOmtleS0x");
    }

    #[test]
    fn url_safe_is_unpadded_and_round_trips() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let enc = base64url(&bytes);
        assert!(!enc.contains('=') && !enc.contains('+') && !enc.contains('/'));
        assert_eq!(base64url_decode(&enc).unwrap(), bytes);
        assert_eq!(base64url(b"\xfb\xff"), "-_8");
        // Padded standard input decodes too, which is what a JWT library's output looks like.
        assert_eq!(base64url_decode("Zm9v").unwrap(), b"foo");
        assert_eq!(base64url_decode("Zg==").unwrap(), b"f");
        assert!(base64url_decode("not base64!").is_none());
    }
}
