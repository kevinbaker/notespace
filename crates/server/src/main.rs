//! notespace as one binary. The web layer is `notespace_app`; this file is the [`Platform`]
//! made of a SQLite file, `reqwest`, the process environment and an in-memory page cache, plus
//! the things a Worker gets from its runtime: a listener, a background sweep, static files.
//!
//! Deliberately simple. One thread: the handlers' futures are not `Send` (the `Store` trait is
//! `?Send` for wasm's sake) and a current-thread runtime is what makes `#[handler]`'s wrapper
//! sound here; SQLite serialises writes anyway, and rendering a page costs tens of microseconds.
//! Configuration is environment variables with the same names as the Worker's, and the two
//! secrets a deployment needs are generated on first run into a file beside the database.

use async_trait::async_trait;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use notespace_app::{Cached, HttpRequest, HttpResponse, Job, Platform};
use notespace_core::email::providers::Sender;
use notespace_core::email::{MailError, Message};
use notespace_core::oidc::Jwk;
use notespace_store_sqlite::SqliteStore;
use send_wrapper::SendWrapper;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The page cache holds this many entries; the oldest go first. Keys carry the content version,
/// so a stale entry is never served, only evicted.
const CACHE_ENTRIES: usize = 512;

/// The moderation sweep's cadence: the Worker's cron, here as an interval.
const SWEEP_EVERY: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
struct Native {
    store: SendWrapper<Rc<SqliteStore>>,
    cache: SendWrapper<Rc<RefCell<Cache>>>,
    http: reqwest::Client,
    /// Environment plus generated secrets plus this platform's defaults.
    config: Arc<HashMap<String, String>>,
    /// The moderation queue: an in-process channel, drained by a task on the same thread.
    queue: tokio::sync::mpsc::UnboundedSender<Job>,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Cached>,
    order: std::collections::VecDeque<String>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn log(line: &str) {
    eprintln!("{line}");
}

#[async_trait(?Send)]
impl Platform for Native {
    type Store = SqliteStore;

    fn store(&self) -> &SqliteStore {
        &self.store
    }

    fn now_ms(&self) -> i64 {
        now_ms()
    }

    fn var(&self, name: &str) -> Option<String> {
        self.config
            .get(name)
            .filter(|v| !v.trim().is_empty())
            .cloned()
    }

    async fn http(&self, req: HttpRequest) -> Result<HttpResponse, String> {
        let mut r = match req.method {
            "POST" => self.http.post(&req.url),
            _ => self.http.get(&req.url),
        };
        for (k, v) in &req.headers {
            r = r.header(k, v);
        }
        if let Some(body) = req.body {
            r = r.body(body);
        }
        let resp = r.send().await.map_err(|e| e.to_string())?;
        Ok(HttpResponse {
            status: resp.status().as_u16(),
            body: resp.text().await.map_err(|e| e.to_string())?,
        })
    }

    async fn cache_get(&self, key: &str) -> Option<Cached> {
        self.cache.borrow().entries.get(key).cloned()
    }

    async fn cache_put(&self, key: &str, cached: Cached) {
        let mut c = self.cache.borrow_mut();
        if c.entries.insert(key.to_string(), cached).is_none() {
            c.order.push_back(key.to_string());
        }
        while c.order.len() > CACHE_ENTRIES {
            if let Some(old) = c.order.pop_front() {
                c.entries.remove(&old);
            }
        }
    }

    /// Onto the channel; the consumer classifies after this response has gone out. The sweep
    /// is still there for anything that fails.
    async fn enqueue(&self, job: Job) -> Result<bool, String> {
        self.queue
            .send(job)
            .map(|_| true)
            .map_err(|e| e.to_string())
    }

    async fn ai_run(
        &self,
        _model: &str,
        _input: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        None
    }

    fn has_ai(&self) -> bool {
        false
    }

