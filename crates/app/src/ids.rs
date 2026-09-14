//! Generating ids and tokens: the platform's clock for time, `getrandom` for entropy, which is
//! `crypto.getRandomValues` on a Worker and the system RNG natively.

use notespace_core::id::{IdError, PublicId};

pub fn generate(now_ms: i64) -> Result<PublicId, IdError> {
    PublicId::new(now_ms.max(0) as u64, random_u32())
}

pub fn generate_many(now_ms: i64, n: usize) -> Result<Vec<PublicId>, String> {
    (0..n)
        .map(|_| generate(now_ms).map_err(|e| e.to_string()))
        .collect()
}

pub fn random_hex() -> Result<String, String> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).map_err(|e| format!("csprng: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn random_session_token() -> Result<notespace_core::session::SessionToken, String> {
    let mut buf = [0u8; notespace_core::session::TOKEN_BYTES];
    getrandom::getrandom(&mut buf).map_err(|e| format!("csprng: {e}"))?;
    Ok(notespace_core::session::SessionToken::from_bytes(buf))
}

pub fn random_email_token() -> Result<notespace_core::email::EmailToken, String> {
    let mut buf = [0u8; notespace_core::email::TOKEN_BYTES];
    getrandom::getrandom(&mut buf).map_err(|e| format!("csprng: {e}"))?;
    Ok(notespace_core::email::EmailToken::from_bytes(buf))
}

fn random_u32() -> u32 {
    let mut buf = [0u8; 4];
    getrandom::getrandom(&mut buf).expect("the CSPRNG is unavailable");
    u32::from_le_bytes(buf)
}
