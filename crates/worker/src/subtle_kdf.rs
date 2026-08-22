//! PBKDF2 through the runtime's own `crypto.subtle`.
//!
//! The point of this module is a size comparison, and the answer is in DESIGN.md §4.10: WebCrypto
//! is a *platform* API, so using it links no cryptography into the bundle at all — only the
//! wasm-bindgen glue to call it. Argon2, being a Rust crate, is compiled in.
//!
//! It runs natively rather than in wasm, which makes it about six times faster than the same
//! iteration count compiled to wasm. That still is not enough: workerd caps PBKDF2 at 100,000
//! iterations, OWASP asks for 600,000, and even the capped version measured 12.7 ms against a
//! 10 ms budget. Kept for the measurement and for a paid-plan deployment, not used by default.

use worker::js_sys::{global, Array, Object, Promise, Reflect, Uint8Array};
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::wasm_bindgen_futures::JsFuture;

/// workerd refuses more than this, to bound the DoS a single request can cause.
pub const MAX_ITERATIONS: u32 = 100_000;

/// Derive 32 bytes with PBKDF2-HMAC-SHA256.
pub async fn derive(password: &str, salt: &[u8], iterations: u32) -> Result<Vec<u8>, String> {
    if iterations > MAX_ITERATIONS {
        return Err(format!(
            "workerd caps PBKDF2 at {MAX_ITERATIONS} iterations, asked for {iterations}"
        ));
    }
    // There is no `window` in a Worker; the global object carries `crypto`. Reaching it by
    // reflection rather than through web-sys bindings keeps this to a few hundred bytes.
    let crypto = Reflect::get(&global(), &JsValue::from_str("crypto"))
        .map_err(|e| format!("global.crypto: {e:?}"))?;
    let subtle = Reflect::get(&crypto, &JsValue::from_str("subtle"))
        .map_err(|e| format!("crypto.subtle: {e:?}"))?;

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

/// Call an async method on a JS object and await the promise it returns.
async fn call(obj: &JsValue, method: &str, args: &[JsValue]) -> Result<JsValue, String> {
    let f: worker::js_sys::Function = Reflect::get(obj, &JsValue::from_str(method))
        .map_err(|e| format!("{method}: {e:?}"))?
        .unchecked_into();
    let arr = Array::new();
    for a in args {
        arr.push(a);
    }
    let promise: Promise = Reflect::apply(&f, obj, &arr)
        .map_err(|e| format!("{method} apply: {e:?}"))?
        .unchecked_into();
    JsFuture::from(promise)
        .await
        .map_err(|e| format!("{method} await: {e:?}"))
}

fn set(o: &Object, k: &str, v: &JsValue) -> Result<(), String> {
    Reflect::set(o, &JsValue::from_str(k), v)
        .map(|_| ())
        .map_err(|e| format!("set {k}: {e:?}"))
}