    async fn send_email(
        &self,
        _from: &Sender,
        _message: &Message,
    ) -> Option<Result<(), MailError>> {
        None
    }

    fn has_email_binding(&self) -> bool {
        false
    }

    async fn verify_rs256(
        &self,
        key: &Jwk,
        signing_input: &[u8],
        signature: &[u8],
    ) -> Result<bool, String> {
        use base64::Engine;
        use rsa::pkcs1v15::{Signature, VerifyingKey};
        use rsa::signature::Verifier;
        use rsa::{BigUint, RsaPublicKey};
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let n = b64.decode(&key.n).map_err(|e| format!("jwk n: {e}"))?;
        let e = b64.decode(&key.e).map_err(|e| format!("jwk e: {e}"))?;
        let public = RsaPublicKey::new(BigUint::from_bytes_be(&n), BigUint::from_bytes_be(&e))
            .map_err(|e| format!("jwk: {e}"))?;
        let verifier = VerifyingKey::<rsa::sha2::Sha256>::new(public);
        let sig = Signature::try_from(signature).map_err(|e| format!("signature: {e}"))?;
        Ok(verifier.verify(signing_input, &sig).is_ok())
    }
}

/// The files Cloudflare serves as static assets, compiled in. Same paths, same year of caching.
fn static_asset(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    let (bytes, content_type): (&[u8], &str) = match path.as_str() {
        "style.css" => (
            include_bytes!("../../render/public/static/style.css"),
            "text/css; charset=utf-8",
        ),
        "reply.js" => (
            include_bytes!("../../render/public/static/reply.js"),
            "text/javascript; charset=utf-8",
        ),
        "me.js" => (
            include_bytes!("../../render/public/static/me.js"),
            "text/javascript; charset=utf-8",
        ),
        "fonts/noto-sans-roman-latin.woff2" => (
            include_bytes!("../../render/public/static/fonts/noto-sans-roman-latin.woff2"),
            "font/woff2",
        ),
        "fonts/noto-sans-roman-latin-ext.woff2" => (
            include_bytes!("../../render/public/static/fonts/noto-sans-roman-latin-ext.woff2"),
            "font/woff2",
        ),
        "fonts/noto-sans-italic-latin.woff2" => (
            include_bytes!("../../render/public/static/fonts/noto-sans-italic-latin.woff2"),
            "font/woff2",
        ),
        "fonts/noto-sans-italic-latin-ext.woff2" => (
            include_bytes!("../../render/public/static/fonts/noto-sans-italic-latin-ext.woff2"),
            "font/woff2",
        ),
        "fonts/plex-sans-roman-latin.woff2" => (
            include_bytes!("../../render/public/static/fonts/plex-sans-roman-latin.woff2"),
            "font/woff2",
        ),
        "fonts/plex-sans-roman-latin-ext.woff2" => (
            include_bytes!("../../render/public/static/fonts/plex-sans-roman-latin-ext.woff2"),
            "font/woff2",
        ),
        "fonts/plex-sans-italic-latin.woff2" => (
            include_bytes!("../../render/public/static/fonts/plex-sans-italic-latin.woff2"),
            "font/woff2",
        ),
        "fonts/plex-sans-italic-latin-ext.woff2" => (
            include_bytes!("../../render/public/static/fonts/plex-sans-italic-latin-ext.woff2"),
            "font/woff2",
        ),
        _ => return (StatusCode::NOT_FOUND, "not found\n").into_response(),
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response()
}

async fn serve_static(path: axum::extract::Path<String>) -> Response {
    static_asset(path)
}

/// Configuration: the process environment, with two generated secrets and this platform's
/// defaults underneath. A Worker cannot generate a pepper because it has nowhere to keep one;
/// a binary has the directory its database is in.
fn configure(db: &Path) -> Result<HashMap<String, String>, String> {
    let mut config: HashMap<String, String> = std::env::vars().collect();
    // Native defaults: there is no 10 ms budget, so hashing is OWASP's minimum.
    config
        .entry("PASSWORD_SCHEME".into())
        .or_insert_with(|| "owasp".into());
    let keys_path = db.with_extension("keys");
    let mut keys = read_keys(&keys_path)?;
    let mut changed = false;
    for (name, make) in [
        ("CSRF_KEY", random_hex as fn() -> Result<String, String>),
        ("PASSWORD_PEPPER", || Ok(format!("1={}", random_hex()?))),
    ] {
        if config.get(name).is_some_and(|v| !v.trim().is_empty()) {
            continue;
        }
        if !keys.contains_key(name) {
            keys.insert(name.to_string(), make()?);
            changed = true;
        }
        config.insert(name.to_string(), keys[name].clone());
    }
    if changed {
        write_keys(&keys_path, &keys)?;
        log(&format!(
            "notespace: generated secrets in {} -- back this file up with the database; without \
             it no password can be verified",
            keys_path.display()
        ));
    }
    Ok(config)
}

fn read_keys(path: &Path) -> Result<HashMap<String, String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn write_keys(path: &Path, keys: &HashMap<String, String>) -> Result<(), String> {
    let mut names: Vec<&String> = keys.keys().collect();
    names.sort();
    let body: String = names
        .iter()
        .map(|k| format!("{k}={}\n", keys[*k]))
        .collect();
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        f.write_all(body.as_bytes())
            .map_err(|e| format!("{}: {e}", path.display()))
    }
    #[cfg(not(unix))]
    std::fs::write(path, body).map_err(|e| format!("{}: {e}", path.display()))
}

fn random_hex() -> Result<String, String> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).map_err(|e| format!("csprng: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn main() {
    if let Err(why) = run() {
        eprintln!("notespace: {why}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let db: PathBuf = std::env::var("NOTESPACE_DB")
        .unwrap_or_else(|_| "notespace.db".into())
        .into();
    let listen = std::env::var("NOTESPACE_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let config = configure(&db)?;

    notespace_app::runtime::install(notespace_app::runtime::Hooks {
        now_ms,
        log,
        log_error: log,
    });

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let local = tokio::task::LocalSet::new();
    rt.block_on(local.run_until(async move {
        let store = SqliteStore::open(&db).map_err(|e| format!("{}: {e}", db.display()))?;
        let (queue, mut jobs) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let platform = Native {
            store: SendWrapper::new(Rc::new(store)),
            cache: SendWrapper::new(Rc::new(RefCell::new(Cache::default()))),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|e| e.to_string())?,
            config: Arc::new(config),
            queue,
        };
        notespace_app::prepare(&platform);

        // The queue consumer. A job that fails on the store is retried by the sweep, not here.
        let consumer = platform.clone();
        tokio::task::spawn_local(async move {
            while let Some(job) = jobs.recv().await {
                match notespace_app::moderation::resolve(&consumer) {
                    Ok(classifier) => {
                        if let Err(why) =
                            notespace_app::moderation::process(&consumer, &classifier, &job).await
                        {
                            log(&format!("moderation: {why}"));
                        }
                    }
                    Err(why) => log(&format!("moderation: classifier misconfigured: {why}")),
                }
            }
        });

        let sweeper = platform.clone();
        tokio::task::spawn_local(async move {
            let mut tick = tokio::time::interval(SWEEP_EVERY);
            tick.tick().await;
            loop {
                tick.tick().await;
                notespace_app::moderation::sweep(&sweeper).await;
            }
        });

        let app = notespace_app::router(platform).route("/static/{*path}", get(serve_static));
        let listener = tokio::net::TcpListener::bind(&listen)
            .await
            .map_err(|e| format!("listen on {listen}: {e}"))?;
        log(&format!(
            "notespace: serving http://{listen} from {}",
            db.display()
        ));
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .map_err(|e| e.to_string())
    }))
}
