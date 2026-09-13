//! Moderation. Runs off the request path except for the free heuristics.
//!
//! The model triages; it never delivers a final verdict. Every model call is logged with its
//! inputs and output as an action by an actor, so a human can rate it later, and those ratings
//! feed back into how far the model is trusted per space.

pub mod classify;
pub mod heuristics;
pub mod layers;
pub mod pipeline;
pub mod policy;
pub mod providers;

use crate::id::PublicId;
use crate::model::{PostId, SpaceId, Timestamp, UserId};
use serde::{Deserialize, Serialize};

pub use classify::{Classifier, ClassifyError, ClassifyInput, Verdict};
pub use heuristics::{Reason, Signals, Triage};
pub use layers::Layered;
pub use pipeline::{ModerationQueue, NoQueue};
pub use policy::{Disposition, ModerationPolicy};

/// What a post was judged to contain. Forum-shaped rather than LLM-safety-shaped: spam and
/// harassment are most of the work and neither is a Llama Guard hazard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Spam,
    Harassment,
    Hate,
    Violence,
    SexualContent,
    SelfHarm,
    Illegal,
    /// Personal information about someone else.
    Doxxing,
    OffTopic,
    /// Text addressed to the classifier rather than to readers.
    Manipulation,
    Other,
}

impl Category {
    pub const ALL: [Category; 11] = [
        Category::Spam,
        Category::Harassment,
        Category::Hate,
        Category::Violence,
        Category::SexualContent,
        Category::SelfHarm,
        Category::Illegal,
        Category::Doxxing,
        Category::OffTopic,
        Category::Manipulation,
        Category::Other,
    ];

    pub const fn as_str(&self) -> &'static str {
        match self {
            Category::Spam => "spam",
            Category::Harassment => "harassment",
            Category::Hate => "hate",
            Category::Violence => "violence",
            Category::SexualContent => "sexual_content",
            Category::SelfHarm => "self_harm",
            Category::Illegal => "illegal",
            Category::Doxxing => "doxxing",
            Category::OffTopic => "off_topic",
            Category::Manipulation => "manipulation",
            Category::Other => "other",
        }
    }

    /// Whether a flag for this alone may hide a post. Off-topic and "other" are judgement calls
    /// a human makes; a model's confidence in them is not a reason to remove anything.
    pub fn is_severe(&self) -> bool {
        !matches!(
            self,
            Category::OffTopic | Category::Other | Category::Manipulation
        )
    }

    /// Lenient: case and separator insensitive, because a model does not always echo the
    /// enum it was given. Unknown text is `None`, not `Other`, so noise is dropped rather
    /// than counted.
    pub fn parse(s: &str) -> Option<Category> {
        let norm: String = s
            .trim()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        Category::ALL
            .iter()
            .copied()
            .find(|c| c.as_str().replace('_', "") == norm)
            .or(match norm.as_str() {
                "sexual" | "nsfw" | "adult" => Some(Category::SexualContent),
                "selfharm" | "suicide" => Some(Category::SelfHarm),
                "privacy" | "pii" | "dox" => Some(Category::Doxxing),
                "abuse" | "bullying" | "insult" => Some(Category::Harassment),
                "hatespeech" => Some(Category::Hate),
                "threat" | "violent" => Some(Category::Violence),
                "advertising" | "scam" | "promotion" => Some(Category::Spam),
                "offtopic" | "irrelevant" => Some(Category::OffTopic),
                "promptinjection" | "jailbreak" => Some(Category::Manipulation),
                _ => None,
            })
    }
}

impl core::fmt::Display for Category {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who did a thing. The model is an actor like any other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    User,
    Model,
    Rule,
    System,
}

impl ActorKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            ActorKind::User => "user",
            ActorKind::Model => "model",
            ActorKind::Rule => "rule",
            ActorKind::System => "system",
        }
    }
    pub fn parse(s: &str) -> ActorKind {
        match s {
            "user" => ActorKind::User,
            "model" => ActorKind::Model,
            "rule" => ActorKind::Rule,
            _ => ActorKind::System,
        }
    }
}

/// One row to append to the action log.
#[derive(Debug, Clone, PartialEq)]
pub struct NewAction {
    pub actor_kind: ActorKind,
    pub actor_id: Option<UserId>,
    pub actor_name: String,
    pub target_kind: &'static str,
    pub target_id: i64,
    pub action: &'static str,
    /// JSON. Private: the public log never shows it.
    pub detail: serde_json::Value,
    pub public: bool,
    pub created_at: Timestamp,
}

/// A row of the public log, as read back. `detail` is deliberately absent.
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub id: i64,
    pub actor_kind: ActorKind,
    pub actor_name: String,
    pub target_kind: String,
    /// The target's public id when it has one, so the log can link to it.
    pub target_public_id: Option<PublicId>,
    pub action: String,
    pub created_at: Timestamp,
}

/// A row of the full log, for the admin page: the public row plus what it hides.
#[derive(Debug, Clone, PartialEq)]
pub struct LogDetail {
    pub entry: LogEntry,
    pub public: bool,
    pub detail: String,
}

/// Why a post is in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewReason {
    Classifier,
    Reports,
    Rule,
    Appeal,
    /// The classifier could not be reached; a human has to look instead.
    Error,
}

