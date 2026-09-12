//! Run the moderation pipeline's two tiers over a sample of real Hacker News comments, to see
//! what the heuristics and a model actually do with prose nobody wrote as a test case.
//!
//! ```sh
//! ORKEY=sk-or-... cargo run -p notespace-core --example hn_moderate -- target/hn/hn-7d.ndjson
//! ORKEY=... cargo run -p notespace-core --example hn_moderate -- target/hn/hn-7d.ndjson \
//!     --n 40 --seed 7 --model openai/gpt-4o-mini
//! ORKEY=... cargo run -p notespace-core --example hn_moderate -- target/hn/hn-7d.ndjson \
//!     --guard meta-llama/llama-guard-4-12b --grep idiot
//! ```
//!
//! `--guard` puts a Llama Guard model in front of the instruct model, combined by
//! `moderation::layers`; `--grep` samples only comments containing the text, which is how to
//! look at the flag side of a corpus that is mostly fine.
//!
//! Sampling is seeded so a run is repeatable. Nothing is written anywhere; the output is the
//! table. Cost is a few hundred tokens per comment against whatever model is named.

use notespace_core::model::Role;
use notespace_core::moderation::classify::{Call, ClassifyError, ClassifyInput, Classifier, Verdict};
use notespace_core::moderation::heuristics::{triage, Signals, Triage};
use notespace_core::moderation::layers::Layered;
use notespace_core::moderation::policy::{Disposition, ModerationPolicy};
use notespace_core::moderation::providers;
use serde::Deserialize;
use std::io::BufRead;

/// One model on OpenRouter, instruct or Guard, behind the `Classifier` trait so it can be layered.
struct OpenRouter {
    key: String,
    model: String,
    guard: bool,
}

impl OpenRouter {
    fn post(&self, body: serde_json::Value) -> Result<serde_json::Value, ClassifyError> {
        let mut req = ureq::post(providers::OPENROUTER_URL);
        for (k, v) in providers::openai_compatible_headers(&self.key) {
            req = req.set(k, &v);
        }
        let resp = match req.send_json(body) {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => return Err(ClassifyError::Unavailable(e.to_string())),
        };
        resp.into_json()
            .map_err(|e| ClassifyError::Malformed(format!("unreadable body: {e}")))
    }
}

#[async_trait::async_trait(?Send)]
impl Classifier for OpenRouter {
    fn model(&self) -> &str {
        &self.model
    }
    async fn classify(&self, input: &ClassifyInput<'_>) -> Result<Verdict, ClassifyError> {
        if self.guard {
            let out = self.post(providers::openai_compatible_llama_guard_request(input, &self.model))?;
            providers::openai_compatible_llama_guard_parse(&out, &self.model)
        } else {
            let out = self.post(providers::openai_compatible_request(input, &self.model))?;
            providers::openai_compatible_parse(&out, &self.model)
        }
    }
}

/// Either a single model or a Guard in front of one.
enum Stack {
    One(OpenRouter),
    Guarded(Layered<OpenRouter, OpenRouter>),
}

#[async_trait::async_trait(?Send)]
impl Classifier for Stack {
    fn model(&self) -> &str {
        match self {
            Stack::One(c) => c.model(),
            Stack::Guarded(c) => c.model(),
        }
    }
    async fn classify(&self, input: &ClassifyInput<'_>) -> Result<Verdict, ClassifyError> {
        match self {
            Stack::One(c) => c.classify(input).await,
            Stack::Guarded(c) => c.classify(input).await,
        }
    }
}

