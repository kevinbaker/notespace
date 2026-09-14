//! Cloudflare Workers entrypoint: the [`Platform`] made of bindings, and the three events the
//! runtime delivers -- a request, a queue batch, a cron tick -- handed to `notespace_app`.

mod store;
mod subtle;

use async_trait::async_trait;
use axum::response::Response;
use notespace_app::{Cached, HttpRequest, HttpResponse, Job, Platform};
use notespace_core::email::providers::Sender;
use notespace_core::email::{MailError, Message};
use notespace_core::oidc::Jwk;
use send_wrapper::SendWrapper;
use std::rc::Rc;
use store::D1Store;
use tower_service::Service;
use worker::{
    event, Context, Env, Fetch, Headers, HttpRequest as WorkerRequest, MessageBatch, MessageExt,
    Method, Request, RequestInit, Result as WorkerResult,
};

/// Kept in step with wrangler.toml by `the_binding_names_match_wrangler_toml` in
/// `crates/store-sqlite/tests/conformance.rs`, which is where `cargo test` runs.
pub(crate) const DB_BINDING: &str = "DATABASE";
pub(crate) const AI_BINDING: &str = "AI";
pub(crate) const QUEUE_BINDING: &str = "MODERATION";
pub(crate) const EMAIL_BINDING: &str = "EMAIL";

/// One request's view of the Worker: the bindings and a D1 store. `SendWrapper` because axum's
/// state must be `Send + Sync` while `D1Database` is a `JsValue`; an isolate has one thread,
/// which is the condition the wrapper checks.
#[derive(Clone)]
struct Cloudflare {
    env: SendWrapper<Rc<Env>>,
    store: SendWrapper<Rc<D1Store>>,
}

impl Cloudflare {
    fn new(env: Env) -> Result<Self, String> {
        let db = env
            .d1(DB_BINDING)
            .map_err(|e| format!("no D1 binding {DB_BINDING}: {e}"))?;
        Ok(Cloudflare {
            env: SendWrapper::new(Rc::new(env)),
            store: SendWrapper::new(Rc::new(D1Store::new(db))),
        })
    }
}

fn install_hooks() {
    notespace_app::runtime::install(notespace_app::runtime::Hooks {
        now_ms: || worker::Date::now().as_millis() as i64,
        log: |line| worker::console_log!("{}", line),
        log_error: |line| worker::console_error!("{}", line),
    });
}

#[async_trait(?Send)]
impl Platform for Cloudflare {
    type Store = D1Store;

    fn store(&self) -> &D1Store {
        &self.store
    }

    fn now_ms(&self) -> i64 {
        worker::Date::now().as_millis() as i64
    }

    fn var(&self, name: &str) -> Option<String> {
        self.env
            .var(name)
            .ok()
            .map(|v| v.to_string())
            .filter(|v| !v.trim().is_empty())
    }

    fn secret(&self, name: &str) -> Option<String> {
        self.env
            .secret(name)
            .ok()
            .map(|v| v.to_string())
            .filter(|v| !v.is_empty())
    }

    async fn http(&self, req: HttpRequest) -> Result<HttpResponse, String> {
        let h = Headers::new();
        for (k, v) in &req.headers {
            h.set(k, v).map_err(|e| e.to_string())?;
        }
        let mut init = RequestInit::new();
        init.with_headers(h);
        if req.method == "POST" {
            init.with_method(Method::Post)
                .with_body(req.body.map(Into::into));
        }
        let request = Request::new_with_init(&req.url, &init).map_err(|e| e.to_string())?;
        let mut resp = Fetch::Request(request)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        Ok(HttpResponse {
            status: resp.status_code(),
            body: resp.text().await.unwrap_or_default(),
        })
    }

    async fn cache_get(&self, key: &str) -> Option<Cached> {
        let mut hit = worker::Cache::default().get(key, false).await.ok()??;
        let content_type = hit
            .headers()
            .get("content-type")
            .ok()
            .flatten()
            .unwrap_or_else(|| "text/html; charset=utf-8".into());
        Some(Cached {
            body: hit.text().await.ok()?,
            content_type,
        })
    }

    /// Cloudflare does not cache a Worker's own response, so this is what makes `s-maxage` real.
    /// Errors are swallowed: a cache that will not accept a write should not fail the pageview.
    async fn cache_put(&self, key: &str, cached: Cached) {
        let headers = Headers::new();
        for (k, v) in notespace_app::cache::PAGE_HEADERS {
            let v = if k == "content-type" {
                cached.content_type.as_str()
            } else {
                v
            };
            if headers.set(k, v).is_err() {
                return;
            }
        }
        // `fixed` rather than `from_html`, which would overwrite the content type just set.
        let stored = worker::Response::builder()
            .with_status(200)
            .with_headers(headers)
            .fixed(cached.body.into_bytes());
        let _ = worker::Cache::default().put(key, stored).await;
    }

