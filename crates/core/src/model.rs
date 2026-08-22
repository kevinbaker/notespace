//! Domain types.
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

/// Unix seconds. Deliberately not `std::time::SystemTime`: that panics on wasm.
pub type Timestamp = i64;

/// HTML that has already been through the sanitizer.
///
/// The client is the attacker, so sanitizing happens server-side, always. Client-generated
/// HTML persisted and served to other readers is stored XSS, and the read path emits
/// `body_html` verbatim — so the only thing standing between a crafted post and every future
/// reader of that thread is that this string went through `notespace_render`.
///
/// `core` cannot depend on `render` without inverting the layering, so this cannot be enforced
/// by the type system alone. What it can do is make a bypass obvious: there is exactly one
/// constructor, it is named [`SanitizedHtml::assert_sanitized`], and any call to it outside the
/// renderer should fail review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SanitizedHtml(String);

impl SanitizedHtml {
    /// Assert that `html` has been sanitized. Call this from the renderer and nowhere else.
    pub fn assert_sanitized(html: String) -> Self {
        SanitizedHtml(html)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

/// A post to be written.
///
/// Ids and timestamps are supplied by the caller rather than generated here: `core` has no clock
/// and no RNG, because neither exists on wasm.
#[derive(Debug, Clone)]
pub struct NewPost {
    pub public_id: PublicId,
    /// The thread to append to, by public id.
    pub thread: PublicId,
    /// The post being replied to. `None` makes this a new top-level post.
    ///
    /// Addressed by public id, not path: a path is a position, and positions move.
    pub parent: Option<PublicId>,
    pub author_id: UserId,
    /// Source of truth, stored verbatim.
    pub body_md: String,
    /// Rendered at write time so the read path never renders markdown.
    pub body_html: SanitizedHtml,
    pub created_at: Timestamp,
}

/// A container of threads, and the unit configuration attaches to: it owns the permissions
/// and the ranking function that decide how its threads behave.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Space {
    pub id: SpaceId,
    /// Materialized URL path, stored with a trailing separator: `"sports/hockey/"`. This is the
    /// identifier -- there is no separate key column, since the key is the last segment. See
    /// [`crate::space_key::SpacePath`].
    pub path: String,
    /// Display name, e.g. `"Ice Hockey"`. Never appears in a URL.
    pub name: String,
    pub parent_id: Option<SpaceId>,
    pub ranking: Ranking,
    /// 0 means a flat board, which is what the classic-bulletin-board preset selects.
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

/// A conversation within a space: the unit that gets listed, ranked and paginated.
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
    /// Held by the moderation pipeline pending classification.
    Pending,
    Hidden,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub name: String,
    pub state: UserState,
}

/// Why a username stays taken forever: `Deleted` is a tombstone, not a removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UserState {
    #[default]
    Active,
    /// Account gone. The row and the name remain so neither can be reissued.
    Deleted,
    /// Suspended. Sessions are revoked on ban, but this is checked on every authenticated
    /// request too: a ban that depends on a cleanup query having run is not a ban.
    Banned,
}

impl UserState {
    /// Whether this account may act — post, vote, or hold a session.
    pub fn can_act(&self) -> bool {
        matches!(self, UserState::Active)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    /// Internal identity. Carries every foreign key; never appears in a URL.
    pub id: ThreadId,
    /// Opaque, time-sortable id used in URLs.
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
    /// Bumped on edit/delete to invalidate baked pages without a cache purge.
    pub cache_version: i64,
}

/// A single message in a thread, positioned by its materialized `path`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Post {
    pub id: PostId,
    /// Opaque, time-sortable public id.
    ///
    /// A post's other address, `(thread_id, path)`, encodes which thread it is in, so a split
    /// or merge invalidates it. This one survives the move, which is what makes a permalink
    /// durable.
    pub public_id: PublicId,
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
    /// Rendered and sanitized at *write* time. Safe to emit verbatim.
    pub body_html: String,
    pub created_at: Timestamp,
    pub edited_at: Option<Timestamp>,
    pub score: f64,
    pub state: PostState,
}

