//! Generating public ids on a Worker: `Date.now()` for time and `crypto.getRandomValues` for
//! randomness, since wasm has neither `SystemTime` nor a system RNG. Not `Math.random()`, whose
//! output V8 makes predictable — and a predictable id is an enumerable one.

use notespace_core::id::{IdError, PublicId};
use worker::Date;

/// A fresh public id, timestamped now.
pub fn generate() -> Result<PublicId, IdError> {
    PublicId::new(Date::now().as_millis(), random_u32())
}

/// Independently generated, so a retry never re-submits the id that just lost a collision.
pub fn generate_many(n: usize) -> Result<Vec<PublicId>, String> {
    (0..n)
        .map(|_| generate().map_err(|e| e.to_string()))
        .collect()
}

/// 32 random bytes as hex, for anonymous CSRF cookies.
pub fn random_hex() -> Result<String, String> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).map_err(|e| format!("csprng: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// A fresh session token.
#[cfg(feature = "password")]
pub fn random_session_token() -> Result<notespace_core::session::SessionToken, String> {
    let mut buf = [0u8; notespace_core::session::TOKEN_BYTES];
    getrandom::getrandom(&mut buf).map_err(|e| format!("csprng: {e}"))?;
    Ok(notespace_core::session::SessionToken::from_bytes(buf))
}

/// A fresh link token for mail.
pub fn random_email_token() -> Result<notespace_core::email::EmailToken, String> {
    let mut buf = [0u8; notespace_core::email::TOKEN_BYTES];
    getrandom::getrandom(&mut buf).map_err(|e| format!("csprng: {e}"))?;
    Ok(notespace_core::email::EmailToken::from_bytes(buf))
}

fn random_u32() -> u32 {
    let mut buf = [0u8; 4];
    // Falling back to the clock would produce guessable ids.
    getrandom::getrandom(&mut buf).expect("crypto.getRandomValues is unavailable");
    u32::from_le_bytes(buf)
}