/// The classifier is async for the Worker's sake; here the transport is blocking, so the
/// future is ready on first poll and this is enough to run it.
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn noop(_: *const ()) {}
    fn clone(p: *const ()) -> RawWaker {
        RawWaker::new(p, &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut f = std::pin::pin!(f);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

#[derive(Deserialize)]
struct Item {
    id: i64,
    author: Option<String>,
    text: Option<String>,
    #[serde(default)]
    children: Vec<Item>,
    #[serde(rename = "type")]
    kind: String,
}

struct Comment {
    id: i64,
    author: String,
    story: String,
    text: String,
}

fn flatten(item: &Item, story: &str, out: &mut Vec<Comment>) {
    if item.kind == "comment" {
        if let Some(text) = item.text.as_deref().filter(|t| !t.trim().is_empty()) {
            out.push(Comment {
                id: item.id,
                author: item.author.clone().unwrap_or_default(),
                story: story.to_string(),
                text: html_to_text(text),
            });
        }
    }
    for c in &item.children {
        flatten(c, story, out);
    }
}

/// HN comments are a small HTML subset: `<p>`, `<i>`, `<a>`, `<pre><code>`, and entities.
/// Enough of a rendering for a classifier to read.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find('>') else {
            out.push_str(&rest[i..]);
            rest = "";
            break;
        };
        let tag = &rest[i + 1..i + j];
        if tag.starts_with("p") || tag.starts_with("/p") || tag.starts_with("br") {
            out.push_str("\n\n");
        } else if let Some(href) = tag
            .strip_prefix("a ")
            .and_then(|a| a.split("href=\"").nth(1))
            .and_then(|h| h.split('"').next())
        {
            // The visible text of an HN link is the URL anyway; emit it once.
            out.push_str(href);
            // Skip the anchor body up to </a>.
            if let Some(k) = rest[i + j..].find("</a>") {
                rest = &rest[i + j + k + 4..];
                continue;
            }
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    out.replace("&quot;", "\"")
        .replace("&#x2F;", "/")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

/// A small deterministic PRNG so `--seed` means something.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut path = None;
    let mut n = 25usize;
    let mut seed = 1u64;
    let mut model = std::env::var("MOD_MODEL")
        .unwrap_or_else(|_| providers::OPENAI_COMPATIBLE_DEFAULT_MODEL.to_string());
    let mut dry = false;
    let mut grep: Option<String> = None;
    let mut guard: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--n" => n = args.next().and_then(|v| v.parse().ok()).unwrap_or(n),
            "--grep" => grep = args.next().map(|g| g.to_lowercase()),
            "--guard" => guard = args.next(),
            "--seed" => seed = args.next().and_then(|v| v.parse().ok()).unwrap_or(seed),
            "--model" => model = args.next().unwrap_or(model),
            "--dry-run" => dry = true,
            other => path = Some(other.to_string()),
        }
    }
    let Some(path) = path else {
        eprintln!(
            "usage: hn_moderate <hn.ndjson> [--n 25] [--seed 1] [--model id] [--guard id] [--grep text] [--dry-run]"
        );
        std::process::exit(2);
    };
    let key = std::env::var("ORKEY")
        .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
        .ok();
    if key.is_none() && !dry {
        eprintln!("set ORKEY (an OpenRouter key), or pass --dry-run for Tier 0 only");
        std::process::exit(2);
    }

    let file = std::fs::File::open(&path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(2)
    });
    let mut comments = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(story) = serde_json::from_str::<Item>(&line) else {
            continue;
        };
        let title = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(str::to_string))
            .unwrap_or_default();
        flatten(&story, &title, &mut comments);
    }
    eprintln!("{} comments in {path}", comments.len());
    // Sample only comments containing the text: random HN is mostly fine, and the flag side of
    // the classifier needs something to look at.
    if let Some(g) = &grep {
        comments.retain(|c| c.text.to_lowercase().contains(g.as_str()));
        eprintln!("{} contain {g:?}", comments.len());
    }

    // Seeded sample without replacement.
    let mut rng = Lcg(seed);
    let mut picked: Vec<Comment> = Vec::new();
    let mut idx: Vec<usize> = (0..comments.len()).collect();
    for _ in 0..n.min(comments.len()) {
        let i = (rng.next() as usize) % idx.len();
        picked.push(comments.swap_remove(idx.swap_remove(i)));
        idx = (0..comments.len()).collect();
    }

    let policy = ModerationPolicy {
        rules: Some(
            "A general technology discussion board in the spirit of Hacker News: technical \
             curiosity, startups, science. Be civil; disagreement is fine, contempt is not."
                .into(),
        ),
        ..Default::default()
    };
    const NOW: i64 = 1_800_000_000_000;
    const YEAR: i64 = 365 * 86_400_000;

    let back = OpenRouter {
        key: key.clone().unwrap_or_default(),
        model: model.clone(),
        guard: false,
    };
    let stack = match &guard {
        Some(g) => Stack::Guarded(Layered::new(
            OpenRouter {
                key: key.clone().unwrap_or_default(),
                model: g.clone(),
                guard: true,
            },
            back,
        )),
        None => Stack::One(back),
    };

    let mut held = 0;
    let mut by_call = [0u32; 3];
    let mut by_disposition = [0u32; 3];
    let mut failed = 0;
    println!("model: {}\n", stack.model());
    for c in &picked {
        // HN's dump does not say how old an account is; assume established, so the model is
        // judged on content and Tier 0's new-account rule stays out of the picture.
        let t = triage(
            &policy,
            &Signals {
                body_md: &c.text,
                author_created_at: NOW - YEAR,
                author_role: Role::Member,
                is_duplicate: false,
                now: NOW,
            },
        );
        let tier0 = match &t {
            Triage::Publish => "publish".to_string(),
            Triage::Hold(r) => {
                held += 1;
                format!(
                    "HOLD ({})",
                    r.iter().map(|r| r.label()).collect::<Vec<_>>().join("; ")
                )
            }
            Triage::Refuse(_) => "refuse".to_string(),
        };
        println!(
            "── https://news.ycombinator.com/item?id={} · {} · in {:?}",
            c.id,
            c.author,
            c.story.chars().take(60).collect::<String>()
        );
        let preview: String = c.text.split_whitespace().collect::<Vec<_>>().join(" ");
        println!("   {}", preview.chars().take(220).collect::<String>());
        println!("   tier0: {tier0}");
        if dry {
            continue;
        }
        let held_for = match &t {
            Triage::Hold(r) => r.clone(),
            _ => Vec::new(),
        };
        let input = ClassifyInput {
            body_md: &c.text,
            space_name: "Hacker News",
            space_rules: policy.rules.as_deref(),
            thread_title: &c.story,
            author_created_at: NOW - YEAR,
            now: NOW,
            held_for: &held_for,
        };
        match block_on(stack.classify(&input)) {
            Ok(v) => {
                let d = policy.decide(&v);
                by_call[match v.call {
                    Call::Clean => 0,
                    Call::Flag => 1,
                    Call::Unsure => 2,
                }] += 1;
                by_disposition[match d {
                    Disposition::Publish => 0,
                    Disposition::HoldForReview => 1,
                    Disposition::HideForReview => 2,
                }] += 1;
                let cats: Vec<&str> = v.categories.iter().map(|c| c.as_str()).collect();
                println!(
                    "   model: {} {:.2} {:?} -> {:?}\n          {}",
                    v.call.as_str(),
                    v.confidence,
                    cats,
                    d,
                    v.rationale.chars().take(160).collect::<String>()
                );
            }
            Err(e) => {
                println!("   model: {e}");
                failed += 1;
            }
        }
    }
    println!(
        "\n{} sampled · tier0 held {held} · model: clean {} / flag {} / unsure {} · \
         disposition: publish {} / hold {} / hide {} · failed {failed}",
        picked.len(),
        by_call[0],
        by_call[1],
        by_call[2],
        by_disposition[0],
        by_disposition[1],
        by_disposition[2]
    );
}