/// One page of a thread: metadata plus a preorder-contiguous run of posts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadPage {
    /// The space the thread lives in, joined in the same query rather than fetched separately.
    /// The render needs `depth_cap`, and a second round trip for one integer is not in budget.
    pub space: Space,
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

// ---------------------------------------------------------------------------
// String representations
// ---------------------------------------------------------------------------
//
// Each of these enums crosses the database boundary as text, and every `Store` implementation
// has to agree on that text. The D1 adapter arrives at it through serde's `rename_all`; a
// SQLite adapter reading a `String` out of a column needs the same mapping by another route.
//
// Two routes to one mapping is a drift vector: nothing would fail to compile if serde said
// "score_threshold" and a hand-written parser said "scorethreshold" -- the page would just
// silently rank wrongly on one target. So the mapping is written once here, and
// `string_forms_match_serde` asserts the serde representation agrees with it.
//
// Unknown input falls back to `Default` rather than erroring. A row written by a newer version
// with a state this build has never heard of is a reason to render conservatively, not to fail
// the request.

macro_rules! string_enum {
    ($ty:ty { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $ty {
            pub const fn as_str(&self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
        }
        impl core::str::FromStr for $ty {
            type Err = core::convert::Infallible;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(match s { $($text => Self::$variant,)+ _ => Self::default() })
            }
        }
        impl core::fmt::Display for $ty {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

string_enum!(Ranking {
    Bump => "bump",
    Gravity => "gravity",
    Best => "best",
    ScoreThreshold => "score_threshold",
});

string_enum!(ThreadKind {
    Discussion => "discussion",
    Link => "link",
    Question => "question",
    Poll => "poll",
    Announcement => "announcement",
});

string_enum!(ThreadState {
    Visible => "visible",
    Locked => "locked",
    Pinned => "pinned",
    Hidden => "hidden",
    Deleted => "deleted",
});

string_enum!(UserState {
    Active => "active",
    Deleted => "deleted",
    Banned => "banned",
});

string_enum!(PostState {
    Visible => "visible",
    Pending => "pending",
    Hidden => "hidden",
    Deleted => "deleted",
});

#[cfg(test)]
mod string_form_tests {
    use super::*;

    /// The two routes to the same mapping must agree, or the adapters disagree silently.
    macro_rules! check {
        ($ty:ty, [$($variant:expr),+ $(,)?]) => {
            for v in [$($variant),+] {
                let via_serde = serde_json::to_string(&v).unwrap();
                let via_serde = via_serde.trim_matches('"');
                assert_eq!(
                    v.as_str(), via_serde,
                    "as_str and serde disagree for {v:?}"
                );
                assert_eq!(
                    v.to_string().parse::<$ty>().unwrap(), v,
                    "round trip failed for {v:?}"
                );
                assert_eq!(
                    serde_json::from_str::<$ty>(&format!("\"{}\"", v.as_str())).unwrap(), v,
                    "serde could not read back as_str for {v:?}"
                );
            }
        };
    }

    #[test]
    fn string_forms_match_serde() {
        use Ranking::*;
        check!(Ranking, [Bump, Gravity, Best, ScoreThreshold]);
        use ThreadKind::*;
        check!(ThreadKind, [Discussion, Link, Question, Poll, Announcement]);
        use ThreadState::*;
        check!(ThreadState, [Visible, Locked, Pinned, Hidden, Deleted]);
        check!(
            UserState,
            [UserState::Active, UserState::Deleted, UserState::Banned]
        );
        check!(
            PostState,
            [
                PostState::Visible,
                PostState::Pending,
                PostState::Hidden,
                PostState::Deleted
            ]
        );
    }

    #[test]
    fn unknown_values_fall_back_to_default() {
        assert_eq!(
            "who_knows".parse::<PostState>().unwrap(),
            PostState::Visible
        );
        assert_eq!("".parse::<Ranking>().unwrap(), Ranking::Bump);
    }
}
