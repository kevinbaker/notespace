//! Native [`Store`] over plain SQLite: the second implementation of the seam, which is what
//! makes the dual-target promise checkable. It also puts the read path inside an ordinary
//! `cargo test`, where the D1 adapter needs wasm and wrangler.
//!
//! SQL comes from [`notespace_core::sql`], byte for byte what D1 gets; binding and row decoding
//! are what differ. The blocking calls inside `async fn` are fine: `rusqlite` is synchronous and
//! `Store` is `?Send` anyway, because wasm futures are not `Send`.

use notespace_core::email::{ConsumedToken, EmailAddress, StoredToken, TokenKind};
use notespace_core::id::PublicId;
use notespace_core::model::*;
use notespace_core::moderation::{
    classify::Call, ActorKind, AgreementStats, Category, LogEntry, NewAction, NewReview, NewSignal,
    ReportTally, Resolution, ReviewItem, ReviewPost, ReviewReason, WriteContext,
};
use notespace_core::path::Path;
use notespace_core::ratelimit::{AttemptKeys, Attempts};
use notespace_core::session::{Session, TokenHash};
use notespace_core::space_key::SpacePath;
use notespace_core::sql;
use notespace_core::store::{
    async_trait, Authenticated, Credential, NextPath, Page, PostLocation, Store, StoreError,
    StoreResult,
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

/// Unknown values fall back to `Default`, as in the D1 adapter, rather than 500ing the page.
fn parse_enum<T: Default + core::str::FromStr>(s: &str) -> T {
    s.parse().unwrap_or_default()
}

/// One row of the post read path.
fn post_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Post> {
    Ok(Post {
        id: r.get("id")?,
        public_id: PublicId::parse(&r.get::<_, String>("public_id")?)
            .map_err(|e| rusqlite::Error::InvalidColumnName(format!("public_id: {e}")))?,
        thread_id: r.get("thread_id")?,
        parent_id: r.get("parent_id")?,
        path: Path::parse(&r.get::<_, String>("path")?)
            .map_err(|e| rusqlite::Error::InvalidColumnName(format!("path: {e}")))?,
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
}

#[async_trait(?Send)]
impl Store for SqliteStore {
    async fn recent_threads(&self, limit: u32) -> StoreResult<Vec<ThreadSummary>> {
        let mut stmt = self
            .conn
            .prepare_cached(sql::RECENT_THREADS)
            .map_err(backend)?;
        let rows = stmt
            .query_map(rusqlite::params![limit], summary_from_row)
            .map_err(backend)?;
        collect(rows, "thread row")
    }

    async fn space_threads(
        &self,
        space: &SpacePath,
        limit: u32,
    ) -> StoreResult<Vec<ThreadSummary>> {
        let (lo, hi) = space.subtree_range();
        let mut stmt = self
            .conn
            .prepare_cached(sql::SPACE_THREADS)
            .map_err(backend)?;
        let rows = stmt
            .query_map(rusqlite::params![lo, hi, limit], summary_from_row)
            .map_err(backend)?;
        collect(rows, "thread row")
    }

    async fn space_by_path(&self, path: &SpacePath) -> StoreResult<Option<Space>> {
        self.conn
            .query_row(sql::SPACE_BY_PATH, [path.as_stored()], space_from_row)
            .optional()
            .map_err(backend)
    }

    async fn spaces_under(&self, parent: Option<SpaceId>) -> StoreResult<Vec<Space>> {
        let mut stmt = self
            .conn
            .prepare_cached(sql::SPACES_UNDER)
            .map_err(backend)?;
        let rows = stmt
            .query_map(rusqlite::params![parent], space_from_row)
            .map_err(backend)?;
        collect(rows, "space row")
    }

    async fn space_context(&self, space: SpaceId, author: UserId) -> StoreResult<WriteContext> {
        self.conn
            .query_row(
                sql::SPACE_CONTEXT,
                rusqlite::params![space, author],
                write_context_from_row,
            )
            .optional()
            .map_err(backend)?
            .ok_or(StoreError::NotFound)
    }

    async fn create_thread(&self, t: &NewThread) -> StoreResult<Thread> {
        let inserted = self.conn.execute(
            sql::INSERT_THREAD,
            rusqlite::params![
                t.public_id.as_str(),
                t.space_id,
                t.space_path,
                t.kind.as_str(),
                t.title,
                t.url,
                t.author_id,
                t.created_at,
            ],
        );
        match inserted {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                return Err(StoreError::Conflict)
            }
            Err(e) => return Err(backend(e)),
        }
        Ok(Thread {
            id: self.conn.last_insert_rowid(),
            public_id: t.public_id.clone(),
            space_id: t.space_id,
            kind: t.kind,
            title: t.title.clone(),
            url: t.url.clone(),
            author_id: t.author_id,
            author_name: String::new(),
            created_at: t.created_at,
            bumped_at: t.created_at,
            post_count: 0,
            state: ThreadState::Visible,
            cache_version: 0,
        })
    }

    async fn update_post_body(
        &self,
        post: &PublicId,
        body_md: &str,
        body_html: &SanitizedHtml,
        edited_at: Timestamp,
    ) -> StoreResult<()> {
        let tx = self.conn.unchecked_transaction().map_err(backend)?;
        let changed = tx
            .execute(
                sql::UPDATE_POST_BODY,
                rusqlite::params![post.as_str(), body_md, body_html.as_str(), edited_at],
            )
            .map_err(backend)?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        tx.execute(sql::BUMP_THREAD_FOR_POST, [post.as_str()])
            .map_err(backend)?;
        tx.commit().map_err(backend)
    }

    async fn user_profile(&self, name: &str, limit: u32) -> StoreResult<Option<Profile>> {
        let head: Option<(User, Timestamp)> = self
            .conn
            .query_row(sql::USER_PROFILE, [name], |r| {
                Ok((user_from_row(r)?, r.get("created_at")?))
            })
            .optional()
            .map_err(backend)?;
        let Some((user, created_at)) = head else {
            return Ok(None);
        };
        let mut stmt = self
            .conn
            .prepare_cached(sql::USER_RECENT_POSTS)
            .map_err(backend)?;
        let rows = stmt
            .query_map(rusqlite::params![user.id, limit], |r| {
                Ok(ProfilePost {
                    public_id: parse_id(r, "public_id")?,
                    thread_public_id: parse_id(r, "thread_public_id")?,
                    thread_title: r.get("thread_title")?,
                    body_html: r.get("body_html")?,
                    created_at: r.get("created_at")?,
                })
            })
            .map_err(backend)?;
        Ok(Some(Profile {
            user,
            created_at,
            posts: collect(rows, "profile post")?,
        }))
    }

    // -- Email ----------------------------------------------------------------

    async fn account(&self, user: UserId) -> StoreResult<Option<Account>> {
        self.conn
            .query_row(sql::ACCOUNT, [user], |r| {
                Ok(Account {
                    email: r.get("email")?,
                    email_verified_at: r.get("email_verified_at")?,
                    has_password: r.get::<_, i64>("has_password")? != 0,
                })
            })
            .optional()
            .map_err(backend)
    }

    async fn user_by_verified_email(&self, email: &str) -> StoreResult<Option<User>> {
        self.conn
            .query_row(sql::USER_BY_VERIFIED_EMAIL, [email], user_from_row)
            .optional()
            .map_err(backend)
    }

    async fn set_email(&self, user: UserId, email: Option<&str>) -> StoreResult<()> {
        self.conn
            .execute(sql::SET_EMAIL, rusqlite::params![user, email])
            .map_err(backend)?;
        Ok(())
    }

    async fn mark_email_verified(
        &self,
        user: UserId,
        email: &str,
        now: Timestamp,
    ) -> StoreResult<bool> {
        match self.conn.execute(
            sql::MARK_EMAIL_VERIFIED,
            rusqlite::params![user, email, now],
        ) {
            Ok(n) => Ok(n > 0),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                Err(StoreError::Conflict)
            }
            Err(e) => Err(backend(e)),
        }
    }

    async fn create_email_token(&self, t: &StoredToken) -> StoreResult<()> {
        self.conn
            .execute(
                sql::INSERT_EMAIL_TOKEN,
                rusqlite::params![
                    t.token_hash,
                    t.user_id,
                    t.kind.as_str(),
                    t.email.as_str(),
                    t.created_at,
                    t.expires_at,
                ],
            )
            .map_err(backend)?;
        Ok(())
    }

    async fn consume_email_token(
        &self,
        token_hash: &str,
        kind: TokenKind,
        now: Timestamp,
    ) -> StoreResult<Option<ConsumedToken>> {
        self.conn
            .query_row(
                sql::CONSUME_EMAIL_TOKEN,
                rusqlite::params![token_hash, kind.as_str(), now],
                |r| Ok((r.get::<_, i64>("user_id")?, r.get::<_, String>("email")?)),
            )
            .optional()
            .map_err(backend)?
            .map(|(user_id, email)| {
                Ok(ConsumedToken {
                    user_id,
                    email: EmailAddress::parse(&email).map_err(|e| corrupt("token email", e))?,
                })
            })
            .transpose()
    }

    async fn peek_email_token(
        &self,
        token_hash: &str,
        kind: TokenKind,
        now: Timestamp,
    ) -> StoreResult<Option<String>> {
        self.conn
            .query_row(
                sql::PEEK_EMAIL_TOKEN,
                rusqlite::params![token_hash, kind.as_str(), now],
                |r| r.get("name"),
            )
            .optional()
            .map_err(backend)
    }

    async fn retire_email_tokens(
        &self,
        user: UserId,
        kind: TokenKind,
        now: Timestamp,
    ) -> StoreResult<u32> {
        let n = self
            .conn
            .execute(
                sql::RETIRE_EMAIL_TOKENS,
                rusqlite::params![user, kind.as_str(), now],
            )
            .map_err(backend)?;
        Ok(n as u32)
    }

    async fn thread_version(&self, thread: &PublicId) -> StoreResult<Option<i64>> {
        self.conn
            .query_row(sql::THREAD_VERSION, [thread.as_str()], |r| r.get(0))
            .optional()
            .map_err(backend)
    }

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
                post_from_row,
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
                new.state.as_str(),
            ],
        );
        match inserted {
            Ok(_) => {}
            // Only a UNIQUE violation is a race; the rest are caller bugs, not things to retry.
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
            state: new.state,
        })
    }

    async fn create_session(&self, session: &Session) -> StoreResult<()> {
        self.conn
            .execute(
                sql::INSERT_SESSION,
                rusqlite::params![
                    session.token_hash.as_str(),
                    session.user_id,
                    session.created_at,
                    session.refreshed_at,
                    session.expires_at,
                ],
            )
            .map_err(backend)?;
        Ok(())
    }

    async fn lookup_session(
        &self,
        token: &TokenHash,
        now: Timestamp,
    ) -> StoreResult<Option<Authenticated>> {
        self.conn
            .query_row(
                sql::LOOKUP_SESSION,
                rusqlite::params![token.as_str(), now],
                |r| {
                    Ok(Authenticated {
                        session: Session {
                            token_hash: token.clone(),
                            user_id: r.get("user_id")?,
                            created_at: r.get("created_at")?,
                            refreshed_at: r.get("refreshed_at")?,
                            expires_at: r.get("expires_at")?,
                        },
                        user: User {
                            id: r.get("user_id")?,
                            name: r.get("user_name")?,
                            state: parse_enum(&r.get::<_, String>("user_state")?),
                            role: parse_enum(&r.get::<_, String>("user_role")?),
                        },
                    })
                },
            )
            .optional()
            .map_err(backend)
    }

    async fn refresh_session(
        &self,
        token: &TokenHash,
        refreshed_at: Timestamp,
        expires_at: Timestamp,
    ) -> StoreResult<()> {
        self.conn
            .execute(
                sql::REFRESH_SESSION,
                rusqlite::params![token.as_str(), refreshed_at, expires_at],
            )
            .map_err(backend)?;
        Ok(())
    }

    async fn delete_session(&self, token: &TokenHash) -> StoreResult<()> {
        self.conn
            .execute(sql::DELETE_SESSION, [token.as_str()])
            .map_err(backend)?;
        Ok(())
    }

    async fn delete_user_sessions(&self, user: UserId) -> StoreResult<u32> {
        let n = self
            .conn
            .execute(sql::DELETE_USER_SESSIONS, [user])
            .map_err(backend)?;
        Ok(n as u32)
    }

    async fn user_by_name(&self, name: &str) -> StoreResult<Option<Credential>> {
        self.conn
            .query_row(sql::USER_BY_NAME, [name], |r| {
                Ok(Credential {
                    user: User {
                        id: r.get("id")?,
                        name: r.get("name")?,
                        state: parse_enum(&r.get::<_, String>("state")?),
                        role: parse_enum(&r.get::<_, String>("role")?),
                    },
                    password_hash: r.get("password_hash")?,
                })
            })
            .optional()
            .map_err(backend)
    }

    async fn user_by_id(&self, id: UserId) -> StoreResult<Option<User>> {
        self.conn
            .query_row(sql::USER_BY_ID, [id], user_from_row)
            .optional()
            .map_err(backend)
    }

    async fn create_user(
        &self,
        name: &str,
        created_at: Timestamp,
        password_hash: Option<&str>,
    ) -> StoreResult<UserId> {
        match self.conn.execute(
            sql::INSERT_USER,
            rusqlite::params![name, created_at, password_hash],
        ) {
            Ok(_) => Ok(self.conn.last_insert_rowid()),
            // Registration's check and this insert are not atomic, so the index is the guard.
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                Err(StoreError::Conflict)
            }
            Err(e) => Err(backend(e)),
        }
    }

    async fn set_password_hash(&self, user: UserId, hash: &str) -> StoreResult<()> {
        self.conn
            .execute(sql::SET_PASSWORD_HASH, rusqlite::params![user, hash])
            .map_err(backend)?;
        Ok(())
    }

    async fn login_attempts(
        &self,
        keys: &AttemptKeys,
    ) -> StoreResult<(Option<Attempts>, Option<Attempts>)> {
        let mut stmt = self
            .conn
            .prepare_cached(sql::LOGIN_ATTEMPTS)
            .map_err(backend)?;
        let rows = stmt
            .query_map(
                rusqlite::params![keys.identity.as_str(), keys.client.as_str()],
                |r| {
                    Ok((
                        r.get::<_, String>("key")?,
                        Attempts {
                            window_start: r.get("window_start")?,
                            count: r.get::<_, i64>("count")? as u32,
                        },
                    ))
                },
            )
            .map_err(backend)?;
        let (mut identity, mut client) = (None, None);
        for row in rows {
            let (key, a) = row.map_err(backend)?;
            if key == keys.identity {
                identity = Some(a);
            } else if key == keys.client {
                client = Some(a);
            }
        }
        Ok((identity, client))
    }

    async fn record_login_attempt(&self, key: &str, attempts: Attempts) -> StoreResult<()> {
        self.conn
            .execute(
                sql::RECORD_LOGIN_ATTEMPT,
                rusqlite::params![key, attempts.window_start, attempts.count],
            )
            .map_err(backend)?;
        Ok(())
    }

    async fn clear_login_attempts(&self, key: &str) -> StoreResult<()> {
        self.conn
            .execute(sql::CLEAR_LOGIN_ATTEMPTS, [key])
            .map_err(backend)?;
        Ok(())
    }

    async fn sweep_login_attempts(&self, cutoff: Timestamp) -> StoreResult<u32> {
        let n = self
            .conn
            .execute(sql::SWEEP_LOGIN_ATTEMPTS, [cutoff])
            .map_err(backend)?;
        Ok(n as u32)
    }

    async fn locate_post(&self, post: &PublicId, page_size: u32) -> StoreResult<PostLocation> {
        let row: Option<(String, i64, String)> = self
            .conn
            .query_row(sql::LOCATE_POST, [post.as_str()], |r| {
                Ok((
                    r.get("thread_public_id")?,
                    r.get("thread_row_id")?,
                    r.get("post_path")?,
                ))
            })
            .optional()
            .map_err(backend)?;
        let (thread, thread_row, path) = row.ok_or(StoreError::NotFound)?;

        let rank: i64 = self
            .conn
            .query_row(sql::POST_RANK, rusqlite::params![thread_row, &path], |r| {
                r.get("rank")
            })
            .map_err(backend)?;
        let cursor = match notespace_core::store::page_cursor_offset(rank, page_size) {
            None => None,
            Some(offset) => {
                let at: Option<String> = self
                    .conn
                    .query_row(
                        sql::PATH_AT_OFFSET,
                        rusqlite::params![thread_row, offset],
                        |r| r.get("path"),
                    )
                    .optional()
                    .map_err(backend)?;
                match at {
                    Some(p) => Some(Path::parse(&p).map_err(|e| corrupt("cursor path", e))?),
                    None => None,
                }
            }
        };

        Ok(PostLocation {
            thread: PublicId::parse(&thread).map_err(|e| corrupt("thread public_id", e))?,
            path: Path::parse(&path).map_err(|e| corrupt("post path", e))?,
            cursor,
        })
    }

    // -- Moderation ---------------------------------------------------------

    async fn write_context(&self, thread: &PublicId, author: UserId) -> StoreResult<WriteContext> {
        self.conn
            .query_row(
                sql::WRITE_CONTEXT,
                rusqlite::params![thread.as_str(), author],
                write_context_from_row,
            )
            .optional()
            .map_err(backend)?
            .ok_or(StoreError::NotFound)
    }

    async fn author_posted_recently(
        &self,
        author: UserId,
        body_md: &str,
        since: Timestamp,
    ) -> StoreResult<bool> {
        let found: Option<i64> = self
            .conn
            .query_row(
                sql::AUTHOR_POSTED_RECENTLY,
                rusqlite::params![author, body_md, since],
                |r| r.get("found"),
            )
            .optional()
            .map_err(backend)?;
        Ok(found.is_some())
    }

    async fn post_for_review(&self, post: &PublicId) -> StoreResult<ReviewPost> {
        self.conn
            .query_row(sql::POST_FOR_REVIEW, [post.as_str()], |r| {
                Ok(ReviewPost {
                    id: r.get("id")?,
                    public_id: parse_id(r, "public_id")?,
                    thread_public_id: parse_id(r, "thread_public_id")?,
                    thread_title: r.get("thread_title")?,
                    space_id: r.get("space_id")?,
                    space_name: r.get("space_name")?,
                    space_config: r.get("space_config")?,
                    author_id: r.get("author_id")?,
                    author_name: r.get("author_name")?,
                    author_created_at: r.get("author_created_at")?,
                    body_md: r.get("body_md")?,
                    created_at: r.get("created_at")?,
                    state: parse_enum(&r.get::<_, String>("state")?),
                })
            })
            .optional()
            .map_err(backend)?
            .ok_or(StoreError::NotFound)
    }

    async fn set_post_state(
        &self,
        post: &PublicId,
        state: PostState,
        _now: Timestamp,
    ) -> StoreResult<()> {
        let tx = self.conn.unchecked_transaction().map_err(backend)?;
        let changed = tx
            .execute(
                sql::SET_POST_STATE,
                rusqlite::params![post.as_str(), state.as_str()],
            )
            .map_err(backend)?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        tx.execute(sql::BUMP_THREAD_FOR_POST, [post.as_str()])
            .map_err(backend)?;
        tx.commit().map_err(backend)
    }

    async fn log_action(&self, a: &NewAction) -> StoreResult<i64> {
        self.conn
            .execute(
                sql::INSERT_ACTION,
                rusqlite::params![
                    a.actor_kind.as_str(),
                    a.actor_id,
                    a.actor_name,
                    a.target_kind,
                    a.target_id,
                    a.action,
                    a.detail.to_string(),
                    a.public as i64,
                    a.created_at,
                ],
            )
            .map_err(backend)?;
        Ok(self.conn.last_insert_rowid())
    }

    async fn open_review(&self, review: &NewReview) -> StoreResult<()> {
        let (verdict, confidence, categories) = match &review.verdict {
            Some(v) => (
                Some(v.call.as_str()),
                Some(v.confidence),
                Some(serde_json::to_string(&v.categories).map_err(backend)?),
            ),
            None => (None, None, None),
        };
        self.conn
            .execute(
                sql::UPSERT_REVIEW,
                rusqlite::params![
                    review.post_id,
                    review.space_id,
                    review.reason.as_str(),
                    verdict,
                    confidence,
                    categories,
                    review.appeal_text,
                    review.opened_at,
                ],
            )
            .map_err(backend)?;
        Ok(())
    }

    async fn resolve_review(
        &self,
        id: i64,
        resolution: Resolution,
        by: UserId,
        now: Timestamp,
    ) -> StoreResult<Option<ReviewItem>> {
        let tx = self.conn.unchecked_transaction().map_err(backend)?;
        let item = tx
            .query_row(sql::REVIEW_BY_ID, [id], review_from_row)
            .optional()
            .map_err(backend)?;
        let Some(item) = item.filter(|i| !i.resolved) else {
            return Ok(None);
        };
        let changed = tx
            .execute(
                sql::RESOLVE_REVIEW,
                rusqlite::params![id, resolution.as_str(), by, now],
            )
            .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok((changed > 0).then_some(item))
    }

    async fn open_reviews(&self, limit: u32) -> StoreResult<Vec<ReviewItem>> {
        let mut stmt = self
            .conn
            .prepare_cached(sql::OPEN_REVIEWS)
            .map_err(backend)?;
        let rows = stmt.query_map([limit], review_from_row).map_err(backend)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| corrupt("review row", e))?);
        }
        Ok(out)
    }

    async fn add_report(&self, sig: &NewSignal) -> StoreResult<ReportTally> {
        let tx = self.conn.unchecked_transaction().map_err(backend)?;
        let added = tx
            .execute(
                sql::INSERT_SIGNAL,
                rusqlite::params![
                    sig.post_id,
                    sig.user_id,
                    sig.kind,
                    sig.weight,
                    sig.reason,
                    sig.created_at
                ],
            )
            .map_err(backend)?
            > 0;
        let count: i64 = tx
            .query_row(
                sql::COUNT_SIGNALS,
                rusqlite::params![sig.post_id, sig.kind],
                |r| r.get("n"),
            )
            .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(ReportTally {
            added,
            count: count.max(0) as u32,
        })
    }

    async fn pending_posts(&self, older_than: Timestamp, limit: u32) -> StoreResult<Vec<PublicId>> {
        let mut stmt = self
            .conn
            .prepare_cached(sql::PENDING_POSTS)
            .map_err(backend)?;
        let rows = stmt
            .query_map(rusqlite::params![older_than, limit], |r| {
                parse_id(r, "public_id")
            })
            .map_err(backend)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| corrupt("pending post", e))?);
        }
        Ok(out)
    }

    async fn public_log(&self, limit: u32) -> StoreResult<Vec<LogEntry>> {
        let mut stmt = self.conn.prepare_cached(sql::PUBLIC_LOG).map_err(backend)?;
        let rows = stmt
            .query_map([limit], |r| {
                let target: Option<String> = r.get("target_public_id")?;
                Ok(LogEntry {
                    id: r.get("id")?,
                    actor_kind: ActorKind::parse(&r.get::<_, String>("actor_kind")?),
                    actor_name: r.get("actor_name")?,
                    target_kind: r.get("target_kind")?,
                    target_public_id: target.and_then(|t| PublicId::parse(&t).ok()),
                    action: r.get("action")?,
                    created_at: r.get("created_at")?,
                })
            })
            .map_err(backend)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| corrupt("log row", e))?);
        }
        Ok(out)
    }

    async fn agreement(&self, space: SpaceId) -> StoreResult<AgreementStats> {
        self.conn
            .query_row(sql::AGREEMENT, [space], |r| {
                // SUM over no rows is NULL.
                let agreed: Option<i64> = r.get("agreed")?;
                let disagreed: Option<i64> = r.get("disagreed")?;
                Ok(AgreementStats {
                    agreed: agreed.unwrap_or(0).max(0) as u32,
                    disagreed: disagreed.unwrap_or(0).max(0) as u32,
                })
            })
            .map_err(backend)
    }
}

