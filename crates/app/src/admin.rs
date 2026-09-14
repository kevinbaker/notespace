//! The admin pages. Every handler starts with [`moderator`], which is `signed_in` plus the
//! capability check; a visitor without it gets a 404, as the queue does, because the page's
//! existence is not their business. The `MODERATORS` variable bootstraps the first admin.

use crate::platform::Platform;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use notespace_app_macros::handler;
use notespace_core::admin::{self, Outcome, Rejected, SpaceForm, ThreadForm};
use notespace_core::id::PublicId;
use notespace_core::model::{PostState, Ranking, Role, ThreadState, UserState};
use notespace_core::moderation::policy::ModerationPolicy;
use notespace_core::path::Path as TreePath;
use notespace_core::store::{Page, Store, StoreError};
use notespace_core::theme::Theme;
use notespace_core::username::Username;
use notespace_render::admin as page;
use notespace_render::admin::Notice;
use serde::Deserialize;

use crate::{error, form_fields, now_ms, see_other, signed_in, uncached_html, Signed, PAGE_SIZE};

/// Threads on the dashboard.
const RECENT: u32 = 30;
/// Users listed at once.
const USERS: u32 = 100;
/// Log rows shown.
const LOG: u32 = 200;

/// The signed-in visitor, lifted by the `MODERATORS` list if they are on it, and only if the
/// result can moderate. The bootstrap list grants admin: it exists so the first admin can
/// exist, and an admin is what the first one has to be.
async fn moderator<'a, P: Platform>(
    env: &'a P,
    headers: &axum::http::HeaderMap,
    next: &str,
) -> Result<Signed<'a, P::Store>, Response> {
    let mut signed = signed_in(env, headers, next).await?;
    if crate::can_moderate(env, &signed.user) && !signed.user.role.can_moderate() {
        signed.user.role = Role::Admin;
    }
    if !signed.user.role.can_moderate() {
        return Err(error(StatusCode::NOT_FOUND, "not found"));
    }
    Ok(signed)
}

#[derive(Deserialize, Default)]
pub struct NoticeQuery {
    saved: Option<String>,
    error: Option<String>,
    q: Option<String>,
    after: Option<String>,
}

fn notice(q: &NoticeQuery) -> Option<Notice> {
    if q.saved.is_some() {
        return Some(Notice::Saved);
    }
    q.error.as_ref().map(|e| Notice::Error(explain(e)))
}

/// `?error=` codes back into sentences.
fn explain(code: &str) -> String {
    match code {
        "forbidden" => "You do not have permission for that.".into(),
        "title" => "Titles are one line of 3 to 200 characters.".into(),
        "url" => "That link needs to start with http:// or https://.".into(),
        "space" => "That space does not exist.".into(),
        "deep" => "Spaces nest at most three levels deep.".into(),
        "name" => "A space needs a name.".into(),
        "yourself" => "You cannot change your own account here.".into(),
        "expired" => "That form had expired. Please try again.".into(),
        "notfound" => "That no longer exists.".into(),
        other => {
            if let Some(why) = other.strip_prefix("key-") {
                format!("That key will not work: {why}.")
            } else if let Some(why) = other.strip_prefix("theme-") {
                format!("The theme was not saved: {why}.")
            } else {
                "That did not work.".into()
            }
        }
    }
}

fn rejected(r: Rejected) -> &'static str {
    match r {
        Rejected::Forbidden => "forbidden",
        Rejected::NotFound => "notfound",
        Rejected::BadTitle => "title",
        Rejected::BadUrl => "url",
        Rejected::NoSuchSpace => "space",
        Rejected::BadKey(_) => "key-",
        Rejected::TooDeep => "deep",
        Rejected::BadName => "name",
        Rejected::Yourself => "yourself",
    }
}

fn bad_theme(path: &str, why: &str) -> Response {
    see_other(format!("{path}?error=theme-{}", crate::urlencoding(why)))
}

