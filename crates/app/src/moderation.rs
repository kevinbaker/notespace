//! Moderation glue: the classifier the deployment configured, and the queue that hands held
//! posts to it. Request and response shapes come from `core`; this file carries bytes through
//! the platform.

use crate::platform::{HttpRequest, Job, Platform};
use notespace_core::id::PublicId;
use notespace_core::moderation::classify::{Classifier, ClassifyError, ClassifyInput, Verdict};
use notespace_core::moderation::heuristics::Reason;
use notespace_core::moderation::layers::Layered;
use notespace_core::moderation::pipeline::{self, ModerationQueue};
use notespace_core::moderation::providers;

/// `MOD_PROVIDER`: one of `workers_ai` (default when the platform has Workers AI), `llama_guard`,
/// `anthropic`, `openrouter`, `openrouter_guard`, `off` -- or two joined with `+`, a safety
/// model in front of an instruct model: `llama_guard+workers_ai`, `openrouter_guard+openrouter`.
/// `MOD_MODEL` overrides the instruct model, `MOD_GUARD_MODEL` the guard.
pub const PROVIDER_VAR: &str = "MOD_PROVIDER";
pub const MODEL_VAR: &str = "MOD_MODEL";
pub const GUARD_MODEL_VAR: &str = "MOD_GUARD_MODEL";
pub const ANTHROPIC_KEY_SECRET: &str = "ANTHROPIC_API_KEY";
pub const OPENROUTER_KEY_SECRET: &str = "OPENROUTER_API_KEY";

/// Posts a sweep classifies per run.
pub const SWEEP_LIMIT: u32 = 10;

/// Whichever provider the deployment configured. An enum rather than `Box<dyn Classifier>`
/// because the pipeline is generic over `C: Classifier`.
pub enum AnyClassifier<P: Platform> {
    WorkersAi(WorkersAi<P>),
    Anthropic(Anthropic<P>),
    OpenAiCompatible(OpenAiCompatible<P>),
    Layered(Box<Layered<AnyClassifier<P>, AnyClassifier<P>>>),
    /// No model configured. Every call fails as unavailable, which is the pipeline's path for
    /// "a human has to look": the post gets a review item and shows up in the queue. Without
    /// this a held post on a deployment with no classifier stayed pending and invisible forever.
    Human,
}

#[async_trait::async_trait(?Send)]
impl<P: Platform> Classifier for AnyClassifier<P> {
    fn model(&self) -> &str {
        match self {
            AnyClassifier::WorkersAi(c) => &c.model,
            AnyClassifier::Anthropic(c) => &c.model,
            AnyClassifier::OpenAiCompatible(c) => &c.model,
            AnyClassifier::Layered(l) => l.model(),
            AnyClassifier::Human => "none",
        }
    }
    async fn classify(&self, input: &ClassifyInput<'_>) -> Result<Verdict, ClassifyError> {
        match self {
            AnyClassifier::WorkersAi(c) => c.classify(input).await,
            AnyClassifier::Anthropic(c) => c.classify(input).await,
            AnyClassifier::OpenAiCompatible(c) => c.classify(input).await,
            AnyClassifier::Layered(l) => l.classify(input).await,
            AnyClassifier::Human => Err(ClassifyError::Unavailable(
                "no classifier is configured; a moderator has to look".into(),
            )),
        }
    }
}

/// The classifier this deployment configured; [`AnyClassifier::Human`] when it configured none.
pub fn resolve<P: Platform>(p: &P) -> Result<AnyClassifier<P>, String> {
    let provider = p.var(PROVIDER_VAR).unwrap_or_default();
    if let Some((front, back)) = provider.split_once('+') {
        let (Some(front), Some(back)) = (single(p, front.trim())?, single(p, back.trim())?) else {
            return Err(format!(
                "{PROVIDER_VAR}={provider:?}: both layers must be providers"
            ));
        };
        return Ok(AnyClassifier::Layered(Box::new(Layered::new(front, back))));
    }
    Ok(single(p, provider.trim())?.unwrap_or(AnyClassifier::Human))
}

