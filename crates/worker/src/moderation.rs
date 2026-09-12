//! Moderation on the Worker: the classifier behind a binding, and the Queue that wakes the
//! consumer. Request and response shapes come from `core`; this file only carries bytes.

use notespace_core::id::PublicId;
use notespace_core::moderation::classify::{ClassifyError, ClassifyInput, Classifier, Verdict};
use notespace_core::moderation::heuristics::Reason;
use notespace_core::moderation::layers::Layered;
use notespace_core::moderation::pipeline::ModerationQueue;
use notespace_core::moderation::providers;
use serde::{Deserialize, Serialize};
use worker::{Env, Fetch, Headers, Method, Request, RequestInit};

/// Kept in step with wrangler.toml by `the_moderation_binding_names_match_wrangler_toml`.
pub const AI_BINDING: &str = "AI";
pub const QUEUE_BINDING: &str = "MODERATION";

/// `MOD_PROVIDER`: one of `workers_ai` (default when the `AI` binding exists), `llama_guard`,
/// `anthropic`, `openrouter`, `openrouter_guard`, `off` -- or two joined with `+`, a safety
/// model in front of an instruct model: `llama_guard+workers_ai`, `openrouter_guard+openrouter`.
/// `MOD_MODEL` overrides the instruct model, `MOD_GUARD_MODEL` the guard.
pub const PROVIDER_VAR: &str = "MOD_PROVIDER";
pub const MODEL_VAR: &str = "MOD_MODEL";
pub const GUARD_MODEL_VAR: &str = "MOD_GUARD_MODEL";
pub const ANTHROPIC_KEY_SECRET: &str = "ANTHROPIC_API_KEY";
pub const OPENROUTER_KEY_SECRET: &str = "OPENROUTER_API_KEY";

/// One queue message: which post, and what Tier 0 held it for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub post: String,
    #[serde(default)]
    pub reasons: Vec<Reason>,
}

/// Whichever provider the deployment configured. An enum rather than `Box<dyn Classifier>`
/// because the pipeline is generic over `C: Classifier`.
pub enum AnyClassifier {
    WorkersAi(WorkersAi),
    Anthropic(Anthropic),
    OpenAiCompatible(OpenAiCompatible),
    Layered(Box<Layered<AnyClassifier, AnyClassifier>>),
}

#[async_trait::async_trait(?Send)]
impl Classifier for AnyClassifier {
    fn model(&self) -> &str {
        match self {
            AnyClassifier::WorkersAi(c) => &c.model,
            AnyClassifier::Anthropic(c) => &c.model,
            AnyClassifier::OpenAiCompatible(c) => &c.model,
            AnyClassifier::Layered(l) => l.model(),
        }
    }
    async fn classify(&self, input: &ClassifyInput<'_>) -> Result<Verdict, ClassifyError> {
        match self {
            AnyClassifier::WorkersAi(c) => c.classify(input).await,
            AnyClassifier::Anthropic(c) => c.classify(input).await,
            AnyClassifier::OpenAiCompatible(c) => c.classify(input).await,
            AnyClassifier::Layered(l) => l.classify(input).await,
        }
    }
}

/// `None` means moderation is off for this deployment: held posts wait for a human.
pub fn resolve(env: &Env) -> Result<Option<AnyClassifier>, String> {
    let provider = env
        .var(PROVIDER_VAR)
        .map(|v| v.to_string())
        .unwrap_or_default();
    if let Some((front, back)) = provider.split_once('+') {
        let (Some(front), Some(back)) = (single(env, front.trim())?, single(env, back.trim())?)
        else {
            return Err(format!("{PROVIDER_VAR}={provider:?}: both layers must be providers"));
        };
        return Ok(Some(AnyClassifier::Layered(Box::new(Layered::new(
            front, back,
        )))));
    }
    single(env, provider.trim())
}

