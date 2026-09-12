//! Composing classifiers. A safety model in front (Llama Guard: hate, violence, sexual content,
//! self-harm, doxxing -- and blind to spam) and an instruct model behind it that reads the
//! whole rule set. Two opinions are combined by [`combine`], which is pure and is where the
//! trust arithmetic lives.

use super::classify::{Call, ClassifyError, ClassifyInput, Classifier, Verdict};
use super::Category;

/// `front` runs first on every post; `back` runs on every post too, because the front cannot
/// see spam. What the front buys is corroboration on severe categories, which is what lets a
/// flag reach the hide threshold, and a second reader when the two disagree.
pub struct Layered<F, B> {
    pub front: F,
    pub back: B,
    model: String,
}

impl<F: Classifier, B: Classifier> Layered<F, B> {
    pub fn new(front: F, back: B) -> Self {
        let model = format!("{}+{}", front.model(), back.model());
        Layered { front, back, model }
    }
}

#[async_trait::async_trait(?Send)]
impl<F: Classifier, B: Classifier> Classifier for Layered<F, B> {
    fn model(&self) -> &str {
        &self.model
    }
    async fn classify(&self, input: &ClassifyInput<'_>) -> Result<Verdict, ClassifyError> {
        let front = self.front.classify(input).await;
        let back = self.back.classify(input).await;
        combine(front, back, &self.model)
    }
}

/// How two verdicts become one.
///
/// - Both flag: the flag stands, confidence is the noisy-or of the two (two independent readers
///   agreeing is stronger than either), categories are the union.
/// - Front flags, back does not: the flag stands at the front's confidence scaled down, so it
///   holds for a human but cannot hide on its own. The back read the same text and disagreed.
/// - Front clean, back flags only categories the front is trained to see (hate, violence,
///   sexual content, self-harm, illegal content, doxxing): the front's `safe` is a veto on
///   hiding. The flag stands, scaled down to a hold. A back flag for spam or harassment,
///   which the front cannot see, is unchanged.
/// - Front clean or unsure otherwise: the back's verdict, unchanged.
/// - One layer fails: the other's verdict, with the failure noted in the rationale. Both fail:
///   the back's error, since that is the layer that can see everything.
pub fn combine(
    front: Result<Verdict, ClassifyError>,
    back: Result<Verdict, ClassifyError>,
    model: &str,
) -> Result<Verdict, ClassifyError> {
    match (front, back) {
        (Ok(f), Ok(b)) => Ok(merge(f, b, model)),
        (Ok(mut f), Err(e)) => {
            // A lone front verdict is a safety read only; it must not publish spam it cannot see.
            if f.call == Call::Clean {
                f.call = Call::Unsure;
                f.confidence = 0.0;
            }
            f.rationale = format!("{} (second layer failed: {e})", f.rationale);
            f.model = model.to_string();
            Ok(f)
        }
        (Err(e), Ok(mut b)) => {
            b.rationale = format!("{} (safety layer failed: {e})", b.rationale);
            b.model = model.to_string();
            Ok(b)
        }
        (Err(_), Err(e)) => Err(e),
    }
}

/// Weight applied to a flag one layer made and the other contradicted: below the default hide
/// threshold whatever the flagging layer said.
pub const UNCORROBORATED: f64 = 0.8;

/// What a safety classifier is trained on, and so can be trusted to say `safe` about. Spam and
/// harassment are not in Llama Guard's taxonomy; `off_topic` and the rest are not violations.
pub fn guard_visible(c: &Category) -> bool {
    matches!(
        c,
        Category::Hate
            | Category::Violence
            | Category::SexualContent
            | Category::SelfHarm
            | Category::Illegal
            | Category::Doxxing
    )
}