    async fn enqueue(&self, job: Job) -> Result<bool, String> {
        let Ok(queue) = self.env.queue(QUEUE_BINDING) else {
            return Ok(false);
        };
        queue
            .send(job)
            .await
            .map(|_| true)
            .map_err(|e| e.to_string())
    }

    async fn ai_run(
        &self,
        model: &str,
        input: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        let ai = self.env.ai(AI_BINDING).ok()?;
        Some(ai.run(model, input).await.map_err(|e| e.to_string()))
    }

    fn has_ai(&self) -> bool {
        self.env.ai(AI_BINDING).is_ok()
    }

    async fn send_email(&self, from: &Sender, message: &Message) -> Option<Result<(), MailError>> {
        use worker::email::{EmailAddress, SendEmailBuilder};
        let binding = self.env.send_email(EMAIL_BINDING).ok()?;
        let from = EmailAddress::new(from.name.as_deref().unwrap_or(""), from.address.as_str());
        let request = SendEmailBuilder::builder_with_email_address_and_str(
            &from,
            message.to.as_str(),
            &message.subject,
        )
        .text(&message.text)
        .build();
        Some(match binding.send_with_builder(&request).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let code = worker::js_sys::Reflect::get(&e, &"code".into())
                    .ok()
                    .and_then(|c| c.as_string())
                    .unwrap_or_default();
                let message = String::from(e.message());
                Err(MailError::Rejected(
                    format!("cloudflare: {code} {message}").trim().to_string(),
                ))
            }
        })
    }

    fn has_email_binding(&self) -> bool {
        self.env.send_email(EMAIL_BINDING).is_ok()
    }

    async fn verify_rs256(
        &self,
        key: &Jwk,
        signing_input: &[u8],
        signature: &[u8],
    ) -> Result<bool, String> {
        subtle::verify_rs256(key, signing_input, signature).await
    }
}

#[event(fetch)]
async fn fetch(req: WorkerRequest, env: Env, _ctx: Context) -> WorkerResult<Response> {
    // Without this a wasm panic surfaces as an opaque 1101 with no stack.
    console_error_panic_hook::set_once();
    install_hooks();
    if let Some(redirect) = notespace_app::https_redirect(req.uri()) {
        return Ok(redirect);
    }
    let platform = match Cloudflare::new(env) {
        Ok(p) => p,
        Err(why) => {
            worker::console_error!("notespace: {}", why);
            return Ok(axum::response::IntoResponse::into_response((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "misconfigured\n",
            )));
        }
    };
    notespace_app::prepare(&platform);
    let mut resp = notespace_app::router(platform).call(req).await?;
    notespace_app::hsts(&mut resp);
    Ok(resp)
}

/// The queue consumer. A store failure retries the message; a classifier failure does not,
/// because the pipeline has already handed the post to a human.
#[event(queue)]
async fn queue(batch: MessageBatch<Job>, env: Env, _ctx: Context) -> WorkerResult<()> {
    console_error_panic_hook::set_once();
    install_hooks();
    let platform = match Cloudflare::new(env) {
        Ok(p) => p,
        Err(why) => {
            worker::console_error!("moderation: {}", why);
            batch.retry_all();
            return Ok(());
        }
    };
    let classifier = match notespace_app::moderation::resolve(&platform) {
        Ok(c) => c,
        Err(why) => {
            worker::console_log!("moderation: classifier misconfigured: {}", why);
            batch.retry_all();
            return Ok(());
        }
    };
    for msg in batch.messages()? {
        match notespace_app::moderation::process(&platform, &classifier, msg.body()).await {
            Ok(true) => msg.ack(),
            Ok(false) => msg.retry(),
            Err(why) => {
                worker::console_log!("moderation: dropping message: {}", why);
                msg.ack();
            }
        }
    }
    Ok(())
}

/// The safety net: the sweep, every five minutes.
#[event(scheduled)]
async fn scheduled(_event: worker::ScheduledEvent, env: Env, _ctx: worker::ScheduleContext) {
    console_error_panic_hook::set_once();
    install_hooks();
    match Cloudflare::new(env) {
        Ok(p) => notespace_app::moderation::sweep(&p).await,
        Err(why) => worker::console_log!("sweep: {}", why),
    }
}