fn back(path: &str, outcome: Result<Outcome, StoreError>) -> Response {
    match outcome {
        Ok(Outcome::Done) => see_other(format!("{path}?saved=1")),
        Ok(Outcome::Rejected(Rejected::BadKey(why))) => {
            see_other(format!("{path}?error=key-{}", crate::urlencoding(&why)))
        }
        Ok(Outcome::Rejected(r)) => see_other(format!("{path}?error={}", rejected(r))),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[handler]
pub async fn dashboard<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    Query(q): Query<NoticeQuery>,
) -> Response {
    let signed = match moderator(&env, &headers, "/admin").await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let stats = match signed.store.site_stats().await {
        Ok(s) => s,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let recent = match signed.store.recent_threads(RECENT).await {
        Ok(r) => r,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    uncached_html(page::dashboard(&stats, &recent, notice(&q)))
}

// -- Threads and posts ------------------------------------------------------

#[handler]
pub async fn thread_form<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<NoticeQuery>,
) -> Response {
    let Ok(thread) = PublicId::parse(&id) else {
        return error(StatusCode::BAD_REQUEST, "bad thread id");
    };
    let canonical = thread.encode();
    let signed = match moderator(&env, &headers, &format!("/admin/thread/{canonical}")).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let after = match q.after.as_deref().filter(|s| !s.is_empty()) {
        Some(raw) => match TreePath::parse(raw) {
            Ok(p) => Some(p),
            Err(e) => return error(StatusCode::BAD_REQUEST, &format!("bad cursor: {e}")),
        },
        None => None,
    };
    let page = match signed
        .store
        .thread_page(
            &thread,
            &Page {
                after,
                limit: PAGE_SIZE,
            },
        )
        .await
    {
        Ok(p) => p,
        Err(StoreError::NotFound) => return error(StatusCode::NOT_FOUND, "no such thread"),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let spaces = match signed.store.all_spaces().await {
        Ok(s) => s,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    uncached_html(page::thread_page(
        &signed.mint(),
        &page,
        &spaces,
        notice(&q),
    ))
}

#[handler]
pub async fn thread_submit<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: String,
) -> Response {
    let Ok(thread) = PublicId::parse(&id) else {
        return error(StatusCode::BAD_REQUEST, "bad thread id");
    };
    let path = format!("/admin/thread/{}", thread.encode());
    let signed = match moderator(&env, &headers, &path).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fields = form_fields(&body);
    let field = |k: &str| fields.get(k).map(String::as_str).unwrap_or("");
    if !signed.verify(field("csrf")) {
        return see_other(format!("{path}?error=expired"));
    }
    let Some(state) = ThreadState::parse(field("state")) else {
        return error(StatusCode::BAD_REQUEST, "bad state");
    };
    let Ok(space_id) = field("space").parse::<i64>() else {
        return error(StatusCode::BAD_REQUEST, "bad space");
    };
    let form = ThreadForm {
        title: field("title"),
        url: field("url"),
        state,
        space_id,
    };
    back(
        &path,
        admin::update_thread(signed.store, &signed.user, &thread, form, now_ms()).await,
    )
}

/// Posts are acted on from their thread's admin page and return there.
#[handler]
pub async fn post_state<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: String,
) -> Response {
    let Ok(post) = PublicId::parse(&id) else {
        return error(StatusCode::BAD_REQUEST, "bad post id");
    };
    let signed = match moderator(&env, &headers, "/admin").await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let rp = match signed.store.post_for_review(&post).await {
        Ok(rp) => rp,
        Err(StoreError::NotFound) => return error(StatusCode::NOT_FOUND, "no such post"),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let path = format!("/admin/thread/{}", rp.thread_public_id);
    let fields = form_fields(&body);
    let field = |k: &str| fields.get(k).map(String::as_str).unwrap_or("");
    if !signed.verify(field("csrf")) {
        return see_other(format!("{path}?error=expired"));
    }
    let Some(state) = PostState::parse(field("state")) else {
        return error(StatusCode::BAD_REQUEST, "bad state");
    };
    let outcome = admin::set_post_state(signed.store, &signed.user, &post, state, now_ms()).await;
    match outcome {
        Ok(Outcome::Done) => see_other(format!("{path}?saved=1#p{}", post.encode())),
        other => back(&path, other),
    }
}

// -- Users ------------------------------------------------------------------

#[handler]
pub async fn users<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    Query(q): Query<NoticeQuery>,
) -> Response {
    let signed = match moderator(&env, &headers, "/admin/users").await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let query = q.q.as_deref().unwrap_or("").trim().to_lowercase();
    let users = match signed.store.list_users(&query, USERS).await {
        Ok(u) => u,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    uncached_html(page::users_page(&query, &users, notice(&q)))
}

#[handler]
pub async fn user<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    UrlPath(name): UrlPath<String>,
    Query(q): Query<NoticeQuery>,
) -> Response {
    let Ok(username) = Username::parse(&name) else {
        return error(StatusCode::NOT_FOUND, "no such user");
    };
    let signed = match moderator(&env, &headers, &format!("/admin/user/{username}")).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let row = match signed.store.user_row(username.as_str()).await {
        Ok(Some(r)) => r,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such user"),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let is_self = row.user.id == signed.user.id;
    uncached_html(page::user_page(
        &signed.mint(),
        &row,
        signed.user.role.is_admin(),
        is_self,
        notice(&q),
    ))
}

async fn user_action<P: Platform>(
    env: &P,
    headers: &axum::http::HeaderMap,
    name: &str,
    body: &str,
    apply: impl AsyncFnOnce(
        &Signed<'_, P::Store>,
        &notespace_core::model::User,
        &str,
    ) -> Result<Outcome, StoreError>,
) -> Response {
    let Ok(username) = Username::parse(name) else {
        return error(StatusCode::NOT_FOUND, "no such user");
    };
    let path = format!("/admin/user/{username}");
    let signed = match moderator(env, headers, &path).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fields = form_fields(body);
    let field = |k: &str| fields.get(k).map(String::as_str).unwrap_or("").to_string();
    if !signed.verify(&field("csrf")) {
        return see_other(format!("{path}?error=expired"));
    }
    let target = match signed.store.user_row(username.as_str()).await {
        Ok(Some(r)) => r.user,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such user"),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let value = field("state") + &field("role");
    back(&path, apply(&signed, &target, &value).await)
}

#[handler]
pub async fn user_state<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: String,
) -> Response {
    user_action(
        &env,
        &headers,
        &name,
        &body,
        async |signed, target, value| {
            let Some(state) = UserState::parse(value) else {
                return Ok(Outcome::Rejected(Rejected::NotFound));
            };
            admin::set_user_state(signed.store, &signed.user, target, state, now_ms()).await
        },
    )
    .await
}

#[handler]
pub async fn user_role<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: String,
) -> Response {
    user_action(
        &env,
        &headers,
        &name,
        &body,
        async |signed, target, value| {
            let Some(role) = Role::parse(value) else {
                return Ok(Outcome::Rejected(Rejected::NotFound));
            };
            admin::set_user_role(signed.store, &signed.user, target, role, now_ms()).await
        },
    )
    .await
}

// -- Spaces -----------------------------------------------------------------

#[handler]
pub async fn spaces<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    Query(q): Query<NoticeQuery>,
) -> Response {
    let signed = match moderator(&env, &headers, "/admin/spaces").await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let spaces = match signed.store.all_spaces().await {
        Ok(s) => s,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    uncached_html(page::spaces_page(
        &signed.mint(),
        &spaces,
        signed.user.role.is_admin(),
        notice(&q),
    ))
}

/// The policy, layout and theme fields, read the way the form writes them. A theme that does
/// not validate is the one thing here that is refused rather than clamped: the reason names
/// the line, and silently dropping it would be a mystery.
fn space_form<'a>(
    fields: &'a std::collections::HashMap<String, String>,
) -> Result<SpaceForm<'a>, String> {
    let field = |k: &str| fields.get(k).map(String::as_str).unwrap_or("");
    let num = |k: &str, d: i64| field(k).trim().parse::<i64>().unwrap_or(d);
    let real = |k: &str, d: f64| field(k).trim().parse::<f64>().unwrap_or(d);
    let defaults = ModerationPolicy::default();
    let policy = ModerationPolicy {
        enabled: field("enabled") == "1",
        new_account_hours: num("new_account_hours", defaults.new_account_hours).max(0),
        max_links: num("max_links", defaults.max_links as i64).max(0) as u32,
        max_links_new: num("max_links_new", defaults.max_links_new as i64).max(0) as u32,
        blocklist: field("blocklist")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        report_threshold: num("report_threshold", defaults.report_threshold as i64).max(1) as u32,
        publish_confidence: real("publish_confidence", defaults.publish_confidence).clamp(0.0, 1.0),
        hide_confidence: real("hide_confidence", defaults.hide_confidence).clamp(0.0, 1.0),
        duplicate_window_hours: num("duplicate_window_hours", defaults.duplicate_window_hours)
            .max(0),
        public_modlog: field("public_modlog") == "1",
        rules: Some(field("rules").trim().to_string()).filter(|r| !r.is_empty()),
    };
    let theme = Theme::parse_lines(field("theme"))?.with_css(field("theme_css"))?;
    Ok(SpaceForm {
        name: field("name"),
        ranking: Ranking::parse(field("ranking")).unwrap_or_default(),
        depth_cap: num("depth_cap", 8).clamp(0, 64) as u32,
        policy,
        theme,
    })
}

