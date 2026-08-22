//! One test suite, run against every [`Store`] implementation.
//!
//! DESIGN.md §9: "Two test suites: one shared conformance suite run against both `Store`
//! implementations, and pure unit tests for ranking/trust/path logic." This is the first.
//!
//! It lives in `core` rather than in a test directory because it has to be callable from two
//! very different places: a native `cargo test`, and a Worker running against real D1 where
//! `#[test]` does not exist. Both call [`run_all`].
//!
//! # What it is for
//!
//! Not to check that SQLite works. To check that two adapters agree about things the SQL does
//! not enforce: cursor semantics, tree ordering, what "not found" means, whether the space
//! comes back with the thread, whether a post's location survives being asked for. Those are
//! where adapters drift, and drift here is a silently wrong page rather than a failed build.
//!
//! # Fixture
//!
//! Callers seed the store themselves — this module cannot, because inserting is target-specific
//! and there is no write path yet. [`Fixture`] describes what the suite expects to find.

use crate::id::PublicId;
use crate::model::{NewPost, SanitizedHtml};
use crate::path::Path;
use crate::store::{Page, Store, StoreError};

/// What the suite expects the store to already contain.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// A thread with at least [`Fixture::post_count`] posts.
    pub thread: PublicId,
    /// Exact number of posts in that thread.
    pub post_count: u32,
    /// The public id of a post in that thread, and the path it sits at.
    pub known_post: PublicId,
    pub known_post_path: Path,
    /// A well-formed id that is definitely absent.
    pub absent: PublicId,
    /// Ids for posts the write checks will create, and an author to attribute them to.
    ///
    /// Supplied rather than generated because `core` has no RNG. Provide at least 4; leave
    /// empty to skip the write checks entirely, which is what a read-only fixture does.
    pub writable: Vec<PublicId>,
    pub author_id: i64,
}

/// Outcome of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub failure: Option<String>,
}

impl Check {
    fn pass(name: &'static str) -> Self {
        Check {
            name,
            failure: None,
        }
    }
    fn fail(name: &'static str, why: impl Into<String>) -> Self {
        Check {
            name,
            failure: Some(why.into()),
        }
    }
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }
}

macro_rules! require {
    ($name:expr, $cond:expr, $($why:tt)*) => {
        if !$cond {
            return Check::fail($name, format!($($why)*));
        }
    };
}

/// Run every check. Returns one [`Check`] per behaviour, in order.
///
/// Does not stop at the first failure: when two adapters disagree it is far more useful to see
/// the whole shape of the disagreement than the first symptom of it.
pub async fn run_all<S: Store>(store: &S, fx: &Fixture) -> Vec<Check> {
    vec![
        thread_page_returns_its_space(store, fx).await,
        posts_arrive_in_tree_order(store, fx).await,
        limit_is_respected_and_cursor_advances(store, fx).await,
        cursor_is_exclusive(store, fx).await,
        paging_visits_every_post_exactly_once(store, fx).await,
        cursor_is_none_on_the_last_page(store, fx).await,
        absent_thread_is_not_found(store, fx).await,
        locate_post_finds_its_thread(store, fx).await,
        absent_post_is_not_found(store, fx).await,
        posts_never_expose_body_md(store, fx).await,
    ]
    .into_iter()
    .chain(write_checks(store, fx).await)
    .collect()
}

/// Write checks, skipped when the fixture supplies no ids to write with.
///
/// These run last because they mutate the thread: everything above assumes a stable post count.
async fn write_checks<S: Store>(store: &S, fx: &Fixture) -> Vec<Check> {
    if fx.writable.len() < 4 {
        return vec![Check::pass("write checks skipped (read-only fixture)")];
    }
    vec![
        appended_post_is_readable(store, fx).await,
        replies_nest_under_their_parent(store, fx).await,
        siblings_get_consecutive_ordinals(store, fx).await,
        writing_the_same_id_twice_conflicts(store, fx).await,
        reply_to_absent_parent_is_not_found(store, fx).await,
    ]
}

fn draft(fx: &Fixture, id: &PublicId, parent: Option<&PublicId>, body: &str) -> NewPost {
    NewPost {
        public_id: id.clone(),
        thread: fx.thread.clone(),
        parent: parent.cloned(),
        author_id: fx.author_id,
        body_md: body.to_string(),
        body_html: SanitizedHtml::assert_sanitized(format!("<p>{body}</p>")),
        created_at: 1_800_000_000_000,
    }
}

