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

pub mod packed;

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

// ---------------------------------------------------------------------------
// Public id representation: canonical `String` (shipping) vs packed `u128`.
// ---------------------------------------------------------------------------

use crate::packed::PackedId;
use notespace_core::id::PublicId;

/// The same ids in both representations, plus their canonical and messy renderings.
#[derive(Default)]
struct IdCorpus {
    strings: Vec<PublicId>,
    packed: Vec<PackedId>,
    canonical: Vec<String>,
    messy: Vec<String>,
}

thread_local! {
    static IDS: RefCell<IdCorpus> = RefCell::new(IdCorpus::default());
}

/// Build `n` ids at mixed widths in both representations.
#[wasm_bindgen]
pub fn id_setup(n: usize) -> usize {
    const WIDTHS: [usize; 5] = [16, 18, 20, 22, 26];
    let base: u64 = 1_735_689_600_000;
    let (mut a, mut b, mut canon, mut messy) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for i in 0..n as u64 {
        let w = WIDTHS[i as usize % WIDTHS.len()];
        let rand = (i as u128).wrapping_mul(0x9E37_79B9_7F4A_7C15_1234_5678_9ABC_DEF1);
        let Some(p) = PackedId::new(base + i, rand, w) else {
            continue;
        };
        let Ok(s) = PublicId::with_width(base + i, rand, w) else {
            continue;
        };
        let text = s.encode();
        // The URL case: uppercase with grouping hyphens, as someone might type it.
        let grouped = s.encode_grouped().to_uppercase();
        a.push(s);
        b.push(p);
        canon.push(text);
        messy.push(grouped);
    }
    let len = a.len();
    IDS.with(|c| {
        *c.borrow_mut() = IdCorpus {
            strings: a,
            packed: b,
            canonical: canon,
            messy,
        }
    });
    len
}

/// Verify the two representations agree before timing them. Returns the number of mismatches;
/// a nonzero result invalidates every timing below.
#[wasm_bindgen]
pub fn id_cross_check() -> usize {
    IDS.with(|c| {
        let corpus = c.borrow();
        let (strs, packs, canon, messy) = (
            &corpus.strings,
            &corpus.packed,
            &corpus.canonical,
            &corpus.messy,
        );
        let mut bad = 0;
        for i in 0..strs.len() {
            if strs[i].encode() != packs[i].encode() {
                bad += 1;
            }
            if strs[i].timestamp_ms() != packs[i].timestamp_ms() {
                bad += 1;
            }
            if strs[i].width() != packs[i].width() {
                bad += 1;
            }
            // Both must accept the messy form and normalise to the same canonical text.
            match (PublicId::parse(&messy[i]), PackedId::parse(&messy[i])) {
                (Ok(x), Some(y)) if x.encode() == canon[i] && y.encode() == canon[i] => {}
                _ => bad += 1,
            }
        }
        // Sort order must agree across widths, which is the subtle part for the packed form.
        let mut sa: Vec<&PublicId> = strs.iter().collect();
        let mut sb: Vec<&PackedId> = packs.iter().collect();
        sa.sort();
        sb.sort();
        for i in 0..sa.len() {
            if sa[i].encode() != sb[i].encode() {
                bad += 1;
            }
        }
        bad
    })
}

macro_rules! id_bench {
    ($name:ident, $body:expr) => {
        #[wasm_bindgen]
        pub fn $name() -> usize {
            IDS.with(|c| {
                let b = c.borrow();
                #[allow(clippy::redundant_closure_call)]
                ($body)(&b.strings, &b.packed, &b.canonical, &b.messy)
            })
        }
    };
}

id_bench!(
    id_parse_canonical_string,
    |_: &Vec<PublicId>, _: &Vec<PackedId>, canon: &Vec<String>, _: &Vec<String>| {
        canon
            .iter()
            .filter_map(|s| PublicId::parse(s).ok())
            .map(|i| i.width())
            .sum()
    }
);

id_bench!(
    id_parse_canonical_packed,
    |_: &Vec<PublicId>, _: &Vec<PackedId>, canon: &Vec<String>, _: &Vec<String>| {
        canon
            .iter()
            .filter_map(|s| PackedId::parse(s))
            .map(|i| i.width())
            .sum()
    }
);