fn merge(f: Verdict, b: Verdict, model: &str) -> Verdict {
    match (f.call, b.call) {
        (Call::Flag, Call::Flag) => {
            let mut categories: Vec<Category> = f.categories.clone();
            for c in &b.categories {
                if !categories.contains(c) {
                    categories.push(*c);
                }
            }
            Verdict {
                call: Call::Flag,
                confidence: (1.0 - (1.0 - f.confidence) * (1.0 - b.confidence)).clamp(0.0, 1.0),
                categories,
                rationale: format!("{} | {}", f.rationale, b.rationale),
                model: model.to_string(),
            }
        }
        (Call::Flag, _) => Verdict {
            call: Call::Flag,
            confidence: (f.confidence * UNCORROBORATED).clamp(0.0, 1.0),
            categories: f.categories,
            rationale: format!(
                "{} | second layer said {}: {}",
                f.rationale,
                b.call.as_str(),
                b.rationale
            ),
            model: model.to_string(),
        },
        (Call::Clean, Call::Flag)
            if !b.categories.is_empty() && b.categories.iter().all(guard_visible) =>
        {
            Verdict {
                confidence: (b.confidence * UNCORROBORATED).clamp(0.0, 1.0),
                rationale: format!("{} | safety layer said safe", b.rationale),
                model: model.to_string(),
                ..b
            }
        }
        _ => Verdict {
            model: model.to_string(),
            ..b
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moderation::policy::{Disposition, ModerationPolicy};

    fn v(call: Call, confidence: f64, categories: &[Category]) -> Verdict {
        Verdict {
            call,
            confidence,
            categories: categories.to_vec(),
            rationale: call.as_str().into(),
            model: "x".into(),
        }
    }

    #[test]
    fn two_flags_corroborate_into_a_stronger_one_with_the_union_of_categories() {
        let out = combine(
            Ok(v(Call::Flag, 0.85, &[Category::Hate])),
            Ok(v(Call::Flag, 0.8, &[Category::Harassment, Category::Hate])),
            "guard+llm",
        )
        .unwrap();
        assert_eq!(out.call, Call::Flag);
        assert!((out.confidence - 0.97).abs() < 1e-9);
        assert_eq!(out.categories, vec![Category::Hate, Category::Harassment]);
        assert_eq!(out.model, "guard+llm");
        assert_eq!(
            ModerationPolicy::default().decide(&out),
            Disposition::HideForReview,
            "agreement is what lets a flag hide"
        );
    }

    /// The point of the layer: neither model alone reaches the hide bar, both together do; a
    /// guard the back model contradicts cannot hide anything.
    #[test]
    fn an_uncorroborated_guard_flag_holds_but_cannot_hide() {
        let p = ModerationPolicy::default();
        let guard_alone = v(Call::Flag, 1.0, &[Category::Violence]);
        assert_eq!(p.decide(&guard_alone), Disposition::HideForReview, "premise");
        let out = combine(Ok(guard_alone), Ok(v(Call::Clean, 0.9, &[])), "m").unwrap();
        assert_eq!(out.call, Call::Flag);
        assert_eq!(out.confidence, UNCORROBORATED);
        assert_eq!(p.decide(&out), Disposition::HoldForReview);
        assert!(out.rationale.contains("second layer said clean"));
    }

    /// The live sample: an 8B model read a quoted insult as the poster's own and flagged
    /// `hate`/`violence` at 0.9 on a fine comment. A guard that said safe turns that into a hold.
    #[test]
    fn a_safe_guard_vetoes_hiding_on_the_categories_it_can_see_and_nothing_else() {
        let p = ModerationPolicy::default();
        let guard_safe = || v(Call::Clean, 0.85, &[]);

        let severe = v(Call::Flag, 0.9, &[Category::Hate, Category::Violence]);
        assert_eq!(p.decide(&severe), Disposition::HideForReview, "premise");
        let out = combine(Ok(guard_safe()), Ok(severe), "m").unwrap();
        assert_eq!(out.call, Call::Flag);
        assert!((out.confidence - 0.72).abs() < 1e-9);
        assert_eq!(p.decide(&out), Disposition::HoldForReview);
        assert!(out.rationale.contains("safety layer said safe"));

        // Spam is invisible to the guard, so its `safe` says nothing about it.
        let spam = v(Call::Flag, 0.95, &[Category::Spam]);
        let out = combine(Ok(guard_safe()), Ok(spam), "m").unwrap();
        assert_eq!(out.confidence, 0.95);
        assert_eq!(p.decide(&out), Disposition::HideForReview);

        // Mixed: one category the guard cannot see means the flag stands whole.
        let mixed = v(Call::Flag, 0.95, &[Category::Hate, Category::Harassment]);
        let out = combine(Ok(guard_safe()), Ok(mixed), "m").unwrap();
        assert_eq!(out.confidence, 0.95);

        // A flag with no category named is not vetoed here; the policy already refuses to hide it.
        let bare = v(Call::Flag, 0.95, &[]);
        let out = combine(Ok(guard_safe()), Ok(bare), "m").unwrap();
        assert_eq!(out.confidence, 0.95);
        assert_eq!(p.decide(&out), Disposition::HoldForReview);
    }

    #[test]
    fn a_clean_or_unsure_guard_defers_entirely_to_the_back() {
        let back = v(Call::Flag, 0.95, &[Category::Spam]);
        let out = combine(Ok(v(Call::Clean, 0.85, &[])), Ok(back.clone()), "m").unwrap();
        assert_eq!(out.call, Call::Flag);
        assert_eq!(out.confidence, 0.95);
        assert_eq!(out.categories, vec![Category::Spam]);
        let out = combine(Ok(v(Call::Unsure, 0.0, &[])), Ok(v(Call::Clean, 0.9, &[])), "m")
            .unwrap();
        assert_eq!(out.call, Call::Clean);
    }

    #[test]
    fn a_failed_layer_is_noted_and_the_other_verdict_stands() {
        let out = combine(
            Err(ClassifyError::Unavailable("down".into())),
            Ok(v(Call::Clean, 0.9, &[])),
            "m",
        )
        .unwrap();
        assert_eq!(out.call, Call::Clean);
        assert!(out.rationale.contains("safety layer failed"));

        // A guard that said clean cannot publish on its own: it cannot see spam.
        let out = combine(
            Ok(v(Call::Clean, 0.85, &[])),
            Err(ClassifyError::Unavailable("down".into())),
            "m",
        )
        .unwrap();
        assert_eq!(out.call, Call::Unsure);
        assert_eq!(out.confidence, 0.0);

        // But a guard that flagged still flags.
        let out = combine(
            Ok(v(Call::Flag, 0.85, &[Category::Hate])),
            Err(ClassifyError::Unavailable("down".into())),
            "m",
        )
        .unwrap();
        assert_eq!(out.call, Call::Flag);
        assert_eq!(out.categories, vec![Category::Hate]);

        assert!(matches!(
            combine(
                Err(ClassifyError::Unavailable("a".into())),
                Err(ClassifyError::Budget("b".into())),
                "m"
            ),
            Err(ClassifyError::Budget(_))
        ));
    }
}