impl ReviewReason {
    pub const fn as_str(&self) -> &'static str {
        match self {
            ReviewReason::Classifier => "classifier",
            ReviewReason::Reports => "reports",
            ReviewReason::Rule => "rule",
            ReviewReason::Appeal => "appeal",
            ReviewReason::Error => "error",
        }
    }
    pub fn parse(s: &str) -> ReviewReason {
        match s {
            "classifier" => ReviewReason::Classifier,
            "reports" => ReviewReason::Reports,
            "rule" => ReviewReason::Rule,
            "appeal" => ReviewReason::Appeal,
            _ => ReviewReason::Error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Approve,
    Reject,
}

impl Resolution {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Resolution::Approve => "approve",
            Resolution::Reject => "reject",
        }
    }
    pub fn parse(s: &str) -> Option<Resolution> {
        match s {
            "approve" => Some(Resolution::Approve),
            "reject" => Some(Resolution::Reject),
            _ => None,
        }
    }
}

/// Open or reopen a queue item. Upsert on `post_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct NewReview {
    pub post_id: PostId,
    pub space_id: SpaceId,
    pub reason: ReviewReason,
    pub verdict: Option<Verdict>,
    pub appeal_text: Option<String>,
    pub opened_at: Timestamp,
}

/// A queue row with what the reviewer needs to decide, so the page is one query.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewItem {
    pub id: i64,
    pub post_id: PostId,
    pub post_public_id: PublicId,
    pub thread_public_id: PublicId,
    pub thread_title: String,
    pub space_id: SpaceId,
    pub space_name: String,
    pub author_id: UserId,
    pub author_name: String,
    /// Already sanitized at write time.
    pub body_html: String,
    pub post_state: crate::model::PostState,
    pub reason: ReviewReason,
    pub model_verdict: Option<classify::Call>,
    pub model_confidence: Option<f64>,
    pub model_categories: Vec<Category>,
    pub appeal_text: Option<String>,
    pub opened_at: Timestamp,
    pub resolved: bool,
}

/// A report is a signal with negative weight and a reason.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSignal {
    pub post_id: PostId,
    pub user_id: UserId,
    pub kind: &'static str,
    pub weight: f64,
    pub reason: Option<String>,
    pub created_at: Timestamp,
}

pub const SIGNAL_REPORT: &str = "report";

/// Result of adding a report: whether this one was new, and the total now standing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportTally {
    pub added: bool,
    pub count: u32,
}

/// What the write path needs to know before deciding whether to hold a post. One statement.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteContext {
    pub space_id: SpaceId,
    /// Raw JSON from `space.config`; the policy is parsed from it by [`ModerationPolicy::from_config`].
    pub space_config: String,
    pub thread_state: crate::model::ThreadState,
    pub author_created_at: Timestamp,
    pub author_role: crate::model::Role,
}

/// A post as the consumer and the reviewer see it: with its markdown, its space's policy and
/// what is known about its author.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewPost {
    pub id: PostId,
    pub public_id: PublicId,
    pub thread_public_id: PublicId,
    pub thread_title: String,
    pub space_id: SpaceId,
    pub space_name: String,
    pub space_config: String,
    pub author_id: UserId,
    pub author_name: String,
    pub author_created_at: Timestamp,
    pub body_md: String,
    pub created_at: Timestamp,
    pub state: crate::model::PostState,
}

/// How often humans have agreed with the model in a space. Feeds [`ModerationPolicy::effective`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AgreementStats {
    pub agreed: u32,
    pub disagreed: u32,
}

impl AgreementStats {
    pub fn sample(&self) -> u32 {
        self.agreed + self.disagreed
    }

    /// `None` until there is anything to divide by.
    pub fn disagreement_rate(&self) -> Option<f64> {
        let n = self.sample();
        (n > 0).then(|| self.disagreed as f64 / n as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_round_trip_their_string_forms() {
        for c in Category::ALL {
            assert_eq!(Category::parse(c.as_str()), Some(c), "{c:?}");
            let via_serde = serde_json::to_string(&c).unwrap();
            assert_eq!(
                via_serde.trim_matches('"'),
                c.as_str(),
                "serde disagrees for {c:?}"
            );
        }
    }

    #[test]
    fn category_parsing_is_lenient_about_spelling_but_not_meaning() {
        assert_eq!(
            Category::parse("Sexual Content"),
            Some(Category::SexualContent)
        );
        assert_eq!(Category::parse("SELF-HARM"), Some(Category::SelfHarm));
        assert_eq!(Category::parse("hate speech"), Some(Category::Hate));
        assert_eq!(
            Category::parse("prompt injection"),
            Some(Category::Manipulation)
        );
        assert_eq!(
            Category::parse("banana"),
            None,
            "unknown labels are dropped, not Other"
        );
        assert_eq!(Category::parse(""), None);
    }

    #[test]
    fn agreement_rate_is_absent_rather_than_nan_on_no_data() {
        assert_eq!(AgreementStats::default().disagreement_rate(), None);
        let s = AgreementStats {
            agreed: 3,
            disagreed: 1,
        };
        assert_eq!(s.disagreement_rate(), Some(0.25));
    }
}
