//! The steps of the pipeline, each a function over a [`Store`]. The write path calls [`hold`];
//! a queue consumer or a scheduled sweep calls [`process_post`] and [`drain`]; the handlers
//! call [`report`], [`review`] and [`appeal`].
//!
//! Invariants every step keeps:
//! - a post's state only changes alongside an action-log row saying who changed it;
//! - the model's output is never applied without being logged first;
//! - a human decision is final for that item -- nothing automatic reopens or reverses it.

use super::classify::{Call, Classifier, ClassifyError, ClassifyInput, PROMPT_VERSION};
use super::heuristics::Reason;
use super::policy::{Disposition, ModerationPolicy};
use super::{
    ActorKind, NewAction, NewReview, NewSignal, Resolution, ReviewReason, Verdict, SIGNAL_REPORT,
};
use crate::id::PublicId;
use crate::model::{PostId, PostState, Timestamp, User};
use crate::store::{Store, StoreError, StoreResult};

/// Where held posts are announced so a consumer picks them up promptly. Failure is not fatal:
/// the sweep in [`drain`] finds anything still pending.
#[async_trait::async_trait(?Send)]
pub trait ModerationQueue {
    async fn enqueue(&self, post: &PublicId, reasons: &[Reason]) -> Result<(), String>;
}

/// No queue at all. The sweep does all the work; fine for a native binary on a timer.
pub struct NoQueue;

#[async_trait::async_trait(?Send)]
impl ModerationQueue for NoQueue {
    async fn enqueue(&self, _post: &PublicId, _reasons: &[Reason]) -> Result<(), String> {
        Ok(())
    }
}

/// Name Tier 0 signs the log with.
pub const RULE_ACTOR: &str = "tier0";
pub const SYSTEM_ACTOR: &str = "notespace";

/// Longest appeal accepted, in characters.
pub const MAX_APPEAL_CHARS: usize = 2_000;

/// How long a pending post is left to the queue consumer before the sweep takes it.
pub const SWEEP_GRACE_MS: i64 = 60_000;

/// A post was written as `pending`: say why in the log and tell the consumer. Returns whether
/// the enqueue succeeded, for the caller's own log; the outcome is the same either way.
pub async fn hold<S: Store, Q: ModerationQueue>(
    store: &S,
    queue: &Q,
    post_id: PostId,
    public_id: &PublicId,
    reasons: &[Reason],
    now: Timestamp,
) -> StoreResult<Result<(), String>> {
    store
        .log_action(&NewAction {
            actor_kind: ActorKind::Rule,
            actor_id: None,
            actor_name: RULE_ACTOR.into(),
            target_kind: "post",
            target_id: post_id,
            action: "hold",
            detail: serde_json::json!({ "reasons": reasons }),
            public: true,
            created_at: now,
        })
        .await?;
    Ok(queue.enqueue(public_id, reasons).await)
}

/// What the consumer did with one post.
#[derive(Debug, Clone, PartialEq)]
pub enum Processed {
    /// Not pending any more -- a duplicate delivery, or a human got there first.
    Skipped,
    Published,
    /// Hidden pending confirmation.
    Hidden,
    /// Left pending, in the queue for a human.
    Queued,
    /// The classifier failed; queued for a human with no verdict attached.
    ClassifierFailed(ClassifyError),
}

/// Classify one pending post and act on the verdict. Idempotent: anything not `pending` is
/// skipped, so a redelivered message costs one read. `held_for` is what Tier 0 recorded, when
/// the caller has it (a queue message does, the sweep does not).
///
/// **Budget: 2 reads + up to 4 writes**, plus one model call.
pub async fn process_post<S: Store, C: Classifier>(
    store: &S,
    classifier: &C,
    post: &PublicId,
    held_for: &[Reason],
    now: Timestamp,
) -> StoreResult<Processed> {
    let rp = store.post_for_review(post).await?;
    if rp.state != PostState::Pending {
        return Ok(Processed::Skipped);
    }
    let policy = ModerationPolicy::from_config(&rp.space_config);
    if !policy.enabled {
        // Turned off after the post was held. Nobody looks; it goes out.
        store.set_post_state(post, PostState::Visible, now).await?;
        store
            .log_action(&system_action(rp.id, "publish", "moderation disabled", now))
            .await?;
        return Ok(Processed::Published);
    }

    let input = ClassifyInput {
        body_md: &rp.body_md,
        space_name: &rp.space_name,
        space_rules: policy.rules.as_deref(),
        thread_title: &rp.thread_title,
        author_created_at: rp.author_created_at,
        now,
        held_for,
    };
    let verdict = match classifier.classify(&input).await {
        Ok(v) => v,
        Err(e) => {
            store
                .log_action(&NewAction {
                    actor_kind: ActorKind::System,
                    actor_id: None,
                    actor_name: SYSTEM_ACTOR.into(),
                    target_kind: "post",
                    target_id: rp.id,
                    action: "classify_failed",
                    detail: serde_json::json!({ "model": classifier.model(), "error": e.to_string() }),
                    public: false,
                    created_at: now,
                })
                .await?;
            store
                .open_review(&NewReview {
                    post_id: rp.id,
                    space_id: rp.space_id,
                    reason: ReviewReason::Error,
                    verdict: None,
                    appeal_text: None,
                    opened_at: now,
                })
                .await?;
            return Ok(Processed::ClassifierFailed(e));
        }
    };

    // Logged before it is acted on, so a crash between the two leaves a verdict without an
    // action rather than an action without a verdict.
    store
        .log_action(&model_action(&verdict, rp.id, "classify", false, now))
        .await?;

    let effective = policy.effective(&store.agreement(rp.space_id).await?);
    match effective.decide(&verdict) {
        Disposition::Publish => {
            store.set_post_state(post, PostState::Visible, now).await?;
            store
                .log_action(&model_action(&verdict, rp.id, "publish", true, now))
                .await?;
            Ok(Processed::Published)
        }
        Disposition::HideForReview => {
            store.set_post_state(post, PostState::Hidden, now).await?;
            store
                .log_action(&model_action(&verdict, rp.id, "hide", true, now))
                .await?;
            store
                .open_review(&NewReview {
                    post_id: rp.id,
                    space_id: rp.space_id,
                    reason: ReviewReason::Classifier,
                    verdict: Some(verdict),
                    appeal_text: None,
                    opened_at: now,
                })
                .await?;
            Ok(Processed::Hidden)
        }
        Disposition::HoldForReview => {
            store
                .open_review(&NewReview {
                    post_id: rp.id,
                    space_id: rp.space_id,
                    reason: ReviewReason::Classifier,
                    verdict: Some(verdict),
                    appeal_text: None,
                    opened_at: now,
                })
                .await?;
            Ok(Processed::Queued)
        }
    }
}

