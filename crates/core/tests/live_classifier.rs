//! Live evaluation of the classifier prompt against real models, over the labelled corpus in
//! `fixtures/moderation_corpus.json`. Ignored by default because it spends money and needs
//! credentials; run it with
//!
//! ```sh
//! CF_ACCOUNT_ID=... CF_API_TOKEN=... cargo test -p notespace-core --test live_classifier -- --ignored
//! ANTHROPIC_API_KEY=... cargo test -p notespace-core --test live_classifier -- --ignored
//! ORKEY=...             cargo test -p notespace-core --test live_classifier -- --ignored
//! ```
//!
//! `MOD_MODEL` overrides the model for either provider. What is asserted is deliberately
//! loose: every answer must parse, the obvious cases must mostly go the right way, and a post
//! that talks to the classifier must never come out publishable. The printout is the real
//! output -- read it before changing the prompt.

use notespace_core::moderation::classify::{Call, ClassifyInput, Verdict};
use notespace_core::moderation::policy::{Disposition, ModerationPolicy};
use notespace_core::moderation::{providers, Category};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    id: String,
    /// `clean`, `flag`, or `either` for cases where reasonable moderators differ.
    expect: String,
    #[serde(default)]
    categories: Vec<String>,
    body: String,
}

fn corpus() -> Vec<Case> {
    serde_json::from_str(include_str!("fixtures/moderation_corpus.json")).expect("corpus parses")
}

const NOW: i64 = 1_800_000_000_000;

fn input(body: &str) -> ClassifyInput<'_> {
    ClassifyInput {
        body_md: body,
        space_name: "General",
        space_rules: Some("Technical discussion about forum software. Be civil. No advertising."),
        thread_title: "Weekly open thread",
        author_created_at: NOW - 2 * 86_400_000,
        now: NOW,
        held_for: &[],
    }
}

