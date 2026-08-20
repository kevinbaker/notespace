//! Domain types. See DESIGN.md §2 (primitives) and §4 (data model).
//!
//! M0 covers only the read path for a thread page, so this is the Space/Thread/Post/User
//! subset. Signal, Capability, ActionLog and Rule land in M1-M4.

use crate::id::PublicId;
use crate::path::Path;
use serde::{Deserialize, Serialize};

pub type SpaceId = i64;
pub type ThreadId = i64;
pub type PostId = i64;
pub type UserId = i64;

/// Unix seconds. Deliberately not `std::time::SystemTime`: that panics on wasm (DESIGN.md §3.2).
pub type Timestamp = i64;

/// DESIGN.md primitive #1. Owns permissions and the ranking function.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Space {
    pub id: SpaceId,
    pub slug: String,
    pub name: String,
    pub parent_id: Option<SpaceId>,
    pub ranking: Ranking,
    /// 0 means a flat board; see the Classic BB preset in DESIGN.md §6.
    pub depth_cap: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Ranking {
    #[default]
    Bump,
    Gravity,
    Best,
    ScoreThreshold,
}

/// DESIGN.md primitive #2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThreadKind {
    #[default]
    Discussion,
    Link,
    Question,
    Poll,
    Announcement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    #[default]
    Visible,
    Locked,
    Pinned,
    Hidden,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PostState {
    #[default]
    Visible,
    /// Held by the moderation pipeline (DESIGN.md §5) pending classification.
    Pending,
    Hidden,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    /// Internal identity. Carries every foreign key; never appears in a URL.
    pub id: ThreadId,
    /// Opaque, time-sortable id used in URLs (DESIGN.md §4.2).
    pub public_id: PublicId,
    pub space_id: SpaceId,
    pub kind: ThreadKind,
    pub title: String,
    pub url: Option<String>,
    pub author_id: UserId,
    pub author_name: String,
    pub created_at: Timestamp,
    pub bumped_at: Timestamp,
    pub post_count: u32,
    pub state: ThreadState,
    /// Bumped on edit/delete to invalidate baked pages without a cache purge (DESIGN.md §3.3).
    pub cache_version: i64,
}

/// DESIGN.md primitive #3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Post {
    pub id: PostId,
    pub thread_id: ThreadId,
    pub parent_id: Option<PostId>,
    pub path: Path,
    pub depth: u32,
    pub author_id: UserId,
    pub author_name: String,
    /// Source of truth. Never served directly.
    ///
    /// `None` means "not loaded". The read path deliberately does not select this column:
    /// a thread page needs only `body_html`, and shipping both doubles the bytes D1 sends
    /// back for no benefit. The edit path loads it; rendering never does.
    pub body_md: Option<String>,
    /// Rendered and sanitized at *write* time (DESIGN.md §3.3). Safe to emit verbatim.
    pub body_html: String,
    pub created_at: Timestamp,
    pub edited_at: Option<Timestamp>,
    pub score: f64,
    pub state: PostState,
}

/// One page of a thread: metadata plus a preorder-contiguous run of posts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadPage {
    pub thread: Thread,
    /// Already in tree preorder: the store returns them `ORDER BY path`.
    pub posts: Vec<Post>,
    /// Path to resume from for the next page, if the thread continues past this one.
    pub next_cursor: Option<Path>,
}

impl ThreadPage {
    pub fn is_empty(&self) -> bool {
        self.posts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.posts.len()
    }
}

impl Path {
    /// Indentation level to render at, clamped to a space's `depth_cap`.
    ///
    /// A flat board (`depth_cap == 0`) renders every post at the same level regardless of
    /// its stored path, so a space can be reconfigured between flat and threaded without
    /// rewriting stored data.
    pub fn render_depth(&self, depth_cap: u32) -> u32 {
        (self.depth() as u32).min(depth_cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_board_collapses_every_depth() {
        let deep = Path::root(1).unwrap().child(2).unwrap().child(3).unwrap();
        assert_eq!(deep.render_depth(0), 0);
        assert_eq!(deep.render_depth(1), 1);
        assert_eq!(
            deep.render_depth(8),
            2,
            "clamps to actual depth, not the cap"
        );
    }
}