async fn appended_post_is_readable<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "an appended post is readable and bumps the count";
    let before = match store.thread_page(&fx.thread, &Page::first(1)).await {
        Ok(p) => p.thread.post_count,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let post = match store
        .insert_post(&draft(fx, &fx.writable[0], None, "top level"))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("insert: {e}")),
    };
    let loc = match store.locate_post(&post.public_id).await {
        Ok(l) => l,
        Err(e) => return Check::fail(NAME, format!("locate: {e}")),
    };
    require!(NAME, loc.thread == fx.thread, "landed in the wrong thread");
    require!(
        NAME,
        loc.path == post.path,
        "insert reported {} but the row is at {}",
        post.path.as_str(),
        loc.path.as_str()
    );
    let after = match store.thread_page(&fx.thread, &Page::first(1)).await {
        Ok(p) => p.thread.post_count,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        after == before + 1,
        "post_count went {before} -> {after}"
    );
    Check::pass(NAME)
}

async fn replies_nest_under_their_parent<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "a reply nests under its parent";
    let parent = match store
        .insert_post(&draft(fx, &fx.writable[1], None, "parent"))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("parent: {e}")),
    };
    let child = match store
        .insert_post(&draft(
            fx,
            &fx.writable[2],
            Some(&parent.public_id),
            "child",
        ))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("child: {e}")),
    };
    require!(
        NAME,
        child.path.is_descendant_of(&parent.path),
        "{} is not under {}",
        child.path.as_str(),
        parent.path.as_str()
    );
    require!(
        NAME,
        child.depth == parent.depth + 1,
        "depth {} under a parent at {}",
        child.depth,
        parent.depth
    );
    require!(
        NAME,
        child.parent_id == Some(parent.id),
        "parent_id not set to the parent row"
    );
    Check::pass(NAME)
}

async fn siblings_get_consecutive_ordinals<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "siblings get consecutive ordinals, not reused ones";
    // writable[1] was made a parent above; give it a second child.
    let parent = match store.locate_post(&fx.writable[1]).await {
        Ok(l) => l,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let second = match store
        .insert_post(&draft(
            fx,
            &fx.writable[3],
            Some(&fx.writable[1]),
            "sibling",
        ))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        second.path.is_descendant_of(&parent.path),
        "second child escaped the parent"
    );
    // The first child took ordinal 0, so this must be 1 -- not 0 again.
    require!(
        NAME,
        second.path.ordinal() == 1,
        "expected ordinal 1, got {} ({})",
        second.path.ordinal(),
        second.path.as_str()
    );
    Check::pass(NAME)
}

/// The uniqueness that stops a retry from double-posting.
async fn writing_the_same_id_twice_conflicts<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "re-using a public id is rejected, not duplicated";
    // writable[0] was already written by the first check.
    match store
        .insert_post(&draft(fx, &fx.writable[0], None, "duplicate"))
        .await
    {
        Err(StoreError::Conflict) => Check::pass(NAME),
        Err(e) => Check::fail(NAME, format!("wrong error: {e}")),
        Ok(_) => Check::fail(NAME, "wrote a second post with an id already in use"),
    }
}

async fn reply_to_absent_parent_is_not_found<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "replying to a post that does not exist is NotFound";
    let mut d = draft(fx, &fx.absent, Some(&fx.absent), "orphan");
    d.public_id = fx.absent.clone();
    match store.insert_post(&d).await {
        Err(StoreError::NotFound) => Check::pass(NAME),
        Err(e) => Check::fail(NAME, format!("wrong error: {e}")),
        Ok(_) => Check::fail(NAME, "accepted a reply to a nonexistent parent"),
    }
}

async fn thread_page_returns_its_space<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "thread_page returns the thread's space";
    let page = match store.thread_page(&fx.thread, &Page::first(10)).await {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(NAME, page.thread.public_id == fx.thread, "wrong thread");
    require!(
        NAME,
        !page.space.path.is_empty(),
        "space came back with an empty path"
    );
    require!(
        NAME,
        page.space.id == page.thread.space_id,
        "space {} does not match thread.space_id {}",
        page.space.id,
        page.thread.space_id
    );
    Check::pass(NAME)
}

async fn posts_arrive_in_tree_order<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "posts arrive in tree preorder";
    let page = match store
        .thread_page(&fx.thread, &Page::first(fx.post_count))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let paths: Vec<&str> = page.posts.iter().map(|p| p.path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    require!(
        NAME,
        paths == sorted,
        "not ordered by path; first divergence at {:?}",
        paths.iter().zip(&sorted).find(|(a, b)| a != b)
    );
    Check::pass(NAME)
}