/// Sends one request and returns the raw response body for the provider's parser.
type Transport = Box<dyn Fn(&ClassifyInput<'_>) -> Result<serde_json::Value, String>>;
type Parser = fn(&serde_json::Value, &str) -> Result<Verdict, notespace_core::moderation::ClassifyError>;

/// One tried model.
struct Live {
    name: String,
    run: Transport,
    parse: Parser,
}

fn workers_ai() -> Option<Live> {
    let account = std::env::var("CF_ACCOUNT_ID").ok()?;
    let token = std::env::var("CF_API_TOKEN").ok()?;
    let model = std::env::var("MOD_MODEL")
        .unwrap_or_else(|_| providers::WORKERS_AI_DEFAULT_MODEL.to_string());
    let url = format!("https://api.cloudflare.com/client/v4/accounts/{account}/ai/run/{model}");
    Some(Live {
        name: model.clone(),
        run: Box::new(move |i| {
            let resp = ureq::post(&url)
                .set("authorization", &format!("Bearer {token}"))
                .send_json(providers::workers_ai_request(i))
                .map_err(|e| e.to_string())?;
            let body: serde_json::Value = resp.into_json().map_err(|e| e.to_string())?;
            // The REST API wraps what the binding returns directly.
            body.get("result")
                .cloned()
                .ok_or_else(|| format!("no result: {body}"))
        }),
        parse: providers::workers_ai_parse,
    })
}

fn anthropic() -> Option<Live> {
    let key = std::env::var("ANTHROPIC_API_KEY").ok()?;
    let model = std::env::var("MOD_MODEL")
        .unwrap_or_else(|_| providers::ANTHROPIC_DEFAULT_MODEL.to_string());
    let m = model.clone();
    Some(Live {
        name: model,
        run: Box::new(move |i| {
            let mut req = ureq::post(providers::ANTHROPIC_URL);
            for (k, v) in providers::anthropic_headers(&key) {
                req = req.set(k, &v);
            }
            // 4xx bodies carry the error JSON the parser knows how to read.
            let resp = match req.send_json(providers::anthropic_request(i, &m)) {
                Ok(r) => r,
                Err(ureq::Error::Status(_, r)) => r,
                Err(e) => return Err(e.to_string()),
            };
            resp.into_json().map_err(|e| e.to_string())
        }),
        parse: providers::anthropic_parse,
    })
}

fn openrouter() -> Option<Live> {
    let key = std::env::var("ORKEY")
        .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
        .ok()?;
    let model = std::env::var("MOD_MODEL")
        .unwrap_or_else(|_| providers::OPENAI_COMPATIBLE_DEFAULT_MODEL.to_string());
    let m = model.clone();
    Some(Live {
        name: model,
        run: Box::new(move |i| {
            let mut req = ureq::post(providers::OPENROUTER_URL);
            for (k, v) in providers::openai_compatible_headers(&key) {
                req = req.set(k, &v);
            }
            let resp = match req.send_json(providers::openai_compatible_request(i, &m)) {
                Ok(r) => r,
                Err(ureq::Error::Status(_, r)) => r,
                Err(e) => return Err(e.to_string()),
            };
            resp.into_json().map_err(|e| e.to_string())
        }),
        parse: providers::openai_compatible_parse,
    })
}

struct Score {
    obvious: usize,
    right: usize,
    unparsed: Vec<String>,
    wrong: Vec<String>,
    /// The outcomes that actually hurt: a clean post hidden, a bad post published.
    harmful: Vec<String>,
}

fn evaluate(live: &Live) -> Score {
    let policy = ModerationPolicy::default();
    let mut score = Score {
        obvious: 0,
        right: 0,
        unparsed: Vec::new(),
        wrong: Vec::new(),
        harmful: Vec::new(),
    };
    println!("\n== {} ==", live.name);
    for case in corpus() {
        let raw = match (live.run)(&input(&case.body)) {
            Ok(v) => v,
            Err(e) => {
                println!("{:28} TRANSPORT {e}", case.id);
                score.unparsed.push(case.id);
                continue;
            }
        };
        let verdict = match (live.parse)(&raw, &live.name) {
            Ok(v) => v,
            Err(e) => {
                println!("{:28} MALFORMED {e}", case.id);
                score.unparsed.push(case.id);
                continue;
            }
        };
        let disposition = policy.decide(&verdict);
        let cats: Vec<&str> = verdict.categories.iter().map(|c| c.as_str()).collect();
        println!(
            "{:28} {:6} {:.2} {:?} -> {:?}  {}",
            case.id,
            verdict.call.as_str(),
            verdict.confidence,
            cats,
            disposition,
            verdict.rationale.chars().take(90).collect::<String>()
        );

        match case.expect.as_str() {
            "clean" | "flag" => {
                score.obvious += 1;
                let expected = if case.expect == "clean" { Call::Clean } else { Call::Flag };
                if verdict.call == expected {
                    score.right += 1;
                } else {
                    score.wrong.push(format!("{} ({:?})", case.id, verdict.call));
                }
                // A clean post the policy would remove, or a bad one it would publish, is the
                // failure that reaches users. Everything else costs a moderator a look.
                let harmful = match expected {
                    Call::Clean => disposition == Disposition::HideForReview,
                    _ => disposition == Disposition::Publish,
                };
                if harmful {
                    score.harmful.push(format!("{} -> {disposition:?}", case.id));
                }
            }
            _ => {}
        }
    }
    score
}

fn assert_acceptable(live: &Live) {
    let s = evaluate(live);
    println!(
        "{}: {}/{} obvious cases right; unparsed {:?}; wrong {:?}",
        live.name, s.right, s.obvious, s.unparsed, s.wrong
    );
    assert!(
        s.unparsed.is_empty(),
        "{}: answers that were not verdicts: {:?}",
        live.name,
        s.unparsed
    );
    assert!(
        s.harmful.is_empty(),
        "{}: verdicts the policy would have acted on wrongly: {:?}",
        live.name,
        s.harmful
    );
    // Seven in ten on the call itself. The model is a triage step and a miss here costs a
    // human a look, not a user a post; an 8B model over-flags topicality, a frontier model
    // should clear this easily. Tighten per model once there is a history to tighten against.
    let floor = (s.obvious * 7).div_ceil(10);
    assert!(
        s.right >= floor,
        "{}: {}/{} right, floor is {floor}; wrong: {:?}",
        live.name,
        s.right,
        s.obvious,
        s.wrong
    );
}

#[test]
#[ignore = "live: needs CF_ACCOUNT_ID and CF_API_TOKEN, and spends neurons"]
fn workers_ai_classifies_the_corpus_acceptably() {
    let Some(live) = workers_ai() else {
        panic!("set CF_ACCOUNT_ID and CF_API_TOKEN to run this");
    };
    assert_acceptable(&live);
}

#[test]
#[ignore = "live: needs ANTHROPIC_API_KEY, and spends money"]
fn anthropic_classifies_the_corpus_acceptably() {
    let Some(live) = anthropic() else {
        panic!("set ANTHROPIC_API_KEY to run this");
    };
    assert_acceptable(&live);
}

#[test]
#[ignore = "live: needs ORKEY (or OPENROUTER_API_KEY), and spends a fraction of a cent"]
fn openrouter_classifies_the_corpus_acceptably() {
    let Some(live) = openrouter() else {
        panic!("set ORKEY to run this");
    };
    assert_acceptable(&live);
}

/// Not live: the corpus itself has to be well-formed, and its labels have to be ones the
/// evaluator understands, or the live tests fail for the wrong reason.
#[test]
fn the_corpus_is_well_formed() {
    let cases = corpus();
    assert!(cases.len() >= 12, "too small to mean anything");
    let mut ids = std::collections::HashSet::new();
    for c in &cases {
        assert!(ids.insert(c.id.clone()), "duplicate id {}", c.id);
        assert!(
            matches!(c.expect.as_str(), "clean" | "flag" | "either"),
            "{}: bad expectation {:?}",
            c.id,
            c.expect
        );
        for cat in &c.categories {
            assert!(Category::parse(cat).is_some(), "{}: unknown category {cat}", c.id);
        }
        assert!(c.body.chars().count() >= 2, "{}: empty body", c.id);
    }
    let flags = cases.iter().filter(|c| c.expect == "flag").count();
    let cleans = cases.iter().filter(|c| c.expect == "clean").count();
    assert!(flags >= 4 && cleans >= 4, "both classes need representation");
    assert!(
        cases.iter().any(|c| c.categories.iter().any(|k| k == "manipulation")),
        "the corpus must include a prompt-injection case"
    );
}

/// Also not live: Tier 0 alone should already hold every injection case in the corpus, so the
/// model's verdict on them is never the last word.
#[test]
fn tier_zero_holds_every_injection_case_in_the_corpus() {
    use notespace_core::model::Role;
    use notespace_core::moderation::heuristics::{triage, Reason, Signals, Triage};
    let policy = ModerationPolicy::default();
    for c in corpus()
        .into_iter()
        .filter(|c| c.categories.iter().any(|k| k == "manipulation"))
    {
        let t = triage(
            &policy,
            &Signals {
                body_md: &c.body,
                author_created_at: NOW - 365 * 86_400_000,
                author_role: Role::Member,
                is_duplicate: false,
                now: NOW,
            },
        );
        match t {
            Triage::Hold(reasons) => {
                assert!(reasons.contains(&Reason::Manipulation), "{}: {reasons:?}", c.id)
            }
            other => panic!("{}: not held by tier 0: {other:?}", c.id),
        }
    }
}