fn single<P: Platform>(p: &P, provider: &str) -> Result<Option<AnyClassifier<P>>, String> {
    let model = p.var(MODEL_VAR);
    let guard_model = p.var(GUARD_MODEL_VAR);
    match provider {
        "off" | "none" => Ok(None),
        "openrouter" | "openrouter_guard" => {
            let key = p.secret(OPENROUTER_KEY_SECRET).ok_or_else(|| {
                format!("{PROVIDER_VAR}={provider} needs the {OPENROUTER_KEY_SECRET} secret")
            })?;
            let guard = provider == "openrouter_guard";
            Ok(Some(AnyClassifier::OpenAiCompatible(OpenAiCompatible {
                p: p.clone(),
                url: providers::OPENROUTER_URL.into(),
                key,
                model: if guard {
                    guard_model.unwrap_or_else(|| "meta-llama/llama-guard-4-12b".to_string())
                } else {
                    model.unwrap_or_else(|| providers::OPENAI_COMPATIBLE_DEFAULT_MODEL.to_string())
                },
                guard,
            })))
        }
        "anthropic" => {
            let key = p.secret(ANTHROPIC_KEY_SECRET).ok_or_else(|| {
                format!("{PROVIDER_VAR}=anthropic needs the {ANTHROPIC_KEY_SECRET} secret")
            })?;
            Ok(Some(AnyClassifier::Anthropic(Anthropic {
                p: p.clone(),
                key,
                model: model.unwrap_or_else(|| providers::ANTHROPIC_DEFAULT_MODEL.to_string()),
            })))
        }
        "llama_guard" => {
            if !p.has_ai() {
                return Err(format!("{PROVIDER_VAR}=llama_guard needs Workers AI"));
            }
            Ok(Some(AnyClassifier::WorkersAi(WorkersAi {
                p: p.clone(),
                model: guard_model.unwrap_or_else(|| providers::LLAMA_GUARD_MODEL.to_string()),
                guard: true,
            })))
        }
        // Default: Workers AI if the platform has it, otherwise nothing.
        _ if p.has_ai() => Ok(Some(AnyClassifier::WorkersAi(WorkersAi {
            p: p.clone(),
            model: model.unwrap_or_else(|| providers::WORKERS_AI_DEFAULT_MODEL.to_string()),
            guard: false,
        }))),
        "" => Ok(None),
        other => Err(format!(
            "{PROVIDER_VAR}={other:?} needs Workers AI, which this platform does not have"
        )),
    }
}

/// Failures from a binding or a fetch, sorted into retryable and not.
fn transport(e: impl std::fmt::Display) -> ClassifyError {
    let msg = e.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("429") || lower.contains("quota") || lower.contains("rate limit") {
        ClassifyError::Budget(msg)
    } else {
        ClassifyError::Unavailable(msg)
    }
}

pub struct WorkersAi<P: Platform> {
    p: P,
    model: String,
    /// Llama Guard speaks a different dialect from an instruct model.
    guard: bool,
}

#[async_trait::async_trait(?Send)]
impl<P: Platform> Classifier for WorkersAi<P> {
    fn model(&self) -> &str {
        &self.model
    }
    async fn classify(&self, input: &ClassifyInput<'_>) -> Result<Verdict, ClassifyError> {
        let request = if self.guard {
            providers::llama_guard_request(input)
        } else {
            providers::workers_ai_request(input)
        };
        let output = self
            .p
            .ai_run(&self.model, request)
            .await
            .ok_or_else(|| ClassifyError::Unavailable("no Workers AI on this platform".into()))?
            .map_err(transport)?;
        if self.guard {
            providers::llama_guard_parse(&output, &self.model)
        } else {
            providers::workers_ai_parse(&output, &self.model)
        }
    }
}