fn model_action(
    v: &Verdict,
    post_id: i64,
    action: &'static str,
    public: bool,
    now: Timestamp,
) -> NewAction {
    NewAction {
        actor_kind: ActorKind::Model,
        actor_id: None,
        actor_name: v.model.clone(),
        target_kind: "post",
        target_id: post_id,
        action,
        detail: v.to_log_detail(PROMPT_VERSION),
        public,
        created_at: now,
    }
}

fn system_action(post_id: i64, action: &'static str, why: &str, now: Timestamp) -> NewAction {
    NewAction {
        actor_kind: ActorKind::System,
        actor_id: None,
        actor_name: SYSTEM_ACTOR.into(),
        target_kind: "post",
        target_id: post_id,
        action,
        detail: serde_json::json!({ "why": why }),
        public: true,
        created_at: now,
    }
}

/// The safety net: classify whatever has been pending longer than [`SWEEP_GRACE_MS`] and is not
/// already waiting on a human. Each post is processed independently, so one failure does not
/// stop the rest; a store error on a post is returned in its slot.
pub async fn drain<S: Store, C: Classifier>(
    store: &S,
    classifier: &C,
    now: Timestamp,
    limit: u32,
) -> StoreResult<Vec<(PublicId, StoreResult<Processed>)>> {
    let pending = store.pending_posts(now - SWEEP_GRACE_MS, limit).await?;
    let mut out = Vec::with_capacity(pending.len());
    for id in pending {
        let r = process_post(store, classifier, &id, &[], now).await;
        out.push((id, r));
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportOutcome {
    /// Counted. `count` is the distinct reporters now standing.
    Recorded { count: u32 },
    /// This account had already reported this post.
    AlreadyReported { count: u32 },
    /// The report tipped the post over the threshold: it is pending again and queued.
    Held { count: u32 },
    /// Reporting your own post is a delete request, which this is not.
    OwnPost,
    /// Deleted posts are past reporting.
    Gone,
}

/// Longest report reason kept.
pub const MAX_REPORT_CHARS: usize = 500;

/// **Budget: 3 reads/writes**, plus 4 more when the threshold is crossed.
pub async fn report<S: Store, Q: ModerationQueue>(
    store: &S,
    queue: &Q,
    post: &PublicId,
    reporter: &User,
    reason: Option<&str>,
    now: Timestamp,
) -> StoreResult<ReportOutcome> {
    let rp = store.post_for_review(post).await?;
    if rp.author_id == reporter.id {
        return Ok(ReportOutcome::OwnPost);
    }
    if rp.state == PostState::Deleted {
        return Ok(ReportOutcome::Gone);
    }
    let reason: Option<String> = reason
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(|r| r.chars().take(MAX_REPORT_CHARS).collect());
    let tally = store
        .add_report(&NewSignal {
            post_id: rp.id,
            user_id: reporter.id,
            kind: SIGNAL_REPORT,
            weight: -1.0,
            reason,
            created_at: now,
        })
        .await?;
    if !tally.added {
        return Ok(ReportOutcome::AlreadyReported { count: tally.count });
    }
    // Private: who reported whom is not for the public log.
    store
        .log_action(&NewAction {
            actor_kind: ActorKind::User,
            actor_id: Some(reporter.id),
            actor_name: reporter.name.clone(),
            target_kind: "post",
            target_id: rp.id,
            action: "report",
            detail: serde_json::json!({ "count": tally.count }),
            public: false,
            created_at: now,
        })
        .await?;

    let policy = ModerationPolicy::from_config(&rp.space_config);
    if rp.state == PostState::Visible && tally.count >= policy.report_threshold.max(1) {
        store.set_post_state(post, PostState::Pending, now).await?;
        store
            .log_action(&NewAction {
                actor_kind: ActorKind::Rule,
                actor_id: None,
                actor_name: RULE_ACTOR.into(),
                target_kind: "post",
                target_id: rp.id,
                action: "hold",
                detail: serde_json::json!({ "reports": tally.count }),
                public: true,
                created_at: now,
            })
            .await?;
        store
            .open_review(&NewReview {
                post_id: rp.id,
                space_id: rp.space_id,
                reason: ReviewReason::Reports,
                verdict: None,
                appeal_text: None,
                opened_at: now,
            })
            .await?;
        // A second opinion for the reviewer; the item is open whether or not the model answers.
        let _ = queue.enqueue(post, &[]).await;
        return Ok(ReportOutcome::Held { count: tally.count });
    }
    Ok(ReportOutcome::Recorded { count: tally.count })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewOutcome {
    Resolved {
        post: PublicId,
        now_state: PostState,
        /// Whether the human agreed with the model, when the model had made a call.
        agreed: Option<bool>,
    },
    /// Already resolved, or never existed.
    Gone,
    Forbidden,
}

/// A human decides. Approve publishes, reject hides; both log the reviewer and whether the
/// model agreed, which is the metamoderation signal. **Budget: 4 statements.**
pub async fn review<S: Store>(
    store: &S,
    id: i64,
    reviewer: &User,
    resolution: Resolution,
    now: Timestamp,
) -> StoreResult<ReviewOutcome> {
    if !reviewer.role.can_moderate() {
        return Ok(ReviewOutcome::Forbidden);
    }
    let Some(item) = store
        .resolve_review(id, resolution, reviewer.id, now)
        .await?
    else {
        return Ok(ReviewOutcome::Gone);
    };
    let now_state = match resolution {
        Resolution::Approve => PostState::Visible,
        Resolution::Reject => PostState::Hidden,
    };
    store
        .set_post_state(&item.post_public_id, now_state, now)
        .await?;
    let agreed = match (item.model_verdict, resolution) {
        (Some(Call::Clean), Resolution::Approve) | (Some(Call::Flag), Resolution::Reject) => {
            Some(true)
        }
        (Some(Call::Clean), Resolution::Reject) | (Some(Call::Flag), Resolution::Approve) => {
            Some(false)
        }
        _ => None,
    };
    store
        .log_action(&NewAction {
            actor_kind: ActorKind::User,
            actor_id: Some(reviewer.id),
            actor_name: reviewer.name.clone(),
            target_kind: "post",
            target_id: item.post_id,
            action: resolution.as_str(),
            detail: serde_json::json!({
                "review_id": id,
                "reason": item.reason,
                "model_verdict": item.model_verdict,
                "model_agreed": agreed,
            }),
            public: true,
            created_at: now,
        })
        .await?;
    Ok(ReviewOutcome::Resolved {
        post: item.post_public_id,
        now_state,
        agreed,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppealOutcome {
    Filed,
    NotYours,
    /// Only a hidden post can be appealed; a pending one is already in the queue.
    NotHidden,
    Empty,
    TooLong {
        max: usize,
    },
}

/// The author of a hidden post asks for another look. Reopens the item with the text attached;
/// the original verdict stays on it. **Budget: 3 statements.**
pub async fn appeal<S: Store>(
    store: &S,
    post: &PublicId,
    author: &User,
    text: &str,
    now: Timestamp,
) -> StoreResult<AppealOutcome> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(AppealOutcome::Empty);
    }
    if text.chars().count() > MAX_APPEAL_CHARS {
        return Ok(AppealOutcome::TooLong {
            max: MAX_APPEAL_CHARS,
        });
    }
    let rp = match store.post_for_review(post).await {
        Ok(rp) => rp,
        Err(StoreError::NotFound) => return Ok(AppealOutcome::NotYours),
        Err(e) => return Err(e),
    };
    if rp.author_id != author.id {
        return Ok(AppealOutcome::NotYours);
    }
    if rp.state != PostState::Hidden {
        return Ok(AppealOutcome::NotHidden);
    }
    store
        .open_review(&NewReview {
            post_id: rp.id,
            space_id: rp.space_id,
            reason: ReviewReason::Appeal,
            verdict: None,
            appeal_text: Some(text.to_string()),
            opened_at: now,
        })
        .await?;
    store
        .log_action(&NewAction {
            actor_kind: ActorKind::User,
            actor_id: Some(author.id),
            actor_name: author.name.clone(),
            target_kind: "post",
            target_id: rp.id,
            action: "appeal",
            detail: serde_json::json!({}),
            public: true,
            created_at: now,
        })
        .await?;
    Ok(AppealOutcome::Filed)
}
