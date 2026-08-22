//! Generating public ids on a Worker.
//!
//! DESIGN.md §3.2 and §9: no `std::time::SystemTime` and no system RNG on wasm. Both inputs
//! therefore come from the platform rather than from `core`, which is why
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

fn random_u32() -> u32 {
    let mut buf = [0u8; 4];
    // A failure here means the platform has no CSPRNG, which on Workers cannot happen. Falling
    // back to the clock would produce guessable ids, so prefer a loud failure to a quiet one.
    getrandom::getrandom(&mut buf).expect("crypto.getRandomValues is unavailable");
    u32::from_le_bytes(buf)
}
