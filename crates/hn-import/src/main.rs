//! Turns the NDJSON from `scripts/hn-fetch.mjs` into notespace seed SQL on stdout.
//!
//! ```text
//! cargo run --release -p notespace-hn-import -- target/hn/hn.ndjson > target/hn/hn.sql
//! ```
//!
//! Bodies go through `notespace_render::markdown_to_html`, so `body_html` is byte-identical to
//! what the live write path would have stored.

mod hn;
mod markdown;
mod names;
mod tree;

use std::collections::HashSet;
use std::error::Error;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::ExitCode;

use notespace_core::path::MAX_DEPTH;
use notespace_core::reply::MAX_BODY_CHARS;
use notespace_core::PublicId;
use notespace_render::markdown_to_html;

use hn::Item;
use names::Names;

/// Threads and posts are inserted with explicit ids from here up, which is what makes `--reset`
/// a range delete and a re-import idempotent. Anything already in the database -- the M0 seed,
/// or a real account -- uses low autoincrement ids and is left alone.
const ID_BASE: i64 = 1_000_000;

/// Rows per `INSERT`. Bodies dominate the statement size, so the byte budget below is the real
/// bound; this just caps the pathological case of thousands of one-word comments.
const BATCH_ROWS: usize = 20;
const BATCH_BYTES: usize = 48 * 1024;

/// Mixed into the opening post's public id so it differs from its thread's.
const OPENING_SALT: u32 = 0x8000_0000;

/// Where a story lands, keyed by the fetcher's `hn_kind`.
const SUBSPACES: [(&str, &str); 4] = [
    ("ask", "Ask HN"),
    ("show", "Show HN"),
    ("jobs", "Jobs"),
    ("links", "Links"),
];

struct Options {
    input: String,
    space: String,
    depth_cap: u32,
    max_depth: usize,
    max_stories: usize,
    reset: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            input: String::new(),
            space: "hn".into(),
            depth_cap: 10,
            max_depth: MAX_DEPTH,
            max_stories: 0,
            reset: true,
        }
    }
}

fn usage() -> &'static str {
    "usage: notespace-hn-import <input.ndjson> [options]

  --space KEY        top-level space key (default hn)
  --depth-cap N      indentation cap stored on the spaces (default 10)
  --max-depth N      hard nesting limit, 1..32 (default 32)
  --max-stories N    import at most N stories (default 0 = all)
  --no-reset         append instead of replacing a previous import
"
}

fn parse_args() -> Result<Options, String> {
    let mut opts = Options::default();
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--space" => opts.space = value()?,
            "--depth-cap" => opts.depth_cap = value()?.parse().map_err(|_| "bad --depth-cap")?,
            "--max-depth" => opts.max_depth = value()?.parse().map_err(|_| "bad --max-depth")?,
            "--max-stories" => {
                opts.max_stories = value()?.parse().map_err(|_| "bad --max-stories")?
            }
            "--no-reset" => opts.reset = false,
            "-h" | "--help" => return Err(usage().into()),
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}")),
            _ if opts.input.is_empty() => opts.input = arg,
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    if opts.input.is_empty() {
        return Err(usage().into());
    }
    Ok(opts)
}

fn subspace(hn_kind: &str) -> &'static str {
    match hn_kind {
        "ask" => "ask",
        "show" => "show",
        "job" => "jobs",
        _ => "links",
    }
}

fn thread_kind(hn_kind: &str, has_url: bool) -> &'static str {
    match hn_kind {
        "ask" => "question",
        "job" => "announcement",
        "poll" => "poll",
        _ if has_url => "link",
        _ => "discussion",
    }
}

fn quote(s: &str) -> String {
    s.replace('\'', "''")
}

fn sql_str(s: &str) -> String {
    format!("'{}'", quote(s))
}

fn sql_opt(s: Option<&str>) -> String {
    match s.filter(|v| !v.is_empty()) {
        Some(v) => sql_str(v),
        None => "NULL".into(),
    }
}

