//! Posting a reply.
//!
//! The counterpart to [`crate::login`]: the order of operations is the security property, so it
//! lives here where it can be tested rather than in a handler.
//!
//! ```text
//!   validate  ->  rate limit  ->  insert (retrying a path collision)
//!       |              |
//!       |              +-- before the write, so a flood costs a lookup
//!       +-- before anything, so an oversized body never reaches D1
//! ```
//!
//! Three invariants any change here must preserve:
//!
//! - **A body is bounded before it is stored.** D1 rows are a fixed budget and the render cost
//!   is linear in length; an unbounded body is both a storage and a CPU problem.
//! - **A path collision is retried, not papered over.** Two replies to the same parent in the
//!   same moment compute the same ordinal; the `UNIQUE(thread_id, path)` index rejects the
//!   loser. Retrying re-reads the parent, which is the only way to get a correct ordinal.
//! - **The retry is bounded.** An unbounded loop under contention is a CPU exhaustion vector on
//!   a 10 ms budget.

use crate::id::PublicId;
use crate::model::{NewPost, Post, SanitizedHtml, Timestamp, UserId};
use crate::ratelimit::{AttemptKeys, Limit};
use crate::store::{Store, StoreError, StoreResult};

/// Shortest body accepted, in characters. An empty reply is a misclick.
pub const MIN_BODY_CHARS: usize = 2;

/// Longest body accepted, in characters.
///
/// Generous for prose and still far below anything that threatens a row budget or the render
/// cost. A body over this is rejected rather than truncated: silently storing something other
/// than what someone wrote is worse than refusing it.
pub const MAX_BODY_CHARS: usize = 32_768;

/// How many times a path collision is retried before giving up.
///
/// Each retry is a fresh read of the parent plus an insert. Bounded because the loop runs inside
/// a request that has 10 ms of CPU, and an unbounded one under contention is a way to spend it.
pub const MAX_PATH_RETRIES: u32 = 3;

/// What a deployment's write path is configured with.
pub struct ReplyConfig {
    /// Replies allowed per author per window.
    pub per_author: Limit,
    /// Replies allowed per client address per window. Looser: a shared NAT is one client.
    pub per_client: Limit,
}

/// One reply attempt.
pub struct Reply<'a> {
    pub thread: PublicId,
    /// `None` posts at top level.
    pub parent: Option<PublicId>,
    pub author: UserId,
    /// As typed. Stored verbatim as the source of truth.
    pub body_md: &'a str,
    /// Rendered and sanitized by the caller — `core` has no markdown renderer.
    ///
    /// Must be the rendering of `body_md`. Nothing here can check that, which is why the type
    /// is [`SanitizedHtml`] rather than `String`: it is at least provably sanitized.
    pub body_html: SanitizedHtml,
    /// Client address, for the second rate-limit bucket.
    pub client: &'a str,
    /// Caller-generated, because `core` has no RNG on wasm. Regenerated per retry.
    pub ids: &'a [PublicId],
    pub now: Timestamp,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    Empty,
    TooLong {
        chars: usize,
        max: usize,
    },
    /// The space's `depth_cap` would be exceeded.
    TooDeep {
        cap: u32,
    },
    /// No such thread, or no such parent.
    NotFound,
    /// Lost the ordinal race [`MAX_PATH_RETRIES`] times running.
    Contended,
}

pub enum Outcome {
    Posted(Box<Post>),
    Rejected(Rejected),
    RateLimited { retry_after_secs: i64 },
}

/// Validate a body without touching storage.
///
/// Separate so a handler can check before rendering markdown, which is the expensive step.
pub fn check_body(body_md: &str) -> Result<(), Rejected> {
    let chars = body_md.trim().chars().count();
    if chars < MIN_BODY_CHARS {
        return Err(Rejected::Empty);
    }
    if chars > MAX_BODY_CHARS {
        return Err(Rejected::TooLong {
            chars,
            max: MAX_BODY_CHARS,
        });
    }
    Ok(())
}

