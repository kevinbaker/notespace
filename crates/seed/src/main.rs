//! Emits seed SQL for the M0 spike to stdout.
//!
//! Two properties matter here:
//!
//! 1. **It uses the real write path.** Bodies go through `notespace_render::markdown_to_html`,
//!    so `body_html` in the seeded database is byte-identical to what a live post would
//!    store. Measuring the read path against hand-written HTML would measure nothing.
//! 2. **It is deterministic.** A fixed-seed xorshift PRNG, not `rand`, so re-running the
//!    benchmark compares like with like.
//!
//! Usage:
//!   `cargo run -p notespace-seed -- [post_count] sql  > seed.sql`
//!   `cargo run -p notespace-seed -- [post_count] json > fixture.json`
//!
//! The JSON form feeds the wasm CPU benchmark, so the benchmark and the database measure
//! the exact same content.

use notespace_core::id::PublicId;
use notespace_core::path::Path;
use notespace_render::markdown_to_html;

/// Deterministic PRNG. Avoids a dependency and guarantees reproducible measurements.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const USERS: &[&str] = &[
    "alice", "bob", "carol", "dave", "erin", "frank", "grace", "heidi", "ivan", "judy", "mallory",
    "niaj", "olivia", "peggy", "rupert", "sybil", "trent", "victor",
];

/// Body fragments chosen to exercise the parts of the renderer a real forum hits:
/// paragraphs, inline code, fenced blocks, links, lists, quotes, emphasis.
const FRAGMENTS: &[&str] = &[
    "I've been running something similar for about eighteen months now and the maintenance\nburden is genuinely lower than I expected. The tricky part was never the software.",
    "Worth noting that this depends on which version you're on. On 2.x the flag was\n`--strict-mode`, but it got renamed in 3.0 and the old spelling now silently does nothing.",
    "```rust\nfn main() {\n    let total: u64 = (1..=100).sum();\n    println!(\"{total}\");\n}\n```\n\nThat's the whole thing. No allocation, no dependencies.",
    "> the maintenance burden is genuinely lower than I expected\n\nThis matches my experience, though I'd add that it depends heavily on how much you\ncustomise. Stock config is easy; a heavily patched install is not.",
    "A few things that bit us:\n\n- Connection limits are per-process, not per-host\n- The retry backoff is multiplicative, not exponential, despite the docs\n- Timeouts are in seconds in the config file but milliseconds in the API\n",
    "See the [upstream discussion](https://example.org/issues/1234) for background. Short\nversion: it was intentional, and the workaround is to set the header explicitly.",
    "Honestly I think this is a case where the *simple* solution is correct and we're\noverthinking it. What's the actual failure mode if we just... don't cache this?",
    "Numbers from our staging box, for whatever they're worth:\n\n| requests | p50 | p99 |\n|---|---|---|\n| 1k | 4ms | 22ms |\n| 10k | 6ms | 41ms |\n\nProduction is consistently slower but the shape is the same.",
    "Counterpoint: every time I've seen someone do this, it worked fine for two years and\nthen failed in a way that took a week to diagnose. The cost isn't in the happy path.",
    "You can check with `SELECT * FROM sqlite_master WHERE type='index'` — if the index\nisn't listed there, the query planner never had it to begin with.",
    "Yeah, agreed. I'd also flag that ~~the old approach~~ is deprecated as of last month,\nso anything written against it will need revisiting fairly soon regardless.",
    "This is the classic tradeoff between read amplification and write amplification, and\nwhich one you want depends entirely on your ratio. For a forum it's not close: reads\nwin by two orders of magnitude, so pay on write.",
];

fn body(rng: &mut Rng) -> String {
    let n = 1 + rng.below(3);
    let mut out = String::new();
    for i in 0..n {
        if i > 0 {
            out.push_str("\n\n");
        }
        out.push_str(FRAGMENTS[rng.below(FRAGMENTS.len())]);
    }
    out
}

fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// Minimal JSON string encoder, sufficient for the ASCII bodies this file generates.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Deterministic public id for seeded post `i`.
///
/// One millisecond apart so the ids sort in the same order as the posts, which is what a real
/// instance produces and what M0's index-locality result depends on.
fn post_public_id(i: usize) -> String {
    const SEED_BASE_MS: u64 = 1_735_689_600_000;
    notespace_core::PublicId::new(SEED_BASE_MS + i as u64, 0x5EED_0000 ^ i as u32)
        .expect("seed timestamp is in range")
        .encode()
}

