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

pub use markdown::{markdown_to_html, sanitize};
pub use page::thread_page;