/// Post one reply.
///
/// **Budget: 2-6 statements.** Attempt counters, then the insert's own batch. A rate-limited
/// attempt costs one statement and no write at all.
///
/// `r.ids` supplies one public id per attempt: a retry needs a fresh one, because the losing
/// insert may or may not have consumed it. Supply [`MAX_PATH_RETRIES`] + 1 to allow every retry.
pub async fn post<S: Store>(store: &S, cfg: &ReplyConfig, r: Reply<'_>) -> StoreResult<Outcome> {
    // 1. Validate first. The cheapest rejection, and it keeps an oversized body out of both the
    //    rate-limit counters and the renderer.
    if let Err(why) = check_body(r.body_md) {
        return Ok(Outcome::Rejected(why));
    }

    // 2. Rate limit before the write, for the same reason login limits before the hash.
    let keys = AttemptKeys::new(&r.author.to_string(), r.client);
    let (author_state, client_state) = store.login_attempts(&keys).await?;
    let author = cfg.per_author.check(author_state, r.now);
    let client = cfg.per_client.check(client_state, r.now);
    if !author.allowed() || !client.allowed() {
        let wait = author
            .retry_after_secs()
            .into_iter()
            .chain(client.retry_after_secs())
            .max()
            .unwrap_or(1);
        return Ok(Outcome::RateLimited {
            retry_after_secs: wait,
        });
    }

    // 3. Insert, retrying only the collision. Every other error is returned as itself.
    let mut last = Rejected::Contended;
    for public_id in r.ids.iter().take(MAX_PATH_RETRIES as usize + 1) {
        let new = NewPost {
            public_id: public_id.clone(),
            thread: r.thread.clone(),
            parent: r.parent.clone(),
            author_id: r.author,
            body_md: r.body_md.to_string(),
            body_html: r.body_html.clone(),
            created_at: r.now,
        };
        match store.insert_post(&new).await {
            Ok(post) => {
                // A successful write counts against the author's budget. Unlike login, success
                // does not clear it: the limit is there to bound how fast anyone can post.
                if let Some(next) = author.next() {
                    store.record_login_attempt(&keys.identity, next).await?;
                }
                if let Some(next) = client.next() {
                    store.record_login_attempt(&keys.client, next).await?;
                }
                return Ok(Outcome::Posted(Box::new(post)));
            }
            Err(StoreError::Conflict) => {
                last = Rejected::Contended;
                continue;
            }
            Err(StoreError::TooDeep { cap }) => {
                return Ok(Outcome::Rejected(Rejected::TooDeep { cap }))
            }
            Err(StoreError::NotFound) => return Ok(Outcome::Rejected(Rejected::NotFound)),
            Err(e) => return Err(e),
        }
    }
    Ok(Outcome::Rejected(last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_or_whitespace_body_is_refused() {
        for body in ["", " ", "\n\t  \n", "x"] {
            assert_eq!(check_body(body), Err(Rejected::Empty), "accepted {body:?}");
        }
    }

    #[test]
    fn an_oversized_body_is_refused_rather_than_truncated() {
        let body = "a".repeat(MAX_BODY_CHARS + 1);
        assert_eq!(
            check_body(&body),
            Err(Rejected::TooLong {
                chars: MAX_BODY_CHARS + 1,
                max: MAX_BODY_CHARS
            })
        );
        assert!(check_body(&"a".repeat(MAX_BODY_CHARS)).is_ok());
    }

    /// Length is counted in characters, not bytes: a limit that counts bytes rejects the same
    /// number of words differently depending on the language they are written in.
    #[test]
    fn the_limit_counts_characters_not_bytes() {
        let emoji = "🙂".repeat(MAX_BODY_CHARS);
        assert!(emoji.len() > MAX_BODY_CHARS, "not a multi-byte fixture");
        assert!(check_body(&emoji).is_ok());
    }
}
