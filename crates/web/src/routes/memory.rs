//! Read-only `/memory` viewer routes (Task 5).
//!
//! Mounts under the authed sub-router so every entry point is mod-gated.
//! POST handlers (save / delete) land in Task 6 — all routes here are GET.
//!
//! ## Path validation
//!
//! `:user_id` is matched against `^[0-9]{1,32}$` *before* any filesystem
//! access; that single regex is the only thing standing between an
//! attacker-controlled URL and `MemoryStore::read_kind`. Any other shape
//! returns `WebError::Validation` (400). State `:slug` validation lands in
//! Task 6 alongside writes; for now the read path passes through to the
//! store, which simply returns an empty body for missing files.
//!
//! ## Route precedence
//!
//! `/memory/state/new` is declared *before* `/memory/state/{slug}` so axum
//! matches the literal first. A regression test pins this ordering.

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Utc};
use twitch_1337_core::ai::memory::types::{FileKind, MemoryFile};

use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::WebError;
use crate::state::WebState;

pub fn router() -> Router<WebState> {
    Router::new()
        .route("/memory", get(tree))
        .route("/memory/soul", get(view_soul))
        .route("/memory/lore", get(view_lore))
        .route("/memory/users", get(list_users))
        .route("/memory/users/{user_id}", get(view_user))
        // `/memory/state/new` MUST precede `/memory/state/{slug}` so the
        // literal route wins over the dynamic capture.
        .route("/memory/state/new", get(new_state_form))
        .route("/memory/state", get(list_state))
        .route("/memory/state/{slug}", get(view_state))
}

#[derive(Template)]
#[template(path = "memory/tree.html")]
struct TreeTpl {
    user_count: usize,
    state_count: usize,
}

#[derive(Template)]
#[template(path = "memory/editor.html")]
struct EditorTpl<'a> {
    title: &'a str,
    body: &'a str,
    csrf: &'a str,
    mtime: u64,
    byte_cap: usize,
    save_url: &'a str,
    delete_url: Option<&'a str>,
    error: Option<String>,
}

struct StateRow {
    slug: String,
    updated_at: String,
    created_by: String,
}

#[derive(Template)]
#[template(path = "memory/state_list.html")]
struct StateListTpl {
    items: Vec<StateRow>,
}

struct UserRow {
    user_id: String,
    display_name: String,
    updated_at: String,
}

#[derive(Template)]
#[template(path = "memory/users_list.html")]
struct UsersListTpl {
    items: Vec<UserRow>,
}

fn render<T: Template>(tpl: &T) -> Result<Response, WebError> {
    let body = tpl
        .render()
        .map_err(|e| WebError::Internal(eyre::eyre!("render: {e}")))?;
    Ok(Html(body).into_response())
}

fn fmt_ts(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// `^[0-9]{1,32}$` without pulling in a regex crate — the only allowed
/// shape is a positive-length, ≤32-char ASCII-digit string. This is the
/// sole barrier between an attacker URL and `read_kind`, so it must reject
/// dot-segments, slashes, and percent-encoded byte sequences alike.
fn is_valid_user_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_digit())
}

async fn tree(State(state): State<WebState>) -> Result<Response, WebError> {
    let store = &state.memory_store;
    let users = store
        .list_users()
        .await
        .map_err(|e| WebError::Internal(eyre::eyre!("list_users: {e}")))?;
    let states = store
        .list_state()
        .await
        .map_err(|e| WebError::Internal(eyre::eyre!("list_state: {e}")))?;
    render(&TreeTpl {
        user_count: users.len(),
        state_count: states.len(),
    })
}

