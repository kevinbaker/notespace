//! PBKDF2 through the runtime's own `crypto.subtle`, which links no cryptography into the bundle
//! — only the glue to call it — where Argon2 is compiled in.
//!
//! Unused by default: workerd caps PBKDF2 at 100,000 iterations against OWASP's 600,000, and even
//! the capped version exceeds the 10 ms budget. Kept for a paid-plan deployment.

use crate::subtle::{call, set, subtle};
use worker::js_sys::{Array, Object, Uint8Array};
use worker::wasm_bindgen::JsValue;

/// workerd refuses more than this, to bound the DoS a single request can cause.
pub const MAX_ITERATIONS: u32 = 100_000;

/// Derive 32 bytes with PBKDF2-HMAC-SHA256.
pub async fn derive(password: &str, salt: &[u8], iterations: u32) -> Result<Vec<u8>, String> {
    if iterations > MAX_ITERATIONS {
        return Err(format!(
            "workerd caps PBKDF2 at {MAX_ITERATIONS} iterations, asked for {iterations}"
        ));
    }
    let subtle = subtle()?;

    let usages = Array::of1(&JsValue::from_str("deriveBits"));
    let imported = call(
        &subtle,
        "importKey",
        &[
            JsValue::from_str("raw"),
            Uint8Array::from(password.as_bytes()).into(),
            JsValue::from_str("PBKDF2"),
            JsValue::FALSE,
            usages.into(),
        ],
    )
    .await?;

    let params = Object::new();
    set(&params, "name", &JsValue::from_str("PBKDF2"))?;
    set(&params, "hash", &JsValue::from_str("SHA-256"))?;
    set(&params, "salt", &Uint8Array::from(salt))?;
    set(&params, "iterations", &JsValue::from_f64(iterations as f64))?;

    let bits = call(
        &subtle,
        "deriveBits",
        &[params.into(), imported, JsValue::from_f64(256.0)],
    )
    .await?;

    Ok(Uint8Array::new(&bits).to_vec())
}