#[handler]
pub async fn space_create<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    let signed = match moderator(&env, &headers, "/admin/spaces").await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fields = form_fields(&body);
    let field = |k: &str| fields.get(k).map(String::as_str).unwrap_or("");
    if !signed.verify(field("csrf")) {
        return see_other("/admin/spaces?error=expired".into());
    }
    let parent = field("parent").trim().parse::<i64>().ok();
    let form = match space_form(&fields) {
        Ok(f) => f,
        Err(why) => return bad_theme("/admin/spaces", &why),
    };
    match admin::create_space(
        signed.store,
        &signed.user,
        field("key"),
        parent,
        form,
        now_ms(),
    )
    .await
    {
        Ok(Ok(space)) => see_other(format!("/admin/space/{}?saved=1", space.id)),
        Ok(Err(r)) => back("/admin/spaces", Ok(Outcome::Rejected(r))),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[handler]
pub async fn space_form_page<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<i64>,
    Query(q): Query<NoticeQuery>,
) -> Response {
    let signed = match moderator(&env, &headers, &format!("/admin/space/{id}")).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if !signed.user.role.is_admin() {
        return error(StatusCode::NOT_FOUND, "not found");
    }
    let detail = match signed.store.space_detail(id).await {
        Ok(Some(d)) => d,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such space"),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    uncached_html(page::space_page(&signed.mint(), &detail, notice(&q)))
}

#[handler]
pub async fn space_submit<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<i64>,
    body: String,
) -> Response {
    let path = format!("/admin/space/{id}");
    let signed = match moderator(&env, &headers, &path).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fields = form_fields(&body);
    if !signed.verify(fields.get("csrf").map(String::as_str).unwrap_or("")) {
        return see_other(format!("{path}?error=expired"));
    }
    let form = match space_form(&fields) {
        Ok(f) => f,
        Err(why) => return bad_theme(&path, &why),
    };
    back(
        &path,
        admin::update_space(signed.store, &signed.user, id, form, now_ms()).await,
    )
}

// -- Log --------------------------------------------------------------------

#[handler]
pub async fn log<P: Platform>(State(env): State<P>, headers: axum::http::HeaderMap) -> Response {
    let signed = match moderator(&env, &headers, "/admin/log").await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match signed.store.full_log(LOG).await {
        Ok(entries) => uncached_html(page::log_page(&entries)),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
