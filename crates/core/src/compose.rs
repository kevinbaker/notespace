//! Starting a thread. The same shape as [`crate::reply`] -- validate, rate limit, triage,
//! write -- with one more step in front: the space has to exist and be named correctly.
//!
//! The title lives on the thread row and the body is the thread's first post, so the body takes
//! the reply path unchanged, moderation included. The title is scanned by Tier 0 along with the
//! body, but only the post can be held: a held first post renders as awaiting review under a
//! title that is already visible.

use crate::id::PublicId;
use crate::model::{
    NewPost, NewThread, Post, PostState, SanitizedHtml, Thread, ThreadKind, Timestamp, UserId,
};
use crate::moderation::heuristics::Refusal;
use crate::moderation::pipeline::{self, ModerationQueue};
use crate::ratelimit::{AttemptKeys, Limit};
use crate::reply::{self, MAX_PATH_RETRIES};
use crate::space_key::SpacePath;
use crate::store::{Store, StoreError, StoreResult};

pub const MIN_TITLE_CHARS: usize = 3;
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_URL_CHARS: usize = 2_000;

pub struct ComposeConfig {
    pub per_author: Limit,
    pub per_client: Limit,
}

pub struct Draft<'a> {
    pub space: &'a SpacePath,
    pub author: UserId,
    pub title: &'a str,
    /// Empty for a discussion; a link thread otherwise.
    pub url: &'a str,
    pub body_md: &'a str,
    /// Must be the rendering of `body_md`; nothing here can check that.
    pub body_html: SanitizedHtml,
    pub client: &'a str,
    /// Caller-generated, because `core` has no RNG.
    pub thread_id: PublicId,
    /// One per attempt at the first post; supply [`MAX_PATH_RETRIES`] + 1.
    pub post_ids: &'a [PublicId],
    pub now: Timestamp,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    /// Too short, too long, or blank.
    BadTitle,
    /// Not `http(s)://`, or too long.
    BadUrl,
    /// The body failed [`reply::check_body`].
    Body(reply::Rejected),
    /// No such space, or the path is not well-formed.
    NoSuchSpace,
    /// The author posted this exact body a moment ago.
    Duplicate,
    /// Lost the first post's ordinal race repeatedly, which for a fresh thread means a bug.
    Contended,
}

pub enum Outcome {
    Posted {
        thread: Thread,
        post: Box<Post>,
    },
    /// The thread exists; its first post is `pending`.
    Held {
        thread: Thread,
        post: Box<Post>,
    },
    Rejected(Rejected),
    RateLimited {
        retry_after_secs: i64,
    },
}

/// Validate without touching storage. Returns the trimmed title and the URL, if any.
pub fn check<'a>(
    title: &'a str,
    url: &'a str,
    body_md: &str,
) -> Result<(&'a str, Option<&'a str>), Rejected> {
    let title = title.trim();
    let n = title.chars().count();
    if !(MIN_TITLE_CHARS..=MAX_TITLE_CHARS).contains(&n) || title.contains(['\r', '\n']) {
        return Err(Rejected::BadTitle);
    }
    let url = url.trim();
    let url = if url.is_empty() {
        None
    } else {
        let ok = (url.starts_with("https://") || url.starts_with("http://"))
            && url.chars().count() <= MAX_URL_CHARS
            && !url.chars().any(|c| c.is_whitespace() || c.is_control());
        if !ok {
            return Err(Rejected::BadUrl);
        }
        Some(url)
    };
    reply::check_body(body_md).map_err(Rejected::Body)?;
    Ok((title, url))
}

