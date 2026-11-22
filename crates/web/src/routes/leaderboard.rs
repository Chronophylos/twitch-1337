//! `/leaderboard` — read-only 1337 PB ranking.

use askama::Template;
use axum::Router;
use axum::extract::{Extension, State};
use axum::response::Response;
use axum::routing::get;
use twitch_1337_core::commands::leaderboard::ranking;

use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::WebError;
use crate::routes::render;
use crate::state::WebState;

#[derive(Template)]
#[template(path = "leaderboard/list.html")]
struct ListTpl {
    rows: Vec<RowView>,
    user_login: String,
    user_avatar_url: Option<String>,
    csrf: String,
    current_page: &'static str,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
}

struct RowView {
    rank: usize,
    login: String,
    ms: u64,
    date: jiff::civil::Date,
}

pub fn router() -> Router<WebState> {
    Router::new().route("/leaderboard", get(list))
}

async fn list(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
) -> Result<Response, WebError> {
    let rows: Vec<RowView> = {
        let lb = state.leaderboard.read().await;
        ranking(&lb)
            .into_iter()
            .enumerate()
            .map(|(i, (login, pb))| RowView {
                rank: i + 1,
                login: login.to_owned(),
                ms: pb.ms,
                date: pb.date,
            })
            .collect()
    };

    let csrf = csrf::encode(&session.csrf_value);
    let is_mod = session.is_mod();
    let is_owner = matches!(session.role, crate::auth::Role::Owner);
    render(&ListTpl {
        rows,
        user_avatar_url: session.avatar_url.clone(),
        user_login: session.user_login,
        csrf,
        current_page: crate::nav::LEADERBOARD,
        is_mod,
        is_broadcaster: session.is_broadcaster,
        is_owner,
    })
}
