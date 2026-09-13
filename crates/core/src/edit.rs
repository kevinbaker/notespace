//! Editing and deleting one's own posts. Deletion is a tombstone: the row, its position in the
//! tree and its replies all stay, and the page shows `[deleted]` where the body was.
//!
//! An edited body goes back through Tier 0, since the text a moderator approved is not the
//! text a reader now sees.

use crate::id::PublicId;
use crate::model::{PostState, SanitizedHtml, ThreadState, Timestamp, User};
use crate::moderation::pipeline::{self, ModerationQueue};
use crate::moderation::{ActorKind, NewAction};
use crate::reply;
use crate::store::{Store, StoreError, StoreResult};

#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    NotFound,
    /// Not the author, and not a moderator.
    NotYours,
    /// Deleted, or hidden by a moderator: not something its author gets to touch.
    NotEditable,
    Locked,
    Body(reply::Rejected),
}

pub enum EditOutcome {
    Edited,
    /// Rewritten, and held again for review. Carries the thread, for the notice page.
    Held {
        thread: PublicId,
    },
    Rejected(Rejected),
}

/// Who may change a post. Moderators may delete anything; only the author edits.
fn may_edit(user: &User, author_id: i64) -> bool {
    user.id == author_id
}

fn may_delete(user: &User, author_id: i64) -> bool {
    user.id == author_id || user.role.can_moderate()
}

/// **Budget: 4-8 statements.**
pub async fn edit<S: Store, Q: ModerationQueue>(
    store: &S,
    queue: &Q,
    post: &PublicId,
    user: &User,
    body_md: &str,
    body_html: SanitizedHtml,
    now: Timestamp,
) -> StoreResult<EditOutcome> {
    if let Err(why) = reply::check_body(body_md) {
        return Ok(EditOutcome::Rejected(Rejected::Body(why)));
    }
    let rp = match store.post_for_review(post).await {
        Ok(rp) => rp,
        Err(StoreError::NotFound) => return Ok(EditOutcome::Rejected(Rejected::NotFound)),
        Err(e) => return Err(e),
    };
    if !may_edit(user, rp.author_id) {
        return Ok(EditOutcome::Rejected(Rejected::NotYours));
    }
    if !matches!(rp.state, PostState::Visible | PostState::Pending) {
        return Ok(EditOutcome::Rejected(Rejected::NotEditable));
    }
    let ctx = store.write_context(&rp.thread_public_id, user.id).await?;
    if !matches!(ctx.thread_state, ThreadState::Visible | ThreadState::Pinned) {
        return Ok(EditOutcome::Rejected(Rejected::Locked));
    }
    // An unchanged body is not a duplicate of itself, so the duplicate check is skipped.
    let reasons = reply::triage(store, &ctx, user.id, body_md, None, now)
        .await?
        .unwrap_or_default();

    store
        .update_post_body(post, body_md, &body_html, now)
        .await?;
    store
        .log_action(&NewAction {
            actor_kind: ActorKind::User,
            actor_id: Some(user.id),
            actor_name: user.name.clone(),
            target_kind: "post",
            target_id: rp.id,
            action: "edit",
            detail: serde_json::json!({}),
            public: false,
            created_at: now,
        })
        .await?;

    if reasons.is_empty() || rp.state == PostState::Pending {
        return Ok(EditOutcome::Edited);
    }
    store.set_post_state(post, PostState::Pending, now).await?;
    let _ = pipeline::hold(store, queue, rp.id, &rp.public_id, &reasons, now).await?;
    Ok(EditOutcome::Held {
        thread: rp.thread_public_id,
    })
}

pub enum DeleteOutcome {
    Deleted,
    Rejected(Rejected),
}

/// **Budget: 4 statements.** Public in the log when a moderator does it to someone else's post;
/// an author deleting their own is nobody's business.
pub async fn delete<S: Store>(
    store: &S,
    post: &PublicId,
    user: &User,
    now: Timestamp,
) -> StoreResult<DeleteOutcome> {
    let rp = match store.post_for_review(post).await {
        Ok(rp) => rp,
        Err(StoreError::NotFound) => return Ok(DeleteOutcome::Rejected(Rejected::NotFound)),
        Err(e) => return Err(e),
    };
    if !may_delete(user, rp.author_id) {
        return Ok(DeleteOutcome::Rejected(Rejected::NotYours));
    }
    if rp.state == PostState::Deleted {
        return Ok(DeleteOutcome::Deleted);
    }
    store.set_post_state(post, PostState::Deleted, now).await?;
    store
        .log_action(&NewAction {
            actor_kind: ActorKind::User,
            actor_id: Some(user.id),
            actor_name: user.name.clone(),
            target_kind: "post",
            target_id: rp.id,
            action: "delete",
            detail: serde_json::json!({ "was": rp.state.as_str() }),
            public: user.id != rp.author_id,
            created_at: now,
        })
        .await?;
    Ok(DeleteOutcome::Deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, UserState};

    fn user(id: i64, role: Role) -> User {
        User {
            id,
            name: format!("u{id}"),
            state: UserState::Active,
            role,
        }
    }

    #[test]
    fn only_the_author_edits_but_moderators_may_delete() {
        let author = user(1, Role::Member);
        let other = user(2, Role::Member);
        let moderator = user(3, Role::Moderator);
        assert!(may_edit(&author, 1));
        assert!(!may_edit(&other, 1));
        assert!(
            !may_edit(&moderator, 1),
            "moderators edit through review, not in place"
        );
        assert!(may_delete(&author, 1));
        assert!(!may_delete(&other, 1));
        assert!(may_delete(&moderator, 1));
    }
}
