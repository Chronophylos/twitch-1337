//! Public, unauthenticated `GET /1337`: the full 1337 ranking as a standalone
//! German page, linked from `!lb` when the dashboard is enabled. Reads the
//! same in-memory leaderboard and the same [`ranking`] as `!lb` and the
//! dashboard `/leaderboard`, so the three cannot disagree.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::response::Response;
use axum::routing::get;
use tokio::sync::RwLock;
use twitch_1337_core::commands::leaderboard::{PersonalBest, ranking};

use crate::error::WebError;
use crate::routes::render;

type Leaderboard = Arc<RwLock<HashMap<String, PersonalBest>>>;

#[derive(askama::Template)]
#[template(path = "leaderboard/public.html")]
struct PublicPage<'a> {
    channel: &'a str,
    rows: Vec<RowView>,
}

struct RowView {
    rank: usize,
    login: String,
    ms: u64,
    /// `dd.mm.yyyy`, like `!lb`.
    date: String,
}

pub fn router(leaderboard: Leaderboard, channel: Arc<str>) -> Router {
    Router::new().route(
        "/1337",
        get(move || async move { page(&leaderboard, &channel).await }),
    )
}

async fn page(leaderboard: &Leaderboard, channel: &str) -> Result<Response, WebError> {
    let rows = {
        let lb = leaderboard.read().await;
        ranking(&lb)
            .into_iter()
            .enumerate()
            .map(|(i, (login, pb))| RowView {
                rank: i + 1,
                login: login.to_owned(),
                ms: pb.ms,
                date: pb.date.strftime("%d.%m.%Y").to_string(),
            })
            .collect()
    };
    render(&PublicPage { channel, rows })
}
