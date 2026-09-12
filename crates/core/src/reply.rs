//! Posting a reply. Ordering is the security property, so it lives here rather than a handler:
//! validate, then rate limit, then triage, then insert — each step before the one it protects.

use crate::id::PublicId;
use crate::model::{NewPost, Post, PostState, SanitizedHtml, ThreadState, Timestamp, UserId};
use crate::moderation::heuristics::{self, Refusal, Signals, Triage};
use crate::moderation::pipeline::{self, ModerationQueue};
use crate::moderation::policy::ModerationPolicy;
use crate::ratelimit::{AttemptKeys, Limit};
use crate::store::{Store, StoreError, StoreResult};

/// Shortest body accepted, in characters.
pub const MIN_BODY_CHARS: usize = 2;

/// Longest body accepted, in characters. Rejected rather than truncated.
pub const MAX_BODY_CHARS: usize = 32_768;

/// Bounded: an unbounded retry loop under contention spends the whole CPU budget.
pub const MAX_PATH_RETRIES: u32 = 3;

pub struct ReplyConfig {
    pub per_author: Limit,
    /// Looser than `per_author`: a shared NAT is one client.
    pub per_client: Limit,
}

pub struct Reply<'a> {
    pub thread: PublicId,
    /// `None` posts at top level.
    pub parent: Option<PublicId>,
    pub author: UserId,
    pub body_md: &'a str,
    /// Must be the rendering of `body_md`; nothing here can check that.
    pub body_html: SanitizedHtml,
    pub client: &'a str,
    /// Caller-generated: `core` has no RNG on wasm. One per retry.
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
    TooDeep {
        cap: u32,
    },
    /// No such thread, or no such parent.
    NotFound,
    /// Lost the ordinal race [`MAX_PATH_RETRIES`] times running.
    Contended,
    /// The thread is locked.
    Locked,
    /// The author posted this exact body a moment ago.
    Duplicate,
}

pub enum Outcome {
    Posted(Box<Post>),
    /// Written as `pending` and handed to the moderation pipeline. The post exists and has a
    /// permalink; it renders as awaiting review until the pipeline decides.
    Held(Box<Post>),
    Rejected(Rejected),
    RateLimited { retry_after_secs: i64 },
}

/// Separate from [`post`] so a handler can check before rendering markdown.
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

/// **Budget: 4-9 statements.** A rate-limited attempt costs one and no write; a held post costs
/// one more than a published one, for the log row.
///
/// Supply [`MAX_PATH_RETRIES`] + 1 ids: a retry needs a fresh one.
pub async fn post<S: Store, Q: ModerationQueue>(
    store: &S,
    queue: &Q,
    cfg: &ReplyConfig,
    r: Reply<'_>,
) -> StoreResult<Outcome> {
    if let Err(why) = check_body(r.body_md) {
        return Ok(Outcome::Rejected(why));
    }

    // Before the write, for the same reason login limits before the hash.
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

    // Tier 0. Cheap, and before the write so a refused post is never written.
    let ctx = match store.write_context(&r.thread, r.author).await {
        Ok(ctx) => ctx,
        Err(StoreError::NotFound) => return Ok(Outcome::Rejected(Rejected::NotFound)),
        Err(e) => return Err(e),
    };
    if ctx.thread_state == ThreadState::Locked {
        return Ok(Outcome::Rejected(Rejected::Locked));
    }
    let policy = ModerationPolicy::from_config(&ctx.space_config);
    let is_duplicate = policy.duplicate_window_ms() > 0
        && store
            .author_posted_recently(r.author, r.body_md, r.now - policy.duplicate_window_ms())
            .await?;
    let reasons = match heuristics::triage(
        &policy,
        &Signals {
            body_md: r.body_md,
            author_created_at: ctx.author_created_at,
            author_role: ctx.author_role,
            is_duplicate,
            now: r.now,
        },
    ) {
        Triage::Refuse(Refusal::Duplicate) => {
            return Ok(Outcome::Rejected(Rejected::Duplicate));
        }
        Triage::Publish => Vec::new(),
        Triage::Hold(reasons) => reasons,
    };
    let state = if reasons.is_empty() {
        PostState::Visible
    } else {
        PostState::Pending
    };

    // Only a collision is retried; every other error is returned as itself.
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
            state,
        };
        match store.insert_post(&new).await {
            Ok(post) => {
                // Unlike login, success does not clear the counter: the limit bounds post rate.
                if let Some(next) = author.next() {
                    store.record_login_attempt(&keys.identity, next).await?;
                }
                if let Some(next) = client.next() {
                    store.record_login_attempt(&keys.client, next).await?;
                }
                if state == PostState::Pending {
                    // The enqueue result is not this function's to report; the sweep covers it.
                    let _ = pipeline::hold(store, queue, &post, &reasons, r.now).await?;
                    return Ok(Outcome::Held(Box::new(post)));
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

    /// A byte limit would reject the same words differently per language.
    #[test]
    fn the_limit_counts_characters_not_bytes() {
        let emoji = "🙂".repeat(MAX_BODY_CHARS);
        assert!(emoji.len() > MAX_BODY_CHARS, "not a multi-byte fixture");
        assert!(check_body(&emoji).is_ok());
    }
}