fn collect<T>(rows: impl Iterator<Item = rusqlite::Result<T>>, what: &str) -> StoreResult<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| corrupt(what, e))?);
    }
    Ok(out)
}

/// One row of `sql::RECENT_THREADS` / `sql::SPACE_THREADS`.
fn summary_from_row(r: &Row<'_>) -> rusqlite::Result<ThreadSummary> {
    Ok(ThreadSummary {
        public_id: parse_id(r, "public_id")?,
        title: r.get("title")?,
        post_count: r.get::<_, i64>("post_count")? as u32,
        bumped_at: r.get("bumped_at")?,
        author_name: r.get("author_name")?,
        space_name: r.get("space_name")?,
        space_path: r.get("space_path")?,
    })
}

/// One row of `sql::SPACE_BY_PATH` / `sql::SPACES_UNDER`.
fn space_from_row(r: &Row<'_>) -> rusqlite::Result<Space> {
    Ok(Space {
        id: r.get("id")?,
        path: r.get("path")?,
        name: r.get("name")?,
        parent_id: r.get("parent_id")?,
        ranking: parse_enum(&r.get::<_, String>("ranking")?),
        depth_cap: r.get::<_, i64>("depth_cap")? as u32,
    })
}

/// `id, name, state, role` columns, whatever else the row carries.
fn user_from_row(r: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get("id")?,
        name: r.get("name")?,
        state: parse_enum(&r.get::<_, String>("state")?),
        role: parse_enum(&r.get::<_, String>("role")?),
    })
}

