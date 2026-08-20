//! CPU measurement harness for the M0 spike.
//!
//! # Why this exists rather than a native `criterion` benchmark
//!
//! The number DESIGN.md §8 asks for is CPU *on a Worker*, and a Worker runs wasm under V8.
//! Native x86 timings would be measuring the wrong machine code on the wrong engine. Node
//! runs the same V8 that workerd embeds, so timing this module under Node measures the same
//! compiled wasm, executed by the same JIT, that Cloudflare would run.
//!
//! It is not a perfect proxy — workerd's isolate differs in startup and in memory limits,
//! and production hardware differs from this container. It is close enough to answer the
//! question M0 actually poses: is this within an order of magnitude of 10ms, or nowhere near?
//!
//! `Date.now()` inside a real Worker is coarse and advances only on I/O, which is exactly
//! why the measurement is taken here instead of in the Worker itself.

use std::cell::RefCell;

use notespace_core::model::{Space, Thread, ThreadPage};
use notespace_render::{markdown_to_html, thread_page};
use serde::Deserialize;
use wasm_bindgen::prelude::*;

#[derive(Deserialize)]
struct Fixture {
    space: Space,
    thread: Thread,
    posts: Vec<notespace_core::model::Post>,
}

thread_local! {
    static FIXTURE: RefCell<Option<(Space, ThreadPage)>> = const { RefCell::new(None) };
}

/// Parse the fixture. Deliberately outside the timed region: a real Worker gets its posts
/// from D1, not from JSON, so JSON parsing is not part of the read path.
#[wasm_bindgen]
pub fn load(json: &str) -> Result<usize, JsError> {
    let f: Fixture = serde_json::from_str(json).map_err(|e| JsError::new(&e.to_string()))?;
    let n = f.posts.len();
    let page = ThreadPage {
        thread: f.thread,
        posts: f.posts,
        next_cursor: None,
    };
    FIXTURE.with(|c| *c.borrow_mut() = Some((f.space, page)));
    Ok(n)
}

/// **The read path.** What runs on every cold thread-page request: take posts whose HTML was
/// rendered at write time and assemble the document. This is the number that matters, because
/// it runs on every request that misses cache.
///
/// Returns the byte length of the rendered page so the optimiser cannot elide the work.
#[wasm_bindgen]
pub fn render_read_path() -> usize {
    FIXTURE.with(|c| {
        let b = c.borrow();
        let (space, page) = b.as_ref().expect("load() must run first");
        thread_page(space, page).into_string().len()
    })
}

/// **The write path, batched 200x.** Re-renders every post's markdown through
/// pulldown-cmark + ammonia. In production this cost is paid once per post at submit time,
/// never per page view; measuring 200 at once gives the per-post cost and shows what a
/// worst-case cold rebuild of a whole thread would cost.
#[wasm_bindgen]
pub fn render_write_path() -> usize {
    FIXTURE.with(|c| {
        let b = c.borrow();
        let (_, page) = b.as_ref().expect("load() must run first");
        page.posts
            .iter()
            .filter_map(|p| p.body_md.as_deref())
            .map(|md| markdown_to_html(md).len())
            .sum()
    })
}

/// Mean and max nesting depth of the loaded fixture, so a timing can be attributed to a shape.
#[wasm_bindgen]
pub fn depth_stats() -> Vec<f64> {
    FIXTURE.with(|c| {
        let b = c.borrow();
        let (_, page) = b.as_ref().expect("load() must run first");
        let depths: Vec<f64> = page.posts.iter().map(|p| p.path.depth() as f64).collect();
        let mean = depths.iter().sum::<f64>() / depths.len().max(1) as f64;
        let max = depths.iter().cloned().fold(0.0, f64::max);
        vec![mean, max]
    })
}

/// **Path parsing on the read path.** `D1Store` validates every path it loads, so a thread
/// page pays this once per post. Measured separately because it is pure nested-tree logic:
/// if materialized paths were expensive to handle, this is where it would show.
#[wasm_bindgen]
pub fn bench_path_parse() -> usize {
    FIXTURE.with(|c| {
        let b = c.borrow();
        let (_, page) = b.as_ref().expect("load() must run first");
        page.posts
            .iter()
            .filter_map(|p| notespace_core::path::Path::parse(p.path.as_str()).ok())
            .map(|p| p.depth())
            .sum()
    })
}

/// **Path construction on the write path.** Computing a reply's path from its parent's,
/// which is the only tree bookkeeping an insert has to do.
#[wasm_bindgen]
pub fn bench_path_build() -> usize {
    FIXTURE.with(|c| {
        let b = c.borrow();
        let (_, page) = b.as_ref().expect("load() must run first");
        page.posts
            .iter()
            .filter_map(|p| p.path.child(1).ok())
            .map(|p| p.as_str().len())
            .sum()
    })
}

/// **Ordering.** SQLite does this in the index, but the cost of comparing paths is what makes
/// that index cheap, so it is worth knowing.
#[wasm_bindgen]
pub fn bench_path_sort() -> usize {
    FIXTURE.with(|c| {
        let b = c.borrow();
        let (_, page) = b.as_ref().expect("load() must run first");
        let mut paths: Vec<&str> = page.posts.iter().map(|p| p.path.as_str()).collect();
        paths.sort_unstable();
        paths.iter().map(|p| p.len()).sum()
    })
}

/// **Ancestor tests.** What a collapse-subthread or moderate-subtree operation walks.
#[wasm_bindgen]
pub fn bench_path_descendant() -> usize {
    FIXTURE.with(|c| {
        let b = c.borrow();
        let (_, page) = b.as_ref().expect("load() must run first");
        let anchors: Vec<_> = page.posts.iter().take(16).map(|p| &p.path).collect();
        page.posts
            .iter()
            .map(|p| {
                anchors
                    .iter()
                    .filter(|a| p.path.is_descendant_of(a))
                    .count()
            })
            .sum()
    })
}

/// Total bytes of stored path text, which is what the `(thread_id, path)` index has to hold.
#[wasm_bindgen]
pub fn path_bytes() -> usize {
    FIXTURE.with(|c| {
        let b = c.borrow();
        let (_, page) = b.as_ref().expect("load() must run first");
        page.posts.iter().map(|p| p.path.as_str().len()).sum()
    })
}

/// Size of the rendered page, for reporting alongside the timings.
#[wasm_bindgen]
pub fn page_bytes() -> usize {
    render_read_path()
}
