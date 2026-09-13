//! Administration: what a moderator or admin may change, and the log row every change leaves.
//!
//! Two tiers. A moderator acts on content and accounts -- threads, posts, bans -- because that
//! is moderation. An admin also decides who moderates and what the spaces are, because those
//! decide everything else. Every change here is an action-log row with the actor's name on it;
//! the ones a reader would want to know about (a locked thread, a hidden post, a ban) are
//! public, the housekeeping (a retitle, a role change) is not.

use crate::compose;
use crate::id::PublicId;
use crate::model::{
    NewSpace, PostState, Ranking, Role, Space, SpaceId, Thread, ThreadEdit, ThreadState, Timestamp,
    User, UserState,
};
use crate::moderation::policy::ModerationPolicy;
use crate::moderation::{ActorKind, NewAction};
use crate::space_key::{SpaceKey, SpacePath};
use crate::store::{Store, StoreError, StoreResult};
use crate::theme::Theme;

#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    /// Not a moderator, or not an admin where one is needed.
    Forbidden,
    NotFound,
    BadTitle,
    BadUrl,
    /// The target space does not exist.
    NoSuchSpace,
    /// A space key that will not parse, or a path already taken.
    BadKey(String),
    /// Nesting past the limit.
    TooDeep,
    /// A blank space name.
    BadName,
    /// Nobody may demote themselves: an admin who does is an instance with no admin.
    Yourself,
}

pub enum Outcome {
    Done,
    Rejected(Rejected),
}

fn log(
    actor: &User,
    target_kind: &'static str,
    target_id: i64,
    action: &'static str,
    detail: serde_json::Value,
    public: bool,
    now: Timestamp,
) -> NewAction {
    NewAction {
        actor_kind: ActorKind::User,
        actor_id: Some(actor.id),
        actor_name: actor.name.clone(),
        target_kind,
        target_id,
        action,
        detail,
        public,
        created_at: now,
    }
}

/// What the thread form submits. Validated the way a new thread is.
pub struct ThreadForm<'a> {
    pub title: &'a str,
    pub url: &'a str,
    pub state: ThreadState,
    pub space_id: SpaceId,
}

/// Retitle, relink, change state, or move a thread. One statement for the change and one log
/// row per thing that changed, so the log reads as what happened rather than as a diff.
/// **Budget: 3-7 statements.**
pub async fn update_thread<S: Store>(
    store: &S,
    actor: &User,
    thread: &PublicId,
    form: ThreadForm<'_>,
    now: Timestamp,
) -> StoreResult<Outcome> {
    if !actor.role.can_moderate() {
        return Ok(Outcome::Rejected(Rejected::Forbidden));
    }
    let (space, before) = match store.thread_head(thread).await {
        Ok(t) => t,
        Err(StoreError::NotFound) => return Ok(Outcome::Rejected(Rejected::NotFound)),
        Err(e) => return Err(e),
    };
    // The body check is a reply's; a thread's title and link are checked as a new one's.
    let (title, url) = match compose::check(form.title, form.url, "a placeholder body") {
        Ok(t) => t,
        Err(compose::Rejected::BadTitle) => return Ok(Outcome::Rejected(Rejected::BadTitle)),
        Err(compose::Rejected::BadUrl) => return Ok(Outcome::Rejected(Rejected::BadUrl)),
        Err(_) => return Ok(Outcome::Rejected(Rejected::BadTitle)),
    };
    let target = if form.space_id == space.id {
        space
    } else {
        match store.space_detail(form.space_id).await? {
            Some(d) => d.space,
            None => return Ok(Outcome::Rejected(Rejected::NoSuchSpace)),
        }
    };
    let edit = ThreadEdit {
        title: title.to_string(),
        url: url.map(str::to_string),
        state: form.state,
        space_id: target.id,
        space_path: target.path.clone(),
    };
    store.update_thread(thread, &edit).await?;

    if edit.title != before.title || edit.url != before.url {
        store
            .log_action(&log(
                actor,
                "thread",
                before.id,
                "edit",
                serde_json::json!({ "from": { "title": before.title, "url": before.url }, "to": { "title": edit.title, "url": edit.url } }),
                false,
                now,
            ))
            .await?;
    }
    if edit.state != before.state {
        // Reader-visible: a lock or a hide is something the thread's participants should see.
        store
            .log_action(&log(
                actor,
                "thread",
                before.id,
                state_action(edit.state),
                serde_json::json!({ "from": before.state.as_str(), "to": edit.state.as_str() }),
                true,
                now,
            ))
            .await?;
    }
    if edit.space_id != before.space_id {
        store
            .log_action(&log(
                actor,
                "thread",
                before.id,
                "move",
                serde_json::json!({ "to": edit.space_path }),
                true,
                now,
            ))
            .await?;
    }
    Ok(Outcome::Done)
}