/// One JSON POST. Error bodies are JSON too and the provider's parser sorts them; only an
/// unreadable body is a transport failure.
async fn post_json<P: Platform>(
    p: &P,
    url: &str,
    headers: Vec<(&'static str, String)>,
    body: &serde_json::Value,
) -> Result<serde_json::Value, ClassifyError> {
    let mut req = HttpRequest::post(url, "application/json", body.to_string());
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let resp = p.http(req).await.map_err(transport)?;
    serde_json::from_str(&resp.body).map_err(|e| transport(format!("{e}: {}", resp.body)))
}

pub struct Anthropic<P: Platform> {
    p: P,
    key: String,
    model: String,
}

#[async_trait::async_trait(?Send)]
impl<P: Platform> Classifier for Anthropic<P> {
    fn model(&self) -> &str {
        &self.model
    }
    async fn classify(&self, input: &ClassifyInput<'_>) -> Result<Verdict, ClassifyError> {
        let output = post_json(
            &self.p,
            providers::ANTHROPIC_URL,
            providers::anthropic_headers(&self.key),
            &providers::anthropic_request(input, &self.model),
        )
        .await?;
        providers::anthropic_parse(&output, &self.model)
    }
}

/// OpenRouter, OpenAI, or anything speaking `/chat/completions`; instruct or Llama Guard.
pub struct OpenAiCompatible<P: Platform> {
    p: P,
    url: String,
    key: String,
    model: String,
    guard: bool,
}

#[async_trait::async_trait(?Send)]
impl<P: Platform> Classifier for OpenAiCompatible<P> {
    fn model(&self) -> &str {
        &self.model
    }
    async fn classify(&self, input: &ClassifyInput<'_>) -> Result<Verdict, ClassifyError> {
        let body = if self.guard {
            providers::openai_compatible_llama_guard_request(input, &self.model)
        } else {
            providers::openai_compatible_request(input, &self.model)
        };
        let output = post_json(
            &self.p,
            &self.url,
            providers::openai_compatible_headers(&self.key),
            &body,
        )
        .await?;
        if self.guard {
            providers::openai_compatible_llama_guard_parse(&output, &self.model)
        } else {
            providers::openai_compatible_parse(&output, &self.model)
        }
    }
}

/// The write path's view of the queue: hand the post over and carry on. A platform without a
/// queue says so and the sweep does the work.
pub struct Queue<P: Platform>(pub P);

#[async_trait::async_trait(?Send)]
impl<P: Platform> ModerationQueue for Queue<P> {
    async fn enqueue(&self, post: &PublicId, reasons: &[Reason]) -> Result<(), String> {
        self.0
            .enqueue(Job {
                post: post.encode(),
                reasons: reasons.to_vec(),
            })
            .await
            .map(|_| ())
    }
}

/// What a queue consumer does with one job. `Ok(true)` is done; `Ok(false)` asks for a retry;
/// `Err` is a job that can never succeed and should be dropped.
pub async fn process<P: Platform>(
    p: &P,
    classifier: &AnyClassifier<P>,
    job: &Job,
) -> Result<bool, String> {
    let post = PublicId::parse(&job.post).map_err(|e| format!("bad id {:?}: {e}", job.post))?;
    match pipeline::process_post(p.store(), classifier, &post, &job.reasons, p.now_ms()).await {
        Ok(outcome) => {
            p.log(&format!("moderation: {post} -> {outcome:?}"));
            Ok(true)
        }
        Err(e) => {
            p.log(&format!("moderation: {post} store error, will retry: {e}"));
            Ok(false)
        }
    }
}

/// The safety net: anything still pending past the grace period, that no human has yet, is
/// classified here -- whether the queue is misconfigured, absent, or lost a message.
pub async fn sweep<P: Platform>(p: &P) {
    let classifier = match resolve(p) {
        Ok(c) => c,
        Err(why) => {
            p.log(&format!("sweep: classifier misconfigured: {why}"));
            return;
        }
    };
    match pipeline::drain(p.store(), &classifier, p.now_ms(), SWEEP_LIMIT).await {
        Ok(results) => {
            for (post, r) in results {
                p.log(&format!("sweep: {post} -> {r:?}"));
            }
        }
        Err(e) => p.log(&format!("sweep: {e}")),
    }
}