fn single(env: &Env, provider: &str) -> Result<Option<AnyClassifier>, String> {
    let model = env.var(MODEL_VAR).map(|v| v.to_string()).ok();
    let guard_model = env.var(GUARD_MODEL_VAR).map(|v| v.to_string()).ok();
    match provider {
        "off" | "none" => Ok(None),
        "openrouter" | "openrouter_guard" => {
            let key = env
                .secret(OPENROUTER_KEY_SECRET)
                .map(|s| s.to_string())
                .map_err(|_| format!("{PROVIDER_VAR}={provider} needs the {OPENROUTER_KEY_SECRET} secret"))?;
            let guard = provider == "openrouter_guard";
            Ok(Some(AnyClassifier::OpenAiCompatible(OpenAiCompatible {
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
            let key = env
                .secret(ANTHROPIC_KEY_SECRET)
                .map(|s| s.to_string())
                .map_err(|_| format!("{PROVIDER_VAR}=anthropic needs the {ANTHROPIC_KEY_SECRET} secret"))?;
            Ok(Some(AnyClassifier::Anthropic(Anthropic {
                key,
                model: model.unwrap_or_else(|| providers::ANTHROPIC_DEFAULT_MODEL.to_string()),
            })))
        }
        "llama_guard" => {
            let ai = env.ai(AI_BINDING).map_err(|e| format!("no {AI_BINDING} binding: {e}"))?;
            Ok(Some(AnyClassifier::WorkersAi(WorkersAi {
                ai,
                model: guard_model.unwrap_or_else(|| providers::LLAMA_GUARD_MODEL.to_string()),
                guard: true,
            })))
        }
        // Default: Workers AI if the binding is there, otherwise nothing.
        _ => match env.ai(AI_BINDING) {
            Ok(ai) => Ok(Some(AnyClassifier::WorkersAi(WorkersAi {
                ai,
                model: model.unwrap_or_else(|| providers::WORKERS_AI_DEFAULT_MODEL.to_string()),
                guard: false,
            }))),
            Err(_) if provider.is_empty() => Ok(None),
            Err(e) => Err(format!("no {AI_BINDING} binding: {e}")),
        },
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

pub struct WorkersAi {
    ai: worker::Ai,
    model: String,
    /// Llama Guard speaks a different dialect from an instruct model.
    guard: bool,
}

#[async_trait::async_trait(?Send)]
impl Classifier for WorkersAi {
    fn model(&self) -> &str {
        &self.model
    }
    async fn classify(&self, input: &ClassifyInput<'_>) -> Result<Verdict, ClassifyError> {
        let request = if self.guard {
            providers::llama_guard_request(input)
        } else {
            providers::workers_ai_request(input)
        };
        let output: serde_json::Value = self
            .ai
            .run(&self.model, request)
            .await
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
async fn post_json(
    url: &str,
    headers: Vec<(&'static str, String)>,
    body: &serde_json::Value,
) -> Result<serde_json::Value, ClassifyError> {
    let h = Headers::new();
    for (k, v) in headers {
        h.set(k, &v).map_err(transport)?;
    }
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(h)
        .with_body(Some(body.to_string().into()));
    let req = Request::new_with_init(url, &init).map_err(transport)?;
    let mut resp = Fetch::Request(req).send().await.map_err(transport)?;
    resp.json().await.map_err(transport)
}

pub struct Anthropic {
    key: String,
    model: String,
}

#[async_trait::async_trait(?Send)]
impl Classifier for Anthropic {
    fn model(&self) -> &str {
        &self.model
    }
    async fn classify(&self, input: &ClassifyInput<'_>) -> Result<Verdict, ClassifyError> {
        let output = post_json(
            providers::ANTHROPIC_URL,
            providers::anthropic_headers(&self.key),
            &providers::anthropic_request(input, &self.model),
        )
        .await?;
        providers::anthropic_parse(&output, &self.model)
    }
}

/// OpenRouter, OpenAI, or anything speaking `/chat/completions`; instruct or Llama Guard.
pub struct OpenAiCompatible {
    url: String,
    key: String,
    model: String,
    guard: bool,
}

#[async_trait::async_trait(?Send)]
impl Classifier for OpenAiCompatible {
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

/// The producer side of the `MODERATION` queue. Absent binding means no queue, and the cron
/// sweep does all the work.
pub struct QueueProducer(worker::Queue);

impl QueueProducer {
    pub fn from_env(env: &Env) -> Option<Self> {
        env.queue(QUEUE_BINDING).ok().map(QueueProducer)
    }
}

#[async_trait::async_trait(?Send)]
impl ModerationQueue for QueueProducer {
    async fn enqueue(&self, post: &PublicId, reasons: &[Reason]) -> Result<(), String> {
        self.0
            .send(Job {
                post: post.encode(),
                reasons: reasons.to_vec(),
            })
            .await
            .map_err(|e| e.to_string())
    }
}

/// Either the real queue or nothing; the reply handler does not care which.
pub enum MaybeQueue {
    Real(QueueProducer),
    None,
}

impl MaybeQueue {
    pub fn from_env(env: &Env) -> Self {
        match QueueProducer::from_env(env) {
            Some(q) => MaybeQueue::Real(q),
            None => MaybeQueue::None,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl ModerationQueue for MaybeQueue {
    async fn enqueue(&self, post: &PublicId, reasons: &[Reason]) -> Result<(), String> {
        match self {
            MaybeQueue::Real(q) => q.enqueue(post, reasons).await,
            MaybeQueue::None => Ok(()),
        }
    }
}
