//! Per-space moderation policy: what holds a post, and how far the model is trusted.
//!
//! Lives in `space.config` under `"moderation"`, so a preset can set it without a schema
//! change. Every field has a default and unknown fields are ignored, so an old config still
//! parses under a newer build.

use super::classify::{Call, Verdict};
use super::AgreementStats;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModerationPolicy {
    /// Off means every post publishes immediately and nothing is classified. Reports still work.
    pub enabled: bool,
    /// Accounts younger than this have every post held for classification. 0 disables.
    pub new_account_hours: i64,
    /// Links per post before a post is held, for established accounts.
    pub max_links: u32,
    /// The same, for accounts inside `new_account_hours`.
    pub max_links_new: u32,
    /// Case-insensitive substrings that hold a post outright.
    pub blocklist: Vec<String>,
    /// Distinct reporters needed to pull a visible post back for review.
    pub report_threshold: u32,
    /// A `clean` verdict at or above this confidence publishes without a human.
    pub publish_confidence: f64,
    /// A `flag` verdict at or above this confidence hides pending review. Below it the post
    /// stays held but visible to no one, and a human decides.
    pub hide_confidence: f64,
    /// A post identical to one the same author made inside this window is refused.
    pub duplicate_window_hours: i64,
    /// Whether the action log is served at `/modlog`.
    pub public_modlog: bool,
    /// Space rules, in prose, handed to the classifier so "off topic" means something here.
    pub rules: Option<String>,
}

impl Default for ModerationPolicy {
    fn default() -> Self {
        ModerationPolicy {
            enabled: true,
            new_account_hours: 72,
            max_links: 5,
            max_links_new: 1,
            blocklist: Vec::new(),
            report_threshold: 3,
            publish_confidence: 0.75,
            hide_confidence: 0.9,
            duplicate_window_hours: 24,
            public_modlog: true,
            rules: None,
        }
    }
}

/// Where a classified post goes next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Confident clean: make it visible.
    Publish,
    /// Confident bad: hide it, and queue it for a human to confirm.
    HideForReview,
    /// Anything else: stays held, and a human decides.
    HoldForReview,
}

impl ModerationPolicy {
    /// Never fails: a broken `config` is treated as an empty one, because a typo in a space's
    /// settings must not take the space offline.
    pub fn from_config(config: &str) -> Self {
        serde_json::from_str::<serde_json::Value>(config)
            .ok()
            .and_then(|v| v.get("moderation").cloned())
            .and_then(|m| serde_json::from_value::<ModerationPolicy>(m).ok())
            .unwrap_or_default()
            .clamped()
    }

    /// Confidence bounds are probabilities; anything else in the config is a mistake, not a
    /// setting.
    fn clamped(mut self) -> Self {
        self.publish_confidence = self.publish_confidence.clamp(0.0, 1.0);
        self.hide_confidence = self.hide_confidence.clamp(0.0, 1.0);
        self
    }

    pub fn new_account_ms(&self) -> i64 {
        self.new_account_hours.max(0) * 3_600_000
    }

    pub fn duplicate_window_ms(&self) -> i64 {
        self.duplicate_window_hours.max(0) * 3_600_000
    }

    /// Metamoderation: humans overruling the model tightens the thresholds, and consistent
    /// agreement loosens them a little. Needs a sample first, so a new space runs on the
    /// configured numbers.
    pub fn effective(&self, agreement: &AgreementStats) -> ModerationPolicy {
        let mut p = self.clone();
        let Some(rate) = agreement.disagreement_rate() else {
            return p;
        };
        let n = agreement.sample();
        if n >= Self::TIGHTEN_SAMPLE && rate > Self::TIGHTEN_ABOVE {
            // Auto-publish is the dangerous direction, so it moves further.
            p.publish_confidence = (p.publish_confidence + 0.15).min(1.0);
            p.hide_confidence = (p.hide_confidence + 0.05).min(1.0);
        } else if n >= Self::LOOSEN_SAMPLE && rate < Self::LOOSEN_BELOW {
            p.publish_confidence = (p.publish_confidence - 0.05).max(0.5);
        }
        p
    }

    /// Resolved reviews before disagreement is allowed to tighten anything.
    pub const TIGHTEN_SAMPLE: u32 = 10;
    /// Disagreement rate above which the model is trusted less.
    pub const TIGHTEN_ABOVE: f64 = 0.3;
    /// Sample before agreement is allowed to loosen anything: more, because loosening is the
    /// direction that publishes bad posts.
    pub const LOOSEN_SAMPLE: u32 = 30;
    pub const LOOSEN_BELOW: f64 = 0.05;

