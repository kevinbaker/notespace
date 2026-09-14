//! `#[handler]` wraps an `async fn`'s body in a `SendWrapper`, so its future is `Send` the way
//! axum requires while everything it awaits -- the `Store`, the platform's HTTP client -- may
//! not be. Sound because both targets run the request on one thread: a Worker isolate has no
//! other, and the native server chooses a current-thread runtime. `SendWrapper` panics rather
//! than misbehaving if that ever stops being true.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemFn};

#[proc_macro_attribute]
pub fn handler(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let f = parse_macro_input!(item as ItemFn);
    let ItemFn {
        attrs,
        vis,
        sig,
        block,
    } = f;
    quote! {
        #(#attrs)*
        #vis #sig {
            ::send_wrapper::SendWrapper::new(async move #block).await
        }
    }
    .into()
}
