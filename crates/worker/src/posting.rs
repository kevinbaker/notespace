//! Spaces, new threads, editing, profiles and feeds. Read pages are user-agnostic and briefly
//! cacheable; the forms are uncached and carry a token, like the reply form.

use axum::extract::{Path as UrlPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use notespace_core::id::PublicId;
use notespace_core::space_key::SpacePath;
use notespace_core::store::{Page, Store, StoreError};
use notespace_core::username::Username;
use notespace_render::compose::{ComposeDraft, ComposeError, EditError};
use worker::Env;

use crate::store::D1Store;
use crate::{
    cache, client_address, error, form_fields, host, ids, moderation, now_ms, see_other, signed_in,
    uncached_html, Signed, DB_BINDING, PAGE_SIZE,
};

/// Threads shown on a space page.
const SPACE_LIMIT: u32 = 50;
/// Posts shown on a profile.
const PROFILE_LIMIT: u32 = 30;

/// A user-agnostic page that changes with every write: cached at the edge for seconds.
fn brief_html(html: String, timing: &str, max_age: u32) -> Response {
    let mut resp = (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (
                header::CACHE_CONTROL,
                format!("public, max-age=0, s-maxage={max_age}"),
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (
                header::REFERRER_POLICY,
                "strict-origin-when-cross-origin".to_string(),
            ),
        ],
        html,
    )
        .into_response();
    if let Ok(v) = timing.parse() {
        resp.headers_mut()
            .insert(header::HeaderName::from_static("server-timing"), v);
    }
    resp
}

/// `/s/{path}` or `/s/{path}/new`: the tail decides.
#[worker::send]
pub async fn space(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    UrlPath(path): UrlPath<String>,
) -> Response {
    if let Some(space) = path.strip_suffix("/new") {
        return compose_form(&env, &headers, space).await;
    }
    let Ok(space_path) = SpacePath::parse(&path) else {
        return error(StatusCode::NOT_FOUND, "no such space");
    };
    // Canonical spelling has no trailing slash and only valid segments.
    if path != space_path.as_url() {
        return (
            StatusCode::MOVED_PERMANENTLY,
            [(header::LOCATION, space_path.url())],
        )
            .into_response();
    }
    let Ok(db) = env.d1(DB_BINDING) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "no D1 binding");
    };
    let store = D1Store::new(db);
    let space = match store.space_by_path(&space_path).await {
        Ok(Some(s)) => s,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such space"),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mut stats = store.last_stats();
    let children = match store.spaces_under(Some(space.id)).await {
        Ok(c) => c,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    stats = stats.plus(store.last_stats());
    let threads = match store.space_threads(&space_path, SPACE_LIMIT).await {
        Ok(t) => t,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    stats = stats.plus(store.last_stats());
    let html = notespace_render::index::space_page(&space, &children, &threads).into_string();
    brief_html(html, &stats.server_timing(), 10)
}

/// Only `/s/{path}/new` accepts a POST.
#[worker::send]
pub async fn space_post(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    UrlPath(path): UrlPath<String>,
    body: String,
) -> Response {
    let Some(space) = path.strip_suffix("/new") else {
        return error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    };
    compose_submit(&env, &headers, space, &body).await
}

/// What both compose handlers need: the space, resolved, and the signed-in visitor.
async fn compose_context(
    env: &Env,
    headers: &axum::http::HeaderMap,
    raw_path: &str,
) -> Result<(Signed, SpacePath, notespace_core::model::Space), Response> {
    let Ok(space_path) = SpacePath::parse(raw_path) else {
        return Err(error(StatusCode::NOT_FOUND, "no such space"));
    };
    let signed = signed_in(env, headers, &format!("{}/new", space_path.url())).await?;
    let space = match signed.store.space_by_path(&space_path).await {
        Ok(Some(s)) => s,
        Ok(None) => return Err(error(StatusCode::NOT_FOUND, "no such space")),
        Err(e) => return Err(error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())),
    };
    Ok((signed, space_path, space))
}

async fn compose_form(env: &Env, headers: &axum::http::HeaderMap, raw_path: &str) -> Response {
    let (signed, space_path, space) = match compose_context(env, headers, raw_path).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    uncached_html(notespace_render::compose::new_thread_page(
        &signed.mint(),
        space_path.as_url(),
        &space.name,
        &ComposeDraft::default(),
        None,
    ))
}

/// Rejections re-render the form in place with the draft, rather than redirecting: a body can
/// be long, and a query string is no place for it.
async fn compose_submit(
    env: &Env,
    headers: &axum::http::HeaderMap,
    raw_path: &str,
    body: &str,
) -> Response {
    use notespace_core::compose::{self, ComposeConfig, Draft, Outcome, Rejected};
    use notespace_core::ratelimit::Limit;
    use notespace_core::reply;

    let (signed, space_path, space) = match compose_context(env, headers, raw_path).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let fields = form_fields(body);
    let field = |k: &str| fields.get(k).map(String::as_str).unwrap_or("");
    let draft = ComposeDraft {
        title: field("title"),
        url: field("url"),
        body: field("body"),
    };
    let again = |e: ComposeError| -> Response {
        uncached_html(notespace_render::compose::new_thread_page(
            &signed.mint(),
            space_path.as_url(),
            &space.name,
            &draft,
            Some(e),
        ))
    };
    if !signed.verify(field("csrf")) {
        return again(ComposeError::Expired);
    }

    // The read path never renders, so an unrenderable body has to fail here.
    let html = notespace_core::model::SanitizedHtml::assert_sanitized(
        notespace_render::markdown_to_html(draft.body),
    );
    let thread_id = match ids::generate() {
        Ok(id) => id,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let post_ids = match ids::generate_many(reply::MAX_PATH_RETRIES as usize + 1) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let cfg = ComposeConfig {
        per_author: Limit {
            max: 5,
            window_ms: 60 * 60_000,
        },
        per_client: Limit {
            max: 15,
            window_ms: 60 * 60_000,
        },
    };
    let attempt = Draft {
        space: &space_path,
        author: signed.user.id,
        title: draft.title,
        url: draft.url,
        body_md: draft.body,
        body_html: html,
        client: &client_address(headers),
        thread_id,
        post_ids: &post_ids,
        now: now_ms(),
    };
    let queue = moderation::MaybeQueue::from_env(env);
    match compose::create(&signed.store, &queue, &cfg, attempt).await {
        Ok(Outcome::Posted { thread, .. }) => see_other(format!("/t/{}", thread.public_id)),
        Ok(Outcome::Held { thread, post }) => {
            see_other(format!("/t/{}/held/{}", thread.public_id, post.public_id))
        }
        Ok(Outcome::RateLimited { retry_after_secs }) => {
            again(ComposeError::RateLimited { retry_after_secs })
        }
        Ok(Outcome::Rejected(why)) => again(match why {
            Rejected::BadTitle => ComposeError::BadTitle {
                min: compose::MIN_TITLE_CHARS,
                max: compose::MAX_TITLE_CHARS,
            },
            Rejected::BadUrl => ComposeError::BadUrl,
            Rejected::Body(reply::Rejected::TooLong { max, .. }) => ComposeError::TooLong { max },
            Rejected::Body(_) => ComposeError::Empty,
            Rejected::NoSuchSpace => ComposeError::NoSuchSpace,
            Rejected::Duplicate => ComposeError::Duplicate,
            Rejected::Contended => ComposeError::Contended,
        }),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

fn edit_error(why: notespace_core::edit::Rejected) -> Option<EditError> {
    use notespace_core::edit::Rejected;
    use notespace_core::reply;
    Some(match why {
        Rejected::NotFound => return None,
        Rejected::NotYours => EditError::NotYours,
        Rejected::NotEditable => EditError::NotEditable,
        Rejected::Locked => EditError::Locked,
        Rejected::Body(reply::Rejected::TooLong { max, .. }) => EditError::TooLong { max },
        Rejected::Body(_) => EditError::Empty,
    })
}

/// The author sees the source and a form; anyone else is told whose post it is.
#[worker::send]
pub async fn edit_form(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let Ok(post) = PublicId::parse(&id) else {
        return error(StatusCode::BAD_REQUEST, "bad post id");
    };
    let canonical = post.encode();
    let signed = match signed_in(&env, &headers, &format!("/p/{canonical}/edit")).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let rp = match signed.store.post_for_review(&post).await {
        Ok(rp) => rp,
        Err(StoreError::NotFound) => return error(StatusCode::NOT_FOUND, "no such post"),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let page = if rp.author_id != signed.user.id {
        notespace_render::compose::edit_page(
            &signed.mint(),
            &canonical,
            None,
            Some(EditError::NotYours),
        )
    } else if !matches!(
        rp.state,
        notespace_core::model::PostState::Visible | notespace_core::model::PostState::Pending
    ) {
        notespace_render::compose::edit_page(
            &signed.mint(),
            &canonical,
            None,
            Some(EditError::NotEditable),
        )
    } else {
        notespace_render::compose::edit_page(&signed.mint(), &canonical, Some(&rp.body_md), None)
    };
    uncached_html(page)
}

#[worker::send]
pub async fn edit_submit(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: String,
) -> Response {
    use notespace_core::edit::{self, EditOutcome};
    let Ok(post) = PublicId::parse(&id) else {
        return error(StatusCode::BAD_REQUEST, "bad post id");
    };
    let canonical = post.encode();
    let signed = match signed_in(&env, &headers, &format!("/p/{canonical}/edit")).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fields = form_fields(&body);
    let draft = fields.get("body").map(String::as_str).unwrap_or("");
    let again = |e: EditError| -> Response {
        uncached_html(notespace_render::compose::edit_page(
            &signed.mint(),
            &canonical,
            Some(draft),
            Some(e),
        ))
    };
    if !signed.verify(fields.get("csrf").map(String::as_str).unwrap_or("")) {
        return again(EditError::Expired);
    }
    let html = notespace_core::model::SanitizedHtml::assert_sanitized(
        notespace_render::markdown_to_html(draft),
    );
    let queue = moderation::MaybeQueue::from_env(&env);
    match edit::edit(
        &signed.store,
        &queue,
        &post,
        &signed.user,
        draft,
        html,
        now_ms(),
    )
    .await
    {
        Ok(EditOutcome::Edited) => see_other(format!("/p/{canonical}")),
        Ok(EditOutcome::Held { thread }) => see_other(format!("/t/{thread}/held/{canonical}")),
        Ok(EditOutcome::Rejected(why)) => match edit_error(why) {
            Some(e) => again(e),
            None => error(StatusCode::NOT_FOUND, "no such post"),
        },
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[worker::send]
pub async fn delete_submit(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: String,
) -> Response {
    use notespace_core::edit::{self, DeleteOutcome};
    let Ok(post) = PublicId::parse(&id) else {
        return error(StatusCode::BAD_REQUEST, "bad post id");
    };
    let canonical = post.encode();
    let signed = match signed_in(&env, &headers, &format!("/p/{canonical}/edit")).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fields = form_fields(&body);
    if !signed.verify(fields.get("csrf").map(String::as_str).unwrap_or("")) {
        return see_other(format!("/p/{canonical}/edit"));
    }
    if fields.get("confirm").map(String::as_str) != Some("yes") {
        return error(StatusCode::BAD_REQUEST, "deletion needs confirm=yes");
    }
    // The bootstrap moderator list counts here as it does in review.
    let mut actor = signed.user.clone();
    if crate::can_moderate(&env, &actor) && !actor.role.can_moderate() {
        actor.role = notespace_core::model::Role::Admin;
    }
    match edit::delete(&signed.store, &post, &actor, now_ms()).await {
        Ok(DeleteOutcome::Deleted) => see_other(format!("/p/{canonical}")),
        Ok(DeleteOutcome::Rejected(why)) => match edit_error(why) {
            Some(e) => uncached_html(notespace_render::compose::edit_page(
                &signed.mint(),
                &canonical,
                None,
                Some(e),
            )),
            None => error(StatusCode::NOT_FOUND, "no such post"),
        },
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Profiles and feeds
// ---------------------------------------------------------------------------

#[worker::send]
pub async fn profile(State(env): State<Env>, UrlPath(name): UrlPath<String>) -> Response {
    let Ok(username) = Username::parse(&name) else {
        return error(StatusCode::NOT_FOUND, "no such user");
    };
    if name != username.as_str() {
        return (
            StatusCode::MOVED_PERMANENTLY,
            [(header::LOCATION, username.url())],
        )
            .into_response();
    }
    let Ok(db) = env.d1(DB_BINDING) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "no D1 binding");
    };
    let store = D1Store::new(db);
    match store.user_profile(username.as_str(), PROFILE_LIMIT).await {
        Ok(Some(profile)) => brief_html(
            notespace_render::profile::profile_page(&profile).into_string(),
            &store.last_stats().server_timing(),
            30,
        ),
        Ok(None) => error(StatusCode::NOT_FOUND, "no such user"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `/t/{id}.rss`: the first page of the thread as a feed, cached under the thread's version
/// like the page is.
#[worker::send]
pub async fn feed(State(env): State<Env>, headers: axum::http::HeaderMap, id: String) -> Response {
    let thread_id = match PublicId::parse(&id) {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("bad thread id: {e}")),
    };
    let canonical = thread_id.encode();
    if id != canonical {
        return (
            StatusCode::MOVED_PERMANENTLY,
            [(header::LOCATION, format!("/t/{canonical}.rss"))],
        )
            .into_response();
    }
    let Ok(db) = env.d1(DB_BINDING) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "no D1 binding");
    };
    let store = D1Store::new(db);
    let version = store.thread_version(&thread_id).await.ok().flatten();
    let lookup = store.last_stats().server_timing();
    let host = host(&headers);
    let key = version.and_then(|v| {
        host.and_then(|h| {
            cache::thread_key(
                h,
                &format!("{canonical}.rss"),
                v,
                notespace_render::BAKE_REVISION,
                None,
            )
        })
    });
    if let Some(k) = &key {
        if let Some(hit) = cache::get(k, &lookup).await {
            return hit;
        }
    }
    let base = format!("https://{}", host.unwrap_or("localhost"));
    match store.thread_page(&thread_id, &Page::first(PAGE_SIZE)).await {
        Ok(page) => {
            let xml = notespace_render::feed::thread_rss(&base, &page);
            let timing = format!(
                "{}, cache;desc=\"miss\"",
                store.last_stats().server_timing()
            );
            if let Some(k) = &key {
                cache::put_typed(k, &xml, "application/rss+xml; charset=utf-8", &timing).await;
            }
            let mut resp = (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "application/rss+xml; charset=utf-8"),
                    (header::CACHE_CONTROL, "public, max-age=0, s-maxage=3600"),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                ],
                xml,
            )
                .into_response();
            if let Ok(v) = timing.parse() {
                resp.headers_mut()
                    .insert(header::HeaderName::from_static("server-timing"), v);
            }
            resp
        }
        Err(StoreError::NotFound) => error(StatusCode::NOT_FOUND, "no such thread"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