/// SQL for a test account with a real Argon2 credential.
///
/// `notespace-seed user <name> <password> <pepper-spec>`. The pepper spec is the same string as
/// PASSWORD_PEPPER, so the hash written here verifies against the running deployment — a fixture
/// hashed under a different pepper would fail login for reasons that look like a code bug.
fn user_sql(name: &str, password: &str, pepper_spec: &str) -> String {
    use notespace_core::password::{self, PepperSet, Scheme};
    let peppers = PepperSet::parse(pepper_spec).expect("valid pepper spec");
    // Deterministic salt: a seeding tool wants reproducible fixtures.
    let salt = password::encode_salt(b"notespace-seed01").expect("salt encodes");
    let hash = password::hash(password, &salt, Scheme::CONSTRAINED, &peppers).expect("hash");
    format!(
        // Upsert: the base seed already creates accounts without credentials, so a plain
        // INSERT collides with the unique name and silently leaves them unable to log in.
        "INSERT INTO user (name, created_at, password_hash, state) VALUES ('{}', {}, '{}', 'active') \
         ON CONFLICT(name) DO UPDATE SET password_hash = excluded.password_hash, state = 'active';",
        sql_quote(&name.to_lowercase()),
        1_735_689_600_000i64,
        sql_quote(&hash),
    )
}

/// Escape a single-quoted SQL literal.
fn sql_quote(s: &str) -> String {
    s.replace('\'', "''")
}

