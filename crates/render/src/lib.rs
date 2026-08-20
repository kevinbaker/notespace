//! Rendering, split by *when* it runs.
//!
//! DESIGN.md §3.3: "Render at write time, never at read time."
//!
//! - [`markdown`] runs once per post, on write. It is allowed to be comparatively expensive.
//! - [`page`] runs on a cold read, assembling already-rendered fragments. It must be cheap.
//!
//! The 10ms CPU budget applies to both, but only the read path runs on every request.

pub mod markdown;
pub mod page;

pub use markdown::{markdown_to_html, sanitize};
pub use page::thread_page;