async fn view_kind(
    state: &WebState,
    session: &Session,
    kind: FileKind,
    title: String,
    save_url: String,
    delete_url: Option<String>,
) -> Result<Response, WebError> {
    let store = &state.memory_store;
    let mf = store
        .read_kind(&kind)
        .await
        .map_err(|e| WebError::Internal(eyre::eyre!("read_kind: {e}")))?;
    let mtime = store
        .current_mtime(&kind)
        .await
        .map_err(|e| WebError::Internal(eyre::eyre!("current_mtime: {e}")))?;
    let byte_cap = state.memory_store.caps().limit_for(&kind);
    let csrf_hex = csrf::encode(&session.csrf_value);
    render(&EditorTpl {
        title: &title,
        body: &mf.body,
        csrf: &csrf_hex,
        mtime,
        byte_cap,
        save_url: &save_url,
        delete_url: delete_url.as_deref(),
        error: None,
    })
}

async fn view_soul(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
) -> Result<Response, WebError> {
    view_kind(
        &state,
        &session,
        FileKind::Soul,
        "SOUL".to_owned(),
        "/memory/soul".to_owned(),
        None,
    )
    .await
}

async fn view_lore(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
) -> Result<Response, WebError> {
    view_kind(
        &state,
        &session,
        FileKind::Lore,
        "LORE".to_owned(),
        "/memory/lore".to_owned(),
        None,
    )
    .await
}

async fn list_users(State(state): State<WebState>) -> Result<Response, WebError> {
    let users: Vec<MemoryFile> = state
        .memory_store
        .list_users()
        .await
        .map_err(|e| WebError::Internal(eyre::eyre!("list_users: {e}")))?;
    let items = users
        .into_iter()
        .map(|mf| {
            let user_id = match &mf.kind {
                FileKind::User { user_id } => user_id.clone(),
                _ => String::new(),
            };
            let display_name = mf.frontmatter.display_name.unwrap_or_default();
            UserRow {
                user_id,
                display_name,
                updated_at: fmt_ts(mf.frontmatter.updated_at),
            }
        })
        .collect();
    render(&UsersListTpl { items })
}

async fn view_user(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(user_id): Path<String>,
) -> Result<Response, WebError> {
    if !is_valid_user_id(&user_id) {
        return Err(WebError::Validation {
            field: "user_id".into(),
            msg: "must be numeric, 1-32 digits".into(),
        });
    }
    let title = format!("User {user_id}");
    let save_url = format!("/memory/users/{user_id}");
    view_kind(
        &state,
        &session,
        FileKind::User {
            user_id: user_id.clone(),
        },
        title,
        save_url,
        None,
    )
    .await
}

async fn list_state(State(state): State<WebState>) -> Result<Response, WebError> {
    let items: Vec<MemoryFile> = state
        .memory_store
        .list_state()
        .await
        .map_err(|e| WebError::Internal(eyre::eyre!("list_state: {e}")))?;
    let items = items
        .into_iter()
        .map(|mf| {
            let slug = match &mf.kind {
                FileKind::State { slug } => slug.clone(),
                _ => String::new(),
            };
            StateRow {
                slug,
                updated_at: fmt_ts(mf.frontmatter.updated_at),
                created_by: mf.frontmatter.created_by.unwrap_or_default(),
            }
        })
        .collect();
    render(&StateListTpl { items })
}

async fn new_state_form(
    Extension(session): Extension<Session>,
    State(state): State<WebState>,
) -> Result<Response, WebError> {
    let csrf_hex = csrf::encode(&session.csrf_value);
    let cap = state.memory_store.caps().state_bytes;
    render(&EditorTpl {
        title: "new state note",
        body: "",
        csrf: &csrf_hex,
        mtime: 0,
        byte_cap: cap,
        save_url: "/memory/state/new",
        delete_url: None,
        error: None,
    })
}

async fn view_state(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(slug): Path<String>,
) -> Result<Response, WebError> {
    let title = format!("State / {slug}");
    let save_url = format!("/memory/state/{slug}");
    let delete_url = format!("/memory/state/{slug}/delete");
    view_kind(
        &state,
        &session,
        FileKind::State { slug: slug.clone() },
        title,
        save_url,
        Some(delete_url),
    )
    .await
}