fn main() {
    // `user <name> <password> <pepper-spec>` emits one INSERT and exits.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("user") {
        match (argv.get(2), argv.get(3), argv.get(4)) {
            (Some(name), Some(pw), Some(spec)) => {
                println!("{}", user_sql(name, pw, spec));
                return;
            }
            _ => {
                eprintln!("usage: notespace-seed user <name> <password> <pepper-spec>");
                std::process::exit(2);
            }
        }
    }
    let post_count: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let json_mode = std::env::args().nth(2).is_some_and(|m| m == "json");
    // Nesting profile. Reply-tree shape is an independent variable from post count, and the
    // read path's cost may depend on it, so it has to be controllable to be measured.
    let profile = std::env::args().nth(3).unwrap_or_else(|| "mixed".into());

    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let base_time: i64 = 1_735_689_600; // 2025-01-01T00:00:00Z, fixed for reproducibility

    // Built from the thread's own creation time so the id is time-sortable exactly as a live
    // one would be, and deterministic so re-running the benchmark compares like with like.
    let thread_public_id = PublicId::new(base_time as u64 * 1000, rng.next() as u32)
        .unwrap_or_else(|_| unreachable!("base_time is within the 48-bit range"));

    if !json_mode {
        println!("-- Generated by `cargo run -p notespace-seed -- {post_count} sql`. Do not edit.");
        println!("-- Bodies are rendered through the real write path (pulldown-cmark + ammonia).");
        println!("DELETE FROM post;");
        println!("DELETE FROM thread;");
        println!("DELETE FROM space;");
        println!("DELETE FROM user;");

        for (i, name) in USERS.iter().enumerate() {
            println!(
                "INSERT INTO user (id, name, created_at) VALUES ({}, '{}', {});",
                i + 1,
                name,
                base_time
            );
        }

        println!(
            "INSERT INTO space (id, name, ranking, depth_cap, path) \
             VALUES (1, 'General', 'bump', 8, 'general/');"
        );
        println!(
            "INSERT INTO thread (id, public_id, space_id, kind, title, author_id, created_at, \
             bumped_at, post_count, state, cache_version) VALUES (1, '{}', 1, 'discussion', \
             'Is a Rust forum on a free Cloudflare account actually viable?', 1, {base_time}, \
             {}, {post_count}, 'visible', 0);",
            thread_public_id,
            base_time + post_count as i64 * 300
        );
        eprintln!("thread public id: {thread_public_id}  (/t/{thread_public_id})");
    }

    // Build a realistic reply tree: mostly shallow, with occasional deep subthreads.
    // `frontier` holds candidate parents; picking a recent one biases toward the deep
    // back-and-forth pattern real threads actually produce.
    let mut roots = 0u32;
    let mut frontier: Vec<(Path, u32)> = Vec::new(); // (path, children so far)
    let mut rows: Vec<(usize, Path, Option<usize>)> = Vec::new();

    // Per-profile knobs: how often a post starts a new top-level subthread, and how far back
    // in the frontier a reply may attach. `flat` never nests; `deep` always extends the most
    // recent post, producing one long chain up against MAX_DEPTH.
    let (root_pct, reply_span, force_chain) = match profile.as_str() {
        "flat" => (100, 1, false), // every post top-level: the Classic BB preset
        "shallow" => (45, 24, false), // wide and short, like a busy Q&A page
        "deep" => (2, 1, true),    // pathological: near-maximal nesting
        _ => (30, 12, false),      // "mixed": realistic forum thread
    };

    for i in 0..post_count {
        let start_root = frontier.is_empty() || rng.below(100) < root_pct;
        let (path, parent_idx) = if start_root {
            roots += 1;
            (Path::root(roots).expect("root ordinal within bounds"), None)
        } else {
            let span = frontier.len().min(reply_span);
            let idx = if force_chain {
                frontier.len() - 1
            } else {
                frontier.len() - 1 - rng.below(span)
            };
            let (parent_path, kids) = frontier[idx].clone();
            match parent_path.child(kids + 1) {
                Ok(child) => {
                    frontier[idx].1 += 1;
                    let parent_row = rows
                        .iter()
                        .position(|(_, p, _)| *p == parent_path)
                        .expect("parent was emitted before its child");
                    (child, Some(parent_row))
                }
                // Depth cap reached: start a new root rather than dropping the post.
                Err(_) => {
                    roots += 1;
                    if force_chain {
                        frontier.clear();
                    }
                    (Path::root(roots).expect("root ordinal within bounds"), None)
                }
            }
        };
        frontier.push((path.clone(), 0));
        rows.push((i, path, parent_idx));
    }

    // Emit in path order so the table's physical order matches the read order. This is what
    // a real forum would NOT have (posts arrive chronologically), so it is deliberately
    // pessimistic to sort here... but the index makes physical order irrelevant, and
    // sorting keeps the generated file readable.
    let mut ordered: Vec<_> = rows.clone();
    ordered.sort_by(|a, b| a.1.cmp(&b.1));

    let mut json_posts: Vec<String> = Vec::new();
    for (i, path, parent_idx) in &ordered {
        let md = body(&mut rng);
        let html = markdown_to_html(&md);
        let author = 1 + rng.below(USERS.len());
        let created = base_time + *i as i64 * 300;

        if json_mode {
            // Hand-rolled JSON: the seed crate stays dependency-free so it cannot drift
            // from what the wasm benchmark compiles against.
            json_posts.push(format!(
                "{{\"id\":{},\"public_id\":\"{}\",\"thread_id\":1,\"parent_id\":{},\
                 \"path\":\"{}\",\"depth\":{},\
                 \"author_id\":{},\"author_name\":\"{}\",\"body_md\":{},\"body_html\":{},\
                 \"created_at\":{},\"edited_at\":null,\"score\":0.0,\"state\":\"visible\"}}",
                i + 1,
                post_public_id(*i),
                parent_idx
                    .map(|p| (p + 1).to_string())
                    .unwrap_or_else(|| "null".into()),
                path.as_str(),
                path.depth(),
                author,
                USERS[author - 1],
                json_string(&md),
                json_string(&html),
                created,
            ));
        } else {
            let parent_sql = match parent_idx {
                Some(p) => format!("{}", p + 1),
                None => "NULL".to_string(),
            };
            println!(
                "INSERT INTO post (id, public_id, thread_id, parent_id, path, depth, author_id, \
                 body_md, body_html, created_at, score, state) VALUES ({}, '{}', 1, {}, '{}', {}, \
                 {}, '{}', '{}', {}, 0, 'visible');",
                i + 1,
                post_public_id(*i),
                parent_sql,
                path.as_str(),
                path.depth(),
                author,
                sql_escape(&md),
                sql_escape(&html),
                created
            );
        }
    }

    if json_mode {
        println!(
            "{{\"space\":{{\"id\":1,\"path\":\"general/\",\"name\":\"General\",\
             \"parent_id\":null,\"ranking\":\"bump\",\"depth_cap\":8}},\
             \"thread\":{{\"id\":1,\"public_id\":\"{}\",\"space_id\":1,\"kind\":\"discussion\",\
             \"title\":\"Is a Rust forum on a free Cloudflare account actually viable?\",\
             \"url\":null,\"author_id\":1,\"author_name\":\"alice\",\"created_at\":{},\
             \"bumped_at\":{},\"post_count\":{},\"state\":\"visible\",\"cache_version\":0}},\
             \"posts\":[{}]}}",
            thread_public_id,
            base_time,
            base_time + post_count as i64 * 300,
            post_count,
            json_posts.join(",")
        );
    }

    let max_depth = ordered.iter().map(|(_, p, _)| p.depth()).max().unwrap_or(0);
    let mean_depth: f64 = ordered
        .iter()
        .map(|(_, p, _)| p.depth() as f64)
        .sum::<f64>()
        / ordered.len() as f64;
    eprintln!(
        "seeded {post_count} posts [{profile}], {roots} top-level, max depth {max_depth}, \
         mean depth {mean_depth:.2}"
    );
}