async fn limit_is_respected_and_cursor_advances<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "limit is respected and a cursor is offered";
    let limit = 5.min(fx.post_count.saturating_sub(1)).max(1);
    let page = match store.thread_page(&fx.thread, &Page::first(limit)).await {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        page.posts.len() as u32 == limit,
        "asked for {limit}, got {}",
        page.posts.len()
    );
    require!(
        NAME,
        page.next_cursor.is_some(),
        "no cursor despite more posts remaining"
    );
    Check::pass(NAME)
}

async fn cursor_is_exclusive<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "cursor is exclusive";
    let first = match store.thread_page(&fx.thread, &Page::first(1)).await {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let Some(cursor) = first.next_cursor.clone() else {
        return Check::fail(NAME, "no cursor after the first post");
    };
    let second = match store
        .thread_page(&fx.thread, &Page::after(cursor.clone(), 1))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(NAME, !second.posts.is_empty(), "second page was empty");
    require!(
        NAME,
        second.posts[0].path.as_str() != cursor.as_str(),
        "cursor {} was returned again; it must be exclusive",
        cursor.as_str()
    );
    Check::pass(NAME)
}

async fn paging_visits_every_post_exactly_once<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "paging visits every post exactly once";
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<Path> = None;
    // Bounded so a broken cursor loops finitely rather than forever.
    for _ in 0..(fx.post_count + 2) {
        let page = match cursor.clone() {
            None => store.thread_page(&fx.thread, &Page::first(7)).await,
            Some(c) => store.thread_page(&fx.thread, &Page::after(c, 7)).await,
        };
        let page = match page {
            Ok(p) => p,
            Err(e) => return Check::fail(NAME, format!("{e}")),
        };
        if page.posts.is_empty() {
            break;
        }
        seen.extend(page.posts.iter().map(|p| p.path.as_str().to_string()));
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    require!(
        NAME,
        seen.len() as u32 == fx.post_count,
        "walked {} posts, expected {}",
        seen.len(),
        fx.post_count
    );
    let mut uniq = seen.clone();
    uniq.sort_unstable();
    uniq.dedup();
    require!(
        NAME,
        uniq.len() == seen.len(),
        "{} duplicate posts across pages",
        seen.len() - uniq.len()
    );
    Check::pass(NAME)
}

async fn cursor_is_none_on_the_last_page<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "no cursor on the last page";
    let page = match store
        .thread_page(&fx.thread, &Page::first(fx.post_count + 10))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        page.next_cursor.is_none(),
        "offered a cursor past the end of the thread"
    );
    Check::pass(NAME)
}

async fn absent_thread_is_not_found<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "absent thread is NotFound, not an empty page";
    match store.thread_page(&fx.absent, &Page::first(10)).await {
        Err(StoreError::NotFound) => Check::pass(NAME),
        Err(e) => Check::fail(NAME, format!("wrong error: {e}")),
        Ok(_) => Check::fail(NAME, "returned a page for a thread that does not exist"),
    }
}

async fn locate_post_finds_its_thread<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "locate_post resolves to the right thread and path";
    let loc = match store.locate_post(&fx.known_post).await {
        Ok(l) => l,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        loc.thread == fx.thread,
        "got thread {}, expected {}",
        loc.thread,
        fx.thread
    );
    require!(
        NAME,
        loc.path == fx.known_post_path,
        "got path {}, expected {}",
        loc.path.as_str(),
        fx.known_post_path.as_str()
    );
    Check::pass(NAME)
}

async fn absent_post_is_not_found<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "absent post is NotFound";
    match store.locate_post(&fx.absent).await {
        Err(StoreError::NotFound) => Check::pass(NAME),
        Err(e) => Check::fail(NAME, format!("wrong error: {e}")),
        Ok(_) => Check::fail(NAME, "located a post that does not exist"),
    }
}

async fn posts_never_expose_body_md<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "read path does not load body_md";
    let page = match store.thread_page(&fx.thread, &Page::first(5)).await {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    // Shipping the markdown alongside the html doubles what the database sends back for a page
    // that renders only the html. An adapter that selects it will pass every other check here.
    for post in &page.posts {
        require!(
            NAME,
            post.body_md.is_none(),
            "post {} carried body_md into the read path",
            post.public_id
        );
    }
    Check::pass(NAME)
}