    /// A `manipulation` category is never auto-published: text aimed at the classifier is
    /// exactly the text whose `clean` verdict cannot be trusted. A flag hides only when it
    /// names a severe category: "off topic" at confidence 1.0 is still an opinion.
    pub fn decide(&self, v: &Verdict) -> Disposition {
        let manipulated = v.categories.contains(&super::Category::Manipulation);
        let severe = v.categories.iter().any(|c| c.is_severe());
        match v.call {
            Call::Clean if !manipulated && v.confidence >= self.publish_confidence => {
                Disposition::Publish
            }
            Call::Flag if severe && v.confidence >= self.hide_confidence => {
                Disposition::HideForReview
            }
            _ => Disposition::HoldForReview,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moderation::Category;

    fn verdict(call: Call, confidence: f64, categories: &[Category]) -> Verdict {
        Verdict {
            call,
            confidence,
            categories: categories.to_vec(),
            rationale: String::new(),
            model: "test".into(),
        }
    }

    #[test]
    fn a_missing_or_broken_config_is_the_default_policy() {
        assert_eq!(ModerationPolicy::from_config("{}"), ModerationPolicy::default());
        assert_eq!(ModerationPolicy::from_config(""), ModerationPolicy::default());
        assert_eq!(
            ModerationPolicy::from_config("{not json"),
            ModerationPolicy::default()
        );
        assert_eq!(
            ModerationPolicy::from_config(r#"{"moderation": 42}"#),
            ModerationPolicy::default()
        );
    }

    #[test]
    fn a_partial_config_overrides_only_what_it_names() {
        let p = ModerationPolicy::from_config(
            r#"{"ranking":"gravity","moderation":{"report_threshold":1,"blocklist":["buy now"],"future_field":true}}"#,
        );
        assert_eq!(p.report_threshold, 1);
        assert_eq!(p.blocklist, vec!["buy now".to_string()]);
        assert_eq!(p.new_account_hours, ModerationPolicy::default().new_account_hours);
    }

    #[test]
    fn confidences_are_clamped_to_probabilities() {
        let p = ModerationPolicy::from_config(
            r#"{"moderation":{"publish_confidence":7,"hide_confidence":-1}}"#,
        );
        assert_eq!(p.publish_confidence, 1.0);
        assert_eq!(p.hide_confidence, 0.0);
    }

    #[test]
    fn only_a_confident_clean_publishes_and_only_a_confident_flag_hides() {
        let p = ModerationPolicy::default();
        assert_eq!(p.decide(&verdict(Call::Clean, 0.95, &[])), Disposition::Publish);
        assert_eq!(
            p.decide(&verdict(Call::Clean, 0.5, &[])),
            Disposition::HoldForReview
        );
        assert_eq!(
            p.decide(&verdict(Call::Flag, 0.95, &[Category::Spam])),
            Disposition::HideForReview
        );
        assert_eq!(
            p.decide(&verdict(Call::Flag, 0.6, &[Category::Spam])),
            Disposition::HoldForReview
        );
        assert_eq!(
            p.decide(&verdict(Call::Unsure, 1.0, &[])),
            Disposition::HoldForReview
        );
    }

    /// An 8B model flagged ordinary technical posts as off-topic at confidence 1.0 in the
    /// live eval. That must never remove a post on its own.
    #[test]
    fn a_flag_with_only_mild_categories_holds_but_never_hides() {
        let p = ModerationPolicy::default();
        assert_eq!(
            p.decide(&verdict(Call::Flag, 1.0, &[Category::OffTopic])),
            Disposition::HoldForReview
        );
        assert_eq!(
            p.decide(&verdict(Call::Flag, 1.0, &[Category::Other])),
            Disposition::HoldForReview
        );
        assert_eq!(
            p.decide(&verdict(Call::Flag, 1.0, &[])),
            Disposition::HoldForReview,
            "a flag with no category named is not a reason to hide"
        );
        assert_eq!(
            p.decide(&verdict(Call::Flag, 1.0, &[Category::OffTopic, Category::Spam])),
            Disposition::HideForReview
        );
    }

    /// The model never gets the last word on a post that was talking to it.
    #[test]
    fn a_manipulation_category_blocks_auto_publish() {
        let p = ModerationPolicy::default();
        assert_eq!(
            p.decide(&verdict(Call::Clean, 1.0, &[Category::Manipulation])),
            Disposition::HoldForReview
        );
    }

    #[test]
    fn disagreement_tightens_and_agreement_loosens_only_with_a_sample() {
        let p = ModerationPolicy::default();
        let none = p.effective(&AgreementStats::default());
        assert_eq!(none, p, "no data changes nothing");

        let small_bad = AgreementStats {
            agreed: 1,
            disagreed: 4,
        };
        assert_eq!(p.effective(&small_bad), p, "five reviews are not a sample");

        let bad = AgreementStats {
            agreed: 5,
            disagreed: 5,
        };
        let tightened = p.effective(&bad);
        assert!(tightened.publish_confidence > p.publish_confidence);
        assert!(tightened.hide_confidence > p.hide_confidence);

        let good_but_few = AgreementStats {
            agreed: 20,
            disagreed: 0,
        };
        assert_eq!(p.effective(&good_but_few), p, "loosening needs a bigger sample");

        let good = AgreementStats {
            agreed: 40,
            disagreed: 1,
        };
        let loosened = p.effective(&good);
        assert!(loosened.publish_confidence < p.publish_confidence);
        assert_eq!(loosened.hide_confidence, p.hide_confidence, "hiding never loosens");
    }

    #[test]
    fn tightening_never_exceeds_certainty_and_loosening_has_a_floor() {
        let mut p = ModerationPolicy {
            publish_confidence: 0.95,
            hide_confidence: 0.99,
            ..Default::default()
        };
        let t = p.effective(&AgreementStats {
            agreed: 0,
            disagreed: 20,
        });
        assert_eq!(t.publish_confidence, 1.0);
        assert_eq!(t.hide_confidence, 1.0);

        p.publish_confidence = 0.5;
        let l = p.effective(&AgreementStats {
            agreed: 100,
            disagreed: 0,
        });
        assert_eq!(l.publish_confidence, 0.5, "never below a coin flip");
    }
}
