//! `crypto.subtle` by reflection. No `window` in a Worker, and reaching the API this way keeps
//! the glue to a few hundred bytes where `web-sys` bindings would be far more.

use notespace_core::oidc::{Jwk, SignatureCheck};
use worker::js_sys::{global, Array, Function, Object, Promise, Reflect, Uint8Array};
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::wasm_bindgen_futures::JsFuture;

pub fn subtle() -> Result<JsValue, String> {
    let crypto = Reflect::get(&global(), &JsValue::from_str("crypto"))
        .map_err(|e| format!("global.crypto: {e:?}"))?;
    Reflect::get(&crypto, &JsValue::from_str("subtle")).map_err(|e| format!("crypto.subtle: {e:?}"))
}

/// Call an async method on a JS object and await the promise it returns.
pub async fn call(obj: &JsValue, method: &str, args: &[JsValue]) -> Result<JsValue, String> {
    let f: Function = Reflect::get(obj, &JsValue::from_str(method))
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

pub fn set(o: &Object, k: &str, v: &JsValue) -> Result<(), String> {
    Reflect::set(o, &JsValue::from_str(k), v)
        .map(|_| ())
        .map_err(|e| format!("set {k}: {e:?}"))
}

/// RSASSA-PKCS1-v1_5 with SHA-256, the `RS256` every OpenID provider signs with. The key
/// arrives as a JWK and is imported as one; the runtime does the arithmetic.
pub struct Rs256;

#[async_trait::async_trait(?Send)]
impl SignatureCheck for Rs256 {
    async fn verify_rs256(
        &self,
        key: &Jwk,
        signing_input: &[u8],
        signature: &[u8],
    ) -> Result<bool, String> {
        let subtle = subtle()?;
        let jwk = Object::new();
        set(&jwk, "kty", &JsValue::from_str("RSA"))?;
        set(&jwk, "n", &JsValue::from_str(&key.n))?;
        set(&jwk, "e", &JsValue::from_str(&key.e))?;
        set(&jwk, "alg", &JsValue::from_str("RS256"))?;
        set(&jwk, "ext", &JsValue::TRUE)?;
        let algorithm = Object::new();
        set(&algorithm, "name", &JsValue::from_str("RSASSA-PKCS1-v1_5"))?;
        set(&algorithm, "hash", &JsValue::from_str("SHA-256"))?;
        let imported = call(
            &subtle,
            "importKey",
            &[
                JsValue::from_str("jwk"),
                jwk.into(),
                algorithm.clone().into(),
                JsValue::FALSE,
                Array::of1(&JsValue::from_str("verify")).into(),
            ],
        )
        .await?;
        let ok = call(
            &subtle,
            "verify",
            &[
                algorithm.into(),
                imported,
                Uint8Array::from(signature).into(),
                Uint8Array::from(signing_input).into(),
            ],
        )
        .await?;
        Ok(ok.as_bool().unwrap_or(false))
    }
}

/// The platform's RS256 check, over `crypto.subtle`.
pub async fn verify_rs256(
    key: &Jwk,
    signing_input: &[u8],
    signature: &[u8],
) -> Result<bool, String> {
    Rs256.verify_rs256(key, signing_input, signature).await
}