/// `MAX_BODY_CHARS` is a rejection in the write path; here it is a truncation, because dropping
/// a long comment would leave a hole in the tree its replies hang off.
fn clip(md: &str) -> String {
    if md.chars().count() <= MAX_BODY_CHARS {
        return md.to_string();
    }
    let mut out: String = md.chars().take(MAX_BODY_CHARS - 2).collect();
    out.push('…');
    out
}

fn public_id(time_secs: i64, hn_id: i64, salt: u32) -> Option<PublicId> {
    let ms = u64::try_from(time_secs.max(0)).ok()? * 1000;
    PublicId::new(ms, (hn_id as u64 as u32) ^ salt).ok()
}

struct Stats {
    stories: usize,
    posts: usize,
    tombstones: usize,
    skipped: usize,
    malformed: usize,
    max_depth: usize,
    oldest: i64,
    newest: i64,
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    match run(&opts) {
        Ok(stats) => {
            eprintln!(
                "imported {} stories, {} posts ({} tombstones), {} skipped, {} malformed\n\
                 deepest reply: {} levels, span {} .. {}",
                stats.stories,
                stats.posts,
                stats.tombstones,
                stats.skipped,
                stats.malformed,
                stats.max_depth + 1,
                stats.oldest,
                stats.newest,
            );
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("hn-import: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(opts: &Options) -> Result<Stats, Box<dyn Error>> {
    let root = opts.space.trim_matches('/').to_string();
    if root.is_empty() {
        return Err("--space cannot be empty".into());
    }
    let file = File::open(&opts.input).map_err(|e| format!("{}: {e}", opts.input))?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let stdout = std::io::stdout();
    let mut out = BufWriter::with_capacity(1 << 20, stdout.lock());

    writeln!(out, "-- Generated by notespace-hn-import. Do not edit.")?;
    writeln!(
        out,
        "-- Bodies are rendered through the real write path (pulldown-cmark + ammonia)."
    )?;
    writeln!(out, "PRAGMA foreign_keys = ON;")?;

    if opts.reset {
        // Scoped to this import: rows below ID_BASE and spaces outside `root/` are untouched.
        let like = sql_str(&format!("{root}/%"));
        writeln!(
            out,
            "DELETE FROM post WHERE id >= {ID_BASE} AND thread_id IN \
             (SELECT id FROM thread WHERE space_path LIKE {like});"
        )?;
        writeln!(
            out,
            "DELETE FROM thread WHERE id >= {ID_BASE} AND space_path LIKE {like};"
        )?;
        writeln!(out, "DELETE FROM space WHERE path LIKE {like};")?;
    }

    writeln!(
        out,
        "INSERT INTO space (name, parent_id, ranking, depth_cap, path) VALUES \
         ('Hacker News', NULL, 'bump', {}, {}) ON CONFLICT(path) DO NOTHING;",
        opts.depth_cap,
        sql_str(&format!("{root}/")),
    )?;
    for (key, name) in SUBSPACES {
        writeln!(
            out,
            "INSERT INTO space (name, parent_id, ranking, depth_cap, path) VALUES \
             ({}, (SELECT id FROM space WHERE path = {}), 'bump', {}, {}) \
             ON CONFLICT(path) DO NOTHING;",
            sql_str(name),
            sql_str(&format!("{root}/")),
            opts.depth_cap,
            sql_str(&format!("{root}/{key}/")),
        )?;
    }

    let mut names = Names::new();
    let mut emitted: HashSet<String> = HashSet::new();
    // Spans the import: a comment on a merged thread is returned under both stories.
    let mut seen_items: HashSet<i64> = HashSet::new();
    // Authors are inserted by name and referenced by a lookup on the unique name index, so an
    // account that already exists -- from an earlier import, or a real signup -- is reused
    // rather than duplicated.
    for name in names.all() {
        let name = name.to_string();
        if emitted.insert(name.clone()) {
            writeln!(out, "{}", user_sql(&name))?;
        }
    }

    let mut stats = Stats {
        stories: 0,
        posts: 0,
        tombstones: 0,
        skipped: 0,
        malformed: 0,
        max_depth: 0,
        oldest: i64::MAX,
        newest: 0,
    };
    let mut thread_id = ID_BASE;
    let mut post_id = ID_BASE;

    for (lineno, line) in reader.lines().enumerate() {
        if opts.max_stories > 0 && stats.stories >= opts.max_stories {
            break;
        }
        let line = line.map_err(|e| format!("line {}: {e}", lineno + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        // A run interrupted mid-write leaves a truncated last line. Losing one story is
        // better than losing the import, so it is counted rather than fatal.
        let story: Item = match serde_json::from_str(&line) {
            Ok(story) => story,
            Err(e) => {
                if stats.malformed == 0 {
                    eprintln!("{}:{}: {e}", opts.input, lineno + 1);
                }
                stats.malformed += 1;
                continue;
            }
        };

        let title = markdown::decode_entities(story.title.as_deref().unwrap_or(""));
        let placed = tree::place(&story, opts.max_depth, &mut seen_items);
        let thread_public = public_id(story.created_at(), story.id, 0);
        let (Some(thread_public), false, false) =
            (thread_public, title.trim().is_empty(), placed.is_empty())
        else {
            stats.skipped += 1;
            continue;
        };

        thread_id += 1;
        let hn_kind = story.hn_kind.as_deref().unwrap_or("story");
        let space_path = format!("{root}/{}/", subspace(hn_kind));

        // Every author this story introduces, ahead of the rows that reference them.
        let author = ensure_user(&mut names, &mut emitted, &mut out, story.author.as_deref())?;
        let mut post_authors: Vec<String> = Vec::with_capacity(placed.len());
        for p in &placed {
            post_authors.push(ensure_user(
                &mut names,
                &mut emitted,
                &mut out,
                p.item.author.as_deref(),
            )?);
        }

        let bumped = placed
            .iter()
            .map(|p| p.item.created_at())
            .chain(std::iter::once(story.created_at()))
            .max()
            .unwrap_or_default();
        let score = story.points.unwrap_or(0) as f64;
        writeln!(
            out,
            "INSERT INTO thread (id, public_id, space_id, space_path, kind, title, url, \
             author_id, created_at, bumped_at, post_count, score, rank, state, cache_version) \
             VALUES ({thread_id}, {}, (SELECT id FROM space WHERE path = {}), {}, {}, {}, {}, \
             (SELECT id FROM user WHERE name = {}), {}, {}, {}, {score}, {score}, 'visible', 0);",
            sql_str(thread_public.as_str()),
            sql_str(&space_path),
            sql_str(&space_path),
            sql_str(thread_kind(hn_kind, story.url.is_some())),
            sql_str(&title),
            sql_opt(story.url.as_deref()),
            sql_str(&author),
            story.created_at(),
            bumped,
            placed.len(),
        )?;

        let base_post_id = post_id;
        let mut batch: Vec<String> = Vec::new();
        let mut batch_bytes = 0usize;
        for (i, p) in placed.iter().enumerate() {
            post_id += 1;
            stats.posts += 1;
            stats.max_depth = stats.max_depth.max(p.path.depth());
            let created = match p.item.created_at() {
                0 => story.created_at(),
                t => t,
            };
            stats.oldest = stats.oldest.min(created);
            stats.newest = stats.newest.max(created);

            // The opening post shares the story's HN id, so it is salted apart: otherwise
            // /p/<id> and /t/<id> would read as the same id.
            let salt = if p.is_opening { OPENING_SALT } else { 0 };
            let public = public_id(created, p.item.id, salt)
                .map(|id| id.encode())
                // Only an out-of-range timestamp reaches here; the row id keeps it unique.
                .unwrap_or_else(|| format!("hnfallback{:06}", post_id % 1_000_000));

            let tombstone = p.item.is_tombstone();
            if tombstone {
                stats.tombstones += 1;
            }
            let (md, html, state) = if tombstone {
                (String::new(), String::new(), "deleted")
            } else {
                let md = clip(&markdown::to_markdown(p.item.text.as_deref().unwrap_or("")));
                let html = markdown_to_html(&md);
                (md, html, "visible")
            };

            let row = format!(
                "({post_id}, {}, {thread_id}, {}, {}, {}, (SELECT id FROM user WHERE name = {}), \
                 {}, {}, {created}, 0, '{state}')",
                sql_str(&public),
                p.parent
                    .map(|idx| (base_post_id + 1 + idx as i64).to_string())
                    .unwrap_or_else(|| "NULL".into()),
                sql_str(p.path.as_str()),
                p.path.depth(),
                sql_str(&post_authors[i]),
                sql_str(&md),
                sql_str(&html),
            );
            batch_bytes += row.len();
            batch.push(row);
            if batch.len() >= BATCH_ROWS || batch_bytes >= BATCH_BYTES {
                writeln!(out, "{}", post_insert(&batch))?;
                batch.clear();
                batch_bytes = 0;
            }
        }
        if !batch.is_empty() {
            writeln!(out, "{}", post_insert(&batch))?;
        }
        stats.stories += 1;
    }

    out.flush()?;
    if stats.oldest == i64::MAX {
        stats.oldest = 0;
    }
    eprintln!(
        "{} distinct authors, {} names rewritten",
        emitted.len(),
        names.rewritten
    );
    Ok(stats)
}

fn ensure_user(
    names: &mut Names,
    emitted: &mut HashSet<String>,
    out: &mut impl Write,
    hn: Option<&str>,
) -> std::io::Result<String> {
    let name = names.resolve(hn.unwrap_or(""));
    if emitted.insert(name.clone()) {
        writeln!(out, "{}", user_sql(&name))?;
    }
    Ok(name)
}

fn post_insert(rows: &[String]) -> String {
    format!(
        "INSERT INTO post (id, public_id, thread_id, parent_id, path, depth, author_id, \
         body_md, body_html, created_at, score, state) VALUES\n{};",
        rows.join(",\n")
    )
}

fn user_sql(name: &str) -> String {
    // Fixed timestamp: an imported account has no signup date, and inventing one would put a
    // fake number in a column the UI shows.
    format!(
        "INSERT INTO user (name, created_at, state) VALUES ({}, 0, 'active') \
         ON CONFLICT(name) DO NOTHING;",
        sql_str(name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_an_apostrophe() {
        assert_eq!(sql_str("it's"), "'it''s'");
    }

    #[test]
    fn a_body_past_the_write_paths_limit_is_clipped_not_dropped() {
        let long = "a".repeat(MAX_BODY_CHARS + 500);
        let out = clip(&long);
        assert_eq!(out.chars().count(), MAX_BODY_CHARS - 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn the_opening_post_and_its_thread_get_different_public_ids() {
        let thread = public_id(1_700_000_000, 42, 0).unwrap();
        let opening = public_id(1_700_000_000, 42, OPENING_SALT).unwrap();
        assert_ne!(thread.encode(), opening.encode());
    }

    #[test]
    fn stories_land_in_the_space_their_tag_implies() {
        assert_eq!(subspace("ask"), "ask");
        assert_eq!(subspace("show"), "show");
        assert_eq!(subspace("job"), "jobs");
        assert_eq!(subspace("story"), "links");
        assert_eq!(subspace("whatever"), "links");
    }

    #[test]
    fn a_story_with_a_url_is_a_link_thread() {
        assert_eq!(thread_kind("story", true), "link");
        assert_eq!(thread_kind("story", false), "discussion");
        assert_eq!(thread_kind("ask", false), "question");
    }
}
