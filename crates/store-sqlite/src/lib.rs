//! Native [`Store`] over plain SQLite.
//!
//! The second implementation of the seam, and the reason the first one can be trusted. A trait
//! with one implementation is an interface, not a seam: nothing stops it drifting toward the
//! quirks of the only thing behind it. This adapter and
//! [`notespace_core::conformance`] together are what make the dual-target promise in
//! DESIGN.md §3.1 checkable rather than merely stated.
//!
//! It also makes the read path testable in an ordinary `cargo test`. Exercising the D1 adapter
//! means building wasm and booting wrangler; this runs in milliseconds against an in-memory
//! database.
//!
//! The SQL is not written here — it is [`notespace_core::sql`], byte for byte the same
//! statements the D1 adapter sends. What differs is binding and row decoding.
//!
//! Blocking calls inside `async fn` are deliberate. `rusqlite` is synchronous, and against a
//! local file or `:memory:` there is nothing to await. The `Store` trait is `?Send` because
//! wasm futures are not `Send`, which suits a synchronous body fine.

use notespace_core::id::PublicId;
use notespace_core::model::*;
use notespace_core::path::Path;
use notespace_core::sql;
use notespace_core::store::{async_trait, Page, PostLocation, Store, StoreError, StoreResult};
use rusqlite::{Connection, OptionalExtension, Row};

pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// An in-memory database with the migrations applied.
    pub fn in_memory(migrations: &[&str]) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        for m in migrations {
            conn.execute_batch(m)?;
        }
        Ok(Self::new(conn))
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

fn backend(e: impl core::fmt::Display) -> StoreError {
    StoreError::Backend(e.to_string())
}

fn corrupt(what: &str, e: impl core::fmt::Display) -> StoreError {
    StoreError::Corrupt(format!("{what}: {e}"))
}

fn thread_from_row(row: &Row<'_>) -> rusqlite::Result<(Space, Thread)> {
    let space = Space {
        id: row.get("space_id")?,
        path: row.get("space_path")?,
        name: row.get("space_name")?,
        parent_id: None,
        ranking: parse_enum(&row.get::<_, String>("space_ranking")?),
        depth_cap: row.get::<_, i64>("space_depth_cap")? as u32,
    };
    let thread = Thread {
        id: row.get("id")?,
        public_id: PublicId::parse(&row.get::<_, String>("public_id")?)
            .map_err(|e| rusqlite::Error::InvalidColumnName(format!("public_id: {e}")))?,
        space_id: row.get("space_id")?,
        kind: parse_enum(&row.get::<_, String>("kind")?),
        title: row.get("title")?,
        url: row.get("url")?,
        author_id: row.get("author_id")?,
        author_name: row.get("author_name")?,
        created_at: row.get("created_at")?,
        bumped_at: row.get("bumped_at")?,
        post_count: row.get::<_, i64>("post_count")? as u32,
        state: parse_enum(&row.get::<_, String>("state")?),
        cache_version: row.get("cache_version")?,
    };
    Ok((space, thread))
}

/// Unknown enum values fall back to the type's `Default` rather than failing the request.
///
/// A row written by a newer version with a state this build does not know is a reason to render
/// conservatively, not to 500 the page. Matches what the D1 adapter does.
fn parse_enum<T: Default + core::str::FromStr>(s: &str) -> T {
    s.parse().unwrap_or_default()
}

#[async_trait(?Send)]
impl Store for SqliteStore {
    async fn thread_page(&self, thread: &PublicId, page: &Page) -> StoreResult<ThreadPage> {
        let (space, thread_row) = self
            .conn
            .query_row(sql::THREAD, [thread.as_str()], thread_from_row)
            .optional()
            .map_err(backend)?
            .ok_or(StoreError::NotFound)?;

        let cursor = page
            .after
            .as_ref()
            .map(|p| p.as_str())
            .unwrap_or(sql::PATH_START);
        // Over-fetch by one to detect "is there a next page?" without a second COUNT.
        let fetch = page.limit.saturating_add(1);

        let mut stmt = self.conn.prepare_cached(sql::POSTS).map_err(backend)?;
        let rows = stmt
            .query_map(
                rusqlite::params![thread.as_str(), cursor, fetch],
                |r| -> rusqlite::Result<Post> {
                    Ok(Post {
                        id: r.get("id")?,
                        public_id: PublicId::parse(&r.get::<_, String>("public_id")?).map_err(
                            |e| rusqlite::Error::InvalidColumnName(format!("public_id: {e}")),
                        )?,
                        thread_id: r.get("thread_id")?,
                        parent_id: r.get("parent_id")?,
                        path: Path::parse(&r.get::<_, String>("path")?).map_err(|e| {
                            rusqlite::Error::InvalidColumnName(format!("path: {e}"))
                        })?,
                        depth: r.get::<_, i64>("depth")? as u32,
                        author_id: r.get("author_id")?,
                        author_name: r.get("author_name")?,
                        // Never selected on the read path; see notespace_core::sql::POSTS.
                        body_md: None,
                        body_html: r.get("body_html")?,
                        created_at: r.get("created_at")?,
                        edited_at: r.get("edited_at")?,
                        score: r.get("score")?,
                        state: parse_enum(&r.get::<_, String>("state")?),
                    })
                },
            )
            .map_err(backend)?;

        let mut posts = Vec::new();
        for row in rows {
            posts.push(row.map_err(|e| corrupt("post row", e))?);
        }

        // The over-fetched row is the proof there is a next page; it is not part of this one.
        let next_cursor = if posts.len() as u32 > page.limit {
            posts.truncate(page.limit as usize);
            posts.last().map(|p| p.path.clone())
        } else {
            None
        };

        Ok(ThreadPage {
            space,
            thread: thread_row,
            posts,
            next_cursor,
        })
    }

    async fn locate_post(&self, post: &PublicId) -> StoreResult<PostLocation> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(sql::LOCATE_POST, [post.as_str()], |r| {
                Ok((r.get("thread_public_id")?, r.get("post_path")?))
            })
            .optional()
            .map_err(backend)?;
        let (thread, path) = row.ok_or(StoreError::NotFound)?;
        Ok(PostLocation {
            thread: PublicId::parse(&thread).map_err(|e| corrupt("thread public_id", e))?,
            path: Path::parse(&path).map_err(|e| corrupt("post path", e))?,
        })
    }
}