/// One row of `sql::WRITE_CONTEXT` / `sql::SPACE_CONTEXT`.
fn write_context_from_row(r: &Row<'_>) -> rusqlite::Result<WriteContext> {
    Ok(WriteContext {
        space_id: r.get("space_id")?,
        space_config: r.get("space_config")?,
        thread_state: parse_enum(&r.get::<_, String>("thread_state")?),
        author_created_at: r.get("author_created_at")?,
        author_role: parse_enum(&r.get::<_, String>("author_role")?),
    })
}

fn parse_id(r: &Row<'_>, col: &str) -> rusqlite::Result<PublicId> {
    PublicId::parse(&r.get::<_, String>(col)?)
        .map_err(|e| rusqlite::Error::InvalidColumnName(format!("{col}: {e}")))
}

/// One row of `sql::OPEN_REVIEWS` / `sql::REVIEW_BY_ID`.
fn review_from_row(r: &Row<'_>) -> rusqlite::Result<ReviewItem> {
    let categories: Option<String> = r.get("model_categories")?;
    let categories = categories
        .and_then(|c| serde_json::from_str::<Vec<String>>(&c).ok())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| Category::parse(c))
        .collect();
    let verdict: Option<String> = r.get("model_verdict")?;
    Ok(ReviewItem {
        id: r.get("id")?,
        post_id: r.get("post_id")?,
        post_public_id: parse_id(r, "post_public_id")?,
        thread_public_id: parse_id(r, "thread_public_id")?,
        thread_title: r.get("thread_title")?,
        space_id: r.get("space_id")?,
        space_name: r.get("space_name")?,
        author_id: r.get("author_id")?,
        author_name: r.get("author_name")?,
        body_html: r.get("body_html")?,
        post_state: parse_enum(&r.get::<_, String>("post_state")?),
        reason: ReviewReason::parse(&r.get::<_, String>("reason")?),
        model_verdict: verdict.as_deref().and_then(Call::parse),
        model_confidence: r.get("model_confidence")?,
        model_categories: categories,
        appeal_text: r.get("appeal_text")?,
        opened_at: r.get("opened_at")?,
        resolved: r.get::<_, String>("state")? == "resolved",
    })
}