fn state_action(s: ThreadState) -> &'static str {
    match s {
        ThreadState::Visible => "restore",
        ThreadState::Locked => "lock",
        ThreadState::Pinned => "pin",
        ThreadState::Hidden => "hide",
        ThreadState::Deleted => "delete",
    }
}

/// Set a post's state directly: hide, restore, delete, or send back to pending. The queue is
/// the usual route; this is for the post a moderator is looking at.
/// **Budget: 4 statements.**
pub async fn set_post_state<S: Store>(
    store: &S,
    actor: &User,
    post: &PublicId,
    state: PostState,
    now: Timestamp,
) -> StoreResult<Outcome> {
    if !actor.role.can_moderate() {
        return Ok(Outcome::Rejected(Rejected::Forbidden));
    }
    let rp = match store.post_for_review(post).await {
        Ok(rp) => rp,
        Err(StoreError::NotFound) => return Ok(Outcome::Rejected(Rejected::NotFound)),
        Err(e) => return Err(e),
    };
    if rp.state == state {
        return Ok(Outcome::Done);
    }
    store.set_post_state(post, state, now).await?;
    let action = match state {
        PostState::Visible => "restore",
        PostState::Pending => "hold",
        PostState::Hidden => "hide",
        PostState::Deleted => "delete",
    };
    store
        .log_action(&log(
            actor,
            "post",
            rp.id,
            action,
            serde_json::json!({ "from": rp.state.as_str(), "to": state.as_str() }),
            true,
            now,
        ))
        .await?;
    Ok(Outcome::Done)
}

/// Ban, unban, or delete an account. A ban ends every session at once, which is what makes it
/// a ban rather than a note. **Budget: 4 statements.**
pub async fn set_user_state<S: Store>(
    store: &S,
    actor: &User,
    target: &User,
    state: UserState,
    now: Timestamp,
) -> StoreResult<Outcome> {
    if !actor.role.can_moderate() {
        return Ok(Outcome::Rejected(Rejected::Forbidden));
    }
    if target.id == actor.id {
        return Ok(Outcome::Rejected(Rejected::Yourself));
    }
    // Only an admin may act against another moderator; otherwise moderators police each other.
    if target.role.can_moderate() && !actor.role.is_admin() {
        return Ok(Outcome::Rejected(Rejected::Forbidden));
    }
    if target.state == state {
        return Ok(Outcome::Done);
    }
    store.set_user_state(target.id, state).await?;
    if !state.can_act() {
        store.delete_user_sessions(target.id).await?;
    }
    let action = match state {
        UserState::Active => "unban",
        UserState::Banned => "ban",
        UserState::Deleted => "delete_account",
    };
    store
        .log_action(&log(
            actor,
            "user",
            target.id,
            action,
            serde_json::json!({ "from": target.state.as_str(), "to": state.as_str() }),
            true,
            now,
        ))
        .await?;
    Ok(Outcome::Done)
}

/// Grant or take a role. Admin only, and never one's own: an instance whose last admin demoted
/// themselves has no way back but SQL. **Budget: 2 statements.**
pub async fn set_user_role<S: Store>(
    store: &S,
    actor: &User,
    target: &User,
    role: Role,
    now: Timestamp,
) -> StoreResult<Outcome> {
    if !actor.role.is_admin() {
        return Ok(Outcome::Rejected(Rejected::Forbidden));
    }
    if target.id == actor.id {
        return Ok(Outcome::Rejected(Rejected::Yourself));
    }
    if target.role == role {
        return Ok(Outcome::Done);
    }
    store.set_user_role(target.id, role).await?;
    store
        .log_action(&log(
            actor,
            "user",
            target.id,
            "role",
            serde_json::json!({ "from": target.role.as_str(), "to": role.as_str() }),
            false,
            now,
        ))
        .await?;
    Ok(Outcome::Done)
}

/// What the space form submits, for creating and for editing.
pub struct SpaceForm<'a> {
    pub name: &'a str,
    pub ranking: Ranking,
    pub depth_cap: u32,
    pub policy: ModerationPolicy,
    pub theme: Theme,
}

impl SpaceForm<'_> {
    /// `space.config`, with the policy under `"moderation"`, the theme under `"theme"`, and
    /// everything else preserved.
    pub fn config(&self, existing: &str) -> String {
        let mut v = serde_json::from_str::<serde_json::Value>(existing)
            .ok()
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}));
        v["moderation"] = serde_json::to_value(&self.policy).unwrap_or_default();
        if self.theme.is_empty() {
            v.as_object_mut().map(|m| m.remove("theme"));
        } else {
            v["theme"] = self.theme.to_json();
        }
        v.to_string()
    }
}

