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
