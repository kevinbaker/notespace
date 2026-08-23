//! CPU measurement harness, run under Node rather than natively: Node embeds the same V8 that
//! workerd does, so this times the same compiled wasm on the same JIT. Not a perfect proxy —
//! isolate startup, memory limits and hardware all differ — but enough to answer whether
//! something is near 10 ms or nowhere near. `Date.now()` inside a real Worker is too coarse to
//! measure with, which is why this is not in the Worker itself.

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
    static FIXTURE: RefCell<Option<ThreadPage>> = const { RefCell::new(None) };
}

/// Outside the timed region: a real Worker gets its posts from D1, not from JSON.
#[wasm_bindgen]
pub fn load(json: &str) -> Result<usize, JsError> {
    let f: Fixture = serde_json::from_str(json).map_err(|e| JsError::new(&e.to_string()))?;
    let n = f.posts.len();
    let page = ThreadPage {
        space: f.space,
        thread: f.thread,
        posts: f.posts,
        next_cursor: None,
    };
    FIXTURE.with(|c| *c.borrow_mut() = Some(page));
    Ok(n)
}

/// **The read path**, run on every request that misses cache. Returns the page length so the
/// optimiser cannot elide the work.
#[wasm_bindgen]
pub fn render_read_path() -> usize {
    FIXTURE.with(|c| {
        let b = c.borrow();
        let page = b.as_ref().expect("load() must run first");
        thread_page(page).into_string().len()
    })
}

/// **The write path, batched 200x.** Paid once per post at submit time in production; batching
/// gives both the per-post cost and a worst-case whole-thread rebuild.
#[wasm_bindgen]
pub fn render_write_path() -> usize {
    FIXTURE.with(|c| {
        let b = c.borrow();
        let page = b.as_ref().expect("load() must run first");
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
        let page = b.as_ref().expect("load() must run first");
        let depths: Vec<f64> = page.posts.iter().map(|p| p.path.depth() as f64).collect();
        let mean = depths.iter().sum::<f64>() / depths.len().max(1) as f64;
        let max = depths.iter().cloned().fold(0.0, f64::max);
        vec![mean, max]
    })
}

/// **Path parsing on the read path**, paid once per post because `D1Store` validates each one.
#[wasm_bindgen]
pub fn bench_path_parse() -> usize {
    FIXTURE.with(|c| {
        let b = c.borrow();
        let page = b.as_ref().expect("load() must run first");
        page.posts
            .iter()
            .filter_map(|p| notespace_core::path::Path::parse(p.path.as_str()).ok())
            .map(|p| p.depth())
            .sum()
    })
}

/// **Path construction on the write path**: the only tree bookkeeping an insert does.
#[wasm_bindgen]
pub fn bench_path_build() -> usize {
    FIXTURE.with(|c| {
        let b = c.borrow();
        let page = b.as_ref().expect("load() must run first");
        page.posts
            .iter()
            .filter_map(|p| p.path.child(1).ok())
            .map(|p| p.as_str().len())
            .sum()
    })
}

/// **Ordering.** SQLite does this in the index; the comparison cost is what makes it cheap.
#[wasm_bindgen]
pub fn bench_path_sort() -> usize {
    FIXTURE.with(|c| {
        let b = c.borrow();
        let page = b.as_ref().expect("load() must run first");
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
        let page = b.as_ref().expect("load() must run first");
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
        let page = b.as_ref().expect("load() must run first");
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

/// A nonzero result invalidates every timing below.
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
            match (PublicId::parse(&messy[i]), PackedId::parse(&messy[i])) {
                (Ok(x), Some(y)) if x.encode() == canon[i] && y.encode() == canon[i] => {}
                _ => bad += 1,
            }
        }
        // Sort order across widths is the subtle part for the packed form.
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

/// The mix as the real template runs it: through `Display`, which the string form borrows for
/// and the packed form must allocate for. `id_page_mix_string` charges a clone the render path
/// never performs.
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

/// One request's worth: parse the id from the URL, parse the one D1 returned, render it into the
/// page a few times.
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

// ---------------------------------------------------------------------------
// Password hashing against the 10 ms CPU budget
// ---------------------------------------------------------------------------
//
// Password hashing is deliberately slow and the free-plan CPU limit is 10 ms per request.

use argon2::{Algorithm, Argon2, Params, Version};

/// PBKDF2-HMAC-SHA256 at `iters`, in wasm. Returns a byte of the output so nothing is elided.
#[wasm_bindgen]
pub fn kdf_pbkdf2(iters: u32) -> u8 {
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(
        b"correct horse battery staple",
        b"a-salt-16-bytes!",
        iters,
        &mut out,
    );
    out[0]
}

/// Argon2id at the given cost. `m_kib` memory, `t` passes, 1 lane.
#[wasm_bindgen]
pub fn kdf_argon2(m_kib: u32, t: u32) -> u8 {
    let params = Params::new(m_kib, t, 1, Some(32)).expect("valid params");
    let a = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    a.hash_password_into(
        b"correct horse battery staple",
        b"a-salt-16-bytes!",
        &mut out,
    )
    .expect("hash");
    out[0]
}

/// Argon2id with a pepper (Argon2's own `K` parameter), to confirm it costs nothing.
#[wasm_bindgen]
pub fn kdf_argon2_peppered(m_kib: u32, t: u32) -> u8 {
    let params = Params::new(m_kib, t, 1, Some(32)).expect("valid params");
    let secret = [0x5Au8; 32];
    let a = Argon2::new_with_secret(&secret, Algorithm::Argon2id, Version::V0x13, params)
        .expect("secret within length");
    let mut out = [0u8; 32];
    a.hash_password_into(
        b"correct horse battery staple",
        b"a-salt-16-bytes!",
        &mut out,
    )
    .expect("hash");
    out[0]
}

/// HMAC-SHA256 over a short message: the CSRF token path.
#[wasm_bindgen]
pub fn csrf_hmac(n: u32) -> u8 {
    use hmac::{Mac, SimpleHmac};
    let mut last = 0u8;
    for i in 0..n {
        let mut mac =
            SimpleHmac::<sha2::Sha256>::new_from_slice(b"a-32-byte-server-secret-value!!!")
                .expect("key");
        mac.update(b"session-token-hash:1800000000000");
        mac.update(&i.to_le_bytes());
        last = mac.finalize().into_bytes()[0];
    }
    last
}