/// **Budget: 6-11 statements.**
pub async fn create<S: Store, Q: ModerationQueue>(
    store: &S,
    queue: &Q,
    cfg: &ComposeConfig,
    d: Draft<'_>,
) -> StoreResult<Outcome> {
    let (title, url) = match check(d.title, d.url, d.body_md) {
        Ok(t) => t,
        Err(why) => return Ok(Outcome::Rejected(why)),
    };

    // Namespaced apart from replies: starting threads is the scarcer thing.
    let keys = AttemptKeys::new(&format!("thread:{}", d.author), d.client);
    let (author_state, client_state) = store.login_attempts(&keys).await?;
    let author = cfg.per_author.check(author_state, d.now);
    let client = cfg.per_client.check(client_state, d.now);
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

    let Some(space) = store.space_by_path(d.space).await? else {
        return Ok(Outcome::Rejected(Rejected::NoSuchSpace));
    };
    let ctx = match store.space_context(space.id, d.author).await {
        Ok(ctx) => ctx,
        Err(StoreError::NotFound) => return Ok(Outcome::Rejected(Rejected::NoSuchSpace)),
        Err(e) => return Err(e),
    };
    // The title is part of what a reader sees and what a spammer writes, so it is scanned too.
    let scanned = format!("{title}\n\n{}", d.body_md);
    let reasons =
        match reply::triage(store, &ctx, d.author, &scanned, Some(d.body_md), d.now).await? {
            Err(Refusal::Duplicate) => return Ok(Outcome::Rejected(Rejected::Duplicate)),
            Ok(reasons) => reasons,
        };
    let state = if reasons.is_empty() {
        PostState::Visible
    } else {
        PostState::Pending
    };

    let thread = store
        .create_thread(&NewThread {
            public_id: d.thread_id.clone(),
            space_id: space.id,
            space_path: space.path.clone(),
            kind: if url.is_some() {
                ThreadKind::Link
            } else {
                ThreadKind::Discussion
            },
            title: title.to_string(),
            url: url.map(str::to_string),
            author_id: d.author,
            created_at: d.now,
        })
        .await?;

    // The thread counts against the limit whether or not the first post lands.
    if let Some(next) = author.next() {
        store.record_login_attempt(&keys.identity, next).await?;
    }
    if let Some(next) = client.next() {
        store.record_login_attempt(&keys.client, next).await?;
    }

    for public_id in d.post_ids.iter().take(MAX_PATH_RETRIES as usize + 1) {
        let new = NewPost {
            public_id: public_id.clone(),
            thread: thread.public_id.clone(),
            parent: None,
            author_id: d.author,
            body_md: d.body_md.to_string(),
            body_html: d.body_html.clone(),
            created_at: d.now,
            state,
        };
        match store.insert_post(&new).await {
            Ok(post) => {
                if state == PostState::Pending {
                    let _ = pipeline::hold(store, queue, post.id, &post.public_id, &reasons, d.now)
                        .await?;
                    return Ok(Outcome::Held {
                        thread,
                        post: Box::new(post),
                    });
                }
                return Ok(Outcome::Posted {
                    thread,
                    post: Box::new(post),
                });
            }
            Err(StoreError::Conflict) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(Outcome::Rejected(Rejected::Contended))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "a body long enough";

    #[test]
    fn a_plain_discussion_passes() {
        assert_eq!(check("Hello world", "", BODY), Ok(("Hello world", None)));
    }

    #[test]
    fn the_title_is_trimmed_and_bounded() {
        assert_eq!(check("  padded  ", "", BODY), Ok(("padded", None)));
        assert_eq!(check("ab", "", BODY), Err(Rejected::BadTitle));
        assert_eq!(check("   ", "", BODY), Err(Rejected::BadTitle));
        assert_eq!(
            check(&"t".repeat(MAX_TITLE_CHARS + 1), "", BODY),
            Err(Rejected::BadTitle)
        );
        assert!(check(&"t".repeat(MAX_TITLE_CHARS), "", BODY).is_ok());
        assert_eq!(check("two\nlines", "", BODY), Err(Rejected::BadTitle));
    }

    #[test]
    fn a_url_must_be_http_and_clean() {
        assert_eq!(
            check("ttl", "https://example.com/x", BODY),
            Ok(("ttl", Some("https://example.com/x")))
        );
        for bad in [
            "javascript:alert(1)",
            "ftp://example.com",
            "example.com",
            "https://exa mple.com",
            "https://example.com/a\nb",
        ] {
            assert_eq!(
                check("ttl", bad, BODY),
                Err(Rejected::BadUrl),
                "accepted {bad:?}"
            );
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_CHARS));
        assert_eq!(check("ttl", &long, BODY), Err(Rejected::BadUrl));
    }

    #[test]
    fn the_body_is_checked_by_the_reply_rules() {
        assert_eq!(
            check("ttl", "", " "),
            Err(Rejected::Body(reply::Rejected::Empty))
        );
    }
}