/// A new space under `parent`, or at the top level. Admin only. **Budget: 2-3 statements.**
pub async fn create_space<S: Store>(
    store: &S,
    actor: &User,
    key: &str,
    parent: Option<SpaceId>,
    form: SpaceForm<'_>,
    now: Timestamp,
) -> StoreResult<Result<Space, Rejected>> {
    if !actor.role.is_admin() {
        return Ok(Err(Rejected::Forbidden));
    }
    if form.name.trim().is_empty() {
        return Ok(Err(Rejected::BadName));
    }
    let key = match SpaceKey::parse(key) {
        Ok(k) => k,
        Err(e) => return Ok(Err(Rejected::BadKey(e.to_string()))),
    };
    let path = match parent {
        None => SpacePath::root(&key),
        Some(id) => {
            let Some(p) = store.space_detail(id).await? else {
                return Ok(Err(Rejected::NoSuchSpace));
            };
            let parent_path = SpacePath::parse(&p.space.path)
                .map_err(|e| StoreError::Corrupt(format!("space path: {e}")))?;
            match parent_path.child(&key) {
                Ok(p) => p,
                Err(_) => return Ok(Err(Rejected::TooDeep)),
            }
        }
    };
    let new = NewSpace {
        name: form.name.trim().to_string(),
        path: path.as_stored().to_string(),
        parent_id: parent,
        ranking: form.ranking,
        depth_cap: form.depth_cap,
        config: form.config("{}"),
    };
    let id = match store.create_space(&new).await {
        Ok(id) => id,
        Err(StoreError::Conflict) => {
            return Ok(Err(Rejected::BadKey(
                "that key is already taken here".into(),
            )))
        }
        Err(e) => return Err(e),
    };
    store
        .log_action(&log(
            actor,
            "space",
            id,
            "create",
            serde_json::json!({ "path": new.path }),
            true,
            now,
        ))
        .await?;
    Ok(Ok(Space {
        id,
        path: new.path,
        name: new.name,
        parent_id: parent,
        ranking: new.ranking,
        depth_cap: new.depth_cap,
        config: new.config,
    }))
}

/// Rename, re-rank, re-cap, or re-policy a space. Admin only. **Budget: 3 statements.**
pub async fn update_space<S: Store>(
    store: &S,
    actor: &User,
    space: SpaceId,
    form: SpaceForm<'_>,
    now: Timestamp,
) -> StoreResult<Outcome> {
    if !actor.role.is_admin() {
        return Ok(Outcome::Rejected(Rejected::Forbidden));
    }
    if form.name.trim().is_empty() {
        return Ok(Outcome::Rejected(Rejected::BadName));
    }
    let Some(existing) = store.space_detail(space).await? else {
        return Ok(Outcome::Rejected(Rejected::NotFound));
    };
    let config = form.config(&existing.space.config);
    store
        .update_space(
            space,
            form.name.trim(),
            form.ranking,
            form.depth_cap,
            &config,
        )
        .await?;
    store
        .log_action(&log(
            actor,
            "space",
            space,
            "configure",
            serde_json::json!({ "name": form.name.trim(), "moderation": form.policy }),
            false,
            now,
        ))
        .await?;
    Ok(Outcome::Done)
}

/// A thread's `Thread` and the admin-relevant view of it, for the form.
pub async fn thread_for_edit<S: Store>(
    store: &S,
    thread: &PublicId,
) -> StoreResult<Option<(Space, Thread)>> {
    match store.thread_head(thread).await {
        Ok(t) => Ok(Some(t)),
        Err(StoreError::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_space_form_writes_the_policy_and_keeps_the_rest_of_the_config() {
        let form = SpaceForm {
            name: "General",
            ranking: Ranking::Bump,
            depth_cap: 8,
            policy: ModerationPolicy {
                new_account_hours: 2,
                blocklist: vec!["casino".into()],
                ..ModerationPolicy::default()
            },
            theme: Theme::parse_lines("accent: #c00").unwrap(),
        };
        let out = form.config(r#"{"preset":"hn","moderation":{"new_account_hours":72}}"#);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["preset"], "hn", "unrelated config was dropped");
        assert_eq!(v["theme"]["accent"], "#c00");
        assert_eq!(Theme::from_config(&out).both[0].1, "#c00");
        assert_eq!(v["moderation"]["new_account_hours"], 2);
        assert_eq!(v["moderation"]["blocklist"][0], "casino");
        // And it round-trips through the reader the write path uses.
        assert_eq!(ModerationPolicy::from_config(&out).new_account_hours, 2);
        // Garbage in is replaced, not preserved.
        assert!(form.config("not json").starts_with('{'));
    }
}
