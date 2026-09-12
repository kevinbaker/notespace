//! Rendering, split by when it runs: [`markdown`] once per post at write time, where it can
//! afford to be expensive, and [`page`] on a cold read, assembling already-rendered fragments.

pub mod account;
pub mod auth;
pub mod compose;
pub mod feed;
pub mod index;
pub mod markdown;
pub mod moderation;
pub mod page;
pub mod profile;
pub mod time;

pub use markdown::{markdown_to_html, sanitize};
pub use page::thread_page;

/// Changes whenever the baked templates change, so a deploy invalidates every cached page on
/// its own: the edge cache is keyed on this as well as the thread's version. A hash of the
/// template sources at compile time, which is the one thing that cannot be forgotten.
pub const BAKE_REVISION: u64 = fnv1a(
    concat!(
        include_str!("page.rs"),
        include_str!("time.rs"),
        include_str!("feed.rs")
    )
    .as_bytes(),
);

const fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    h
}

/// A plain error page with a way home, for anything that is not a 400.
pub fn error_page(status: u16, reason: &str) -> maud::Markup {
    use maud::{html, DOCTYPE};
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                link rel="icon" href="data:,";
                title { (status) " " (reason) }
                style { (auth::PreEscapedStyle) }
            }
            body {
                main class="auth" {
                    h1 { (status) " " (reason) }
                    @match status {
                        404 => p { "There is nothing at this address. It may have moved, or the link may be wrong." },
                        503 => p { "This part of the site is not configured yet." },
                        _ => p { "Something went wrong on our side. It has been logged." },
                    }
                    p class="muted" { a href="/" { "Back to the index" } }
                }
            }
        }
    }
}
