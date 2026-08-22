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
use notespace_core::store::{
    async_trait, NextPath, Page, PostLocation, Store, StoreError, StoreResult,
};
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

    async fn insert_post(&self, new: &NewPost) -> StoreResult<Post> {
        let tx = self.conn.unchecked_transaction().map_err(backend)?;

        let (thread_id, _count): (i64, i64) = tx
            .query_row(sql::THREAD_ROW, [new.thread.as_str()], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
            .map_err(backend)?
            .ok_or(StoreError::NotFound)?;

        let parent = match &new.parent {
            None => None,
            Some(pid) => {
                let row: Option<(i64, String, i64)> = tx
                    .query_row(
                        sql::POST_IN_THREAD,
                        rusqlite::params![pid.as_str(), thread_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(backend)?;
                let (id, path, _depth) = row.ok_or(StoreError::NotFound)?;
                Some((
                    id,
                    Path::parse(&path).map_err(|e| corrupt("parent path", e))?,
                ))
            }
        };

        let parent_path = parent.as_ref().map(|(_, p)| p.clone());
        let (lo, hi) = NextPath::search_bounds(parent_path.as_ref());
        let last: Option<String> = tx
            .query_row(
                sql::LAST_PATH_IN_RANGE,
                rusqlite::params![thread_id, lo, hi],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        let last = last
            .map(|p| Path::parse(&p).map_err(|e| corrupt("sibling path", e)))
            .transpose()?;
        let path = NextPath::allocate(parent_path.as_ref(), last.as_ref())?;

        let depth = path.depth() as i64;
        let inserted = tx.execute(
            sql::INSERT_POST,
            rusqlite::params![
                new.public_id.as_str(),
                thread_id,
                parent.as_ref().map(|(id, _)| *id),
                path.as_str(),
                depth,
                new.author_id,
                new.body_md,
                new.body_html.as_str(),
                new.created_at,
            ],
        );
        match inserted {
            Ok(_) => {}
            // Only a UNIQUE violation means "somebody else took this"; a NOT NULL or foreign
            // key failure is a bug in the caller and must not be retried forever as a race.
            Err(rusqlite::Error::SqliteFailure(e, msg))
                if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                    || e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY =>
            {
                let _ = msg;
                return Err(StoreError::Conflict);
            }
            Err(e) => return Err(backend(e)),
        }
        let id = tx.last_insert_rowid();
        tx.execute(
            sql::BUMP_THREAD,
            rusqlite::params![thread_id, new.created_at],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;

        Ok(Post {
            id,
            public_id: new.public_id.clone(),
            thread_id,
            parent_id: parent.map(|(id, _)| id),
            path,
            depth: depth as u32,
            author_id: new.author_id,
            author_name: String::new(),
            body_md: Some(new.body_md.clone()),
            body_html: new.body_html.as_str().to_string(),
            created_at: new.created_at,
            edited_at: None,
            score: 0.0,
            state: PostState::Visible,
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
