//! Generating public ids on a Worker.
//!
//! There is no `std::time::SystemTime` and no system RNG on wasm. Both inputs therefore come
//! from the platform rather than from `core`, which is why
//! [`PublicId::new`](notespace_core::id::PublicId::new) takes them as parameters.
//!
//! - **Time** is `Date.now()` through `worker::Date`, which is the JS clock.
//! - **Randomness** is `crypto.getRandomValues`, reached through `getrandom`'s `js` feature.
//!   Not `Math.random()`: V8's PRNG is predictable from observed output, and a predictable id
//!   is an enumerable one — which defeats the only thing an opaque id buys.

use notespace_core::id::{IdError, PublicId};
use worker::Date;

/// A fresh public id, timestamped now.
pub fn generate() -> Result<PublicId, IdError> {
    PublicId::new(Date::now().as_millis(), random_u32())
}

/// Several ids at once, for a write that may have to retry.
///
/// Each is generated independently, so a retry never re-submits the id that just lost a path
/// collision -- which could not win the second time either.
pub fn generate_many(n: usize) -> Result<Vec<PublicId>, String> {
    (0..n)
        .map(|_| generate().map_err(|e| e.to_string()))
        .collect()
}

/// 32 random bytes as hex, for anonymous CSRF cookies.
#[cfg(feature = "password")]
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

fn random_u32() -> u32 {
    let mut buf = [0u8; 4];
    // A failure here means the platform has no CSPRNG, which on Workers cannot happen. Falling
    // back to the clock would produce guessable ids, so prefer a loud failure to a quiet one.
    getrandom::getrandom(&mut buf).expect("crypto.getRandomValues is unavailable");
    u32::from_le_bytes(buf)
}