id_bench!(
    id_parse_messy_string,
    |_: &Vec<PublicId>, _: &Vec<PackedId>, _: &Vec<String>, messy: &Vec<String>| {
        messy
            .iter()
            .filter_map(|s| PublicId::parse(s).ok())
            .map(|i| i.width())
            .sum()
    }
);

id_bench!(
    id_parse_messy_packed,
    |_: &Vec<PublicId>, _: &Vec<PackedId>, _: &Vec<String>, messy: &Vec<String>| {
        messy
            .iter()
            .filter_map(|s| PackedId::parse(s))
            .map(|i| i.width())
            .sum()
    }
);

id_bench!(id_encode_string, |strs: &Vec<PublicId>, _, _, _| strs
    .iter()
    .map(|i| i.encode().len())
    .sum());

id_bench!(id_encode_packed, |_, packs: &Vec<PackedId>, _, _| packs
    .iter()
    .map(|i| i.encode().len())
    .sum());

id_bench!(id_timestamp_string, |strs: &Vec<PublicId>, _, _, _| strs
    .iter()
    .map(|i| i.timestamp_ms() as usize)
    .sum());

id_bench!(id_timestamp_packed, |_, packs: &Vec<PackedId>, _, _| packs
    .iter()
    .map(|i| i.timestamp_ms() as usize)
    .sum());

id_bench!(id_sort_string, |strs: &Vec<PublicId>, _, _, _| {
    let mut v: Vec<&PublicId> = strs.iter().collect();
    v.sort();
    v.len()
});

id_bench!(id_sort_packed, |_, packs: &Vec<PackedId>, _, _| {
    let mut v: Vec<&PackedId> = packs.iter().collect();
    v.sort();
    v.len()
});

/// The mix as the real template runs it.
///
/// `page.rs` renders the id through `Display`, which for the string form borrows and for the
/// packed form must materialise a `String` every time. Measuring `.encode()` instead (as
/// `id_page_mix_string` does) charges the string form for a clone that the render path never
/// performs, so this is the honest comparison.
#[wasm_bindgen]
pub fn id_page_mix_string_display() -> usize {
    IDS.with(|c| {
        let corpus = c.borrow();
        let (canon, messy) = (&corpus.canonical, &corpus.messy);
        let mut acc = 0;
        for i in 0..canon.len() {
            if let (Ok(from_url), Ok(from_db)) =
                (PublicId::parse(&messy[i]), PublicId::parse(&canon[i]))
            {
                // Borrowed, not cloned: what `(t.public_id)` in a maud template costs.
                acc += from_url.as_str().len() + from_db.as_str().len() * 3;
            }
        }
        acc
    })
}

/// What one thread-page request actually costs: parse the id from the URL, parse the one that
/// came back from D1, then render it into the page a few times (canonical URL, RSS link,
/// pager, personalisation hook).
#[wasm_bindgen]
pub fn id_page_mix_string() -> usize {
    IDS.with(|c| {
        let corpus = c.borrow();
        let (canon, messy) = (&corpus.canonical, &corpus.messy);
        let mut acc = 0;
        for i in 0..canon.len() {
            if let (Ok(from_url), Ok(from_db)) =
                (PublicId::parse(&messy[i]), PublicId::parse(&canon[i]))
            {
                acc += from_url.encode().len() + from_db.encode().len() * 3;
            }
        }
        acc
    })
}

#[wasm_bindgen]
pub fn id_page_mix_packed() -> usize {
    IDS.with(|c| {
        let corpus = c.borrow();
        let (canon, messy) = (&corpus.canonical, &corpus.messy);
        let mut acc = 0;
        for i in 0..canon.len() {
            if let (Some(from_url), Some(from_db)) =
                (PackedId::parse(&messy[i]), PackedId::parse(&canon[i]))
            {
                acc += from_url.encode().len() + from_db.encode().len() * 3;
            }
        }
        acc
    })
}

/// In-memory footprint of each representation, excluding heap for the string case.
#[wasm_bindgen]
pub fn id_sizes() -> Vec<usize> {
    vec![
        core::mem::size_of::<PublicId>(),
        core::mem::size_of::<PackedId>(),
    ]
}
