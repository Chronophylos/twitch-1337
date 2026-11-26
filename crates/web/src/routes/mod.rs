use askama::Template;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};

use crate::error::WebError;

pub mod ai_profiles;
pub mod assets;
pub mod commands;
pub mod emotes;
pub mod feedback;
pub mod flights;
pub mod health;
pub mod leaderboard;
pub mod logs;
pub mod memory;
pub mod pings;
pub mod public_leaderboard;
pub mod schedules;
pub mod settings;

pub(crate) fn render<T: Template>(tpl: &T) -> Result<Response, WebError> {
    render_with(StatusCode::OK, tpl)
}

pub(crate) fn render_with<T: Template>(status: StatusCode, tpl: &T) -> Result<Response, WebError> {
    let body = tpl
        .render()
        .map_err(|e| WebError::Internal(eyre::eyre!("render: {e}")))?;
    Ok((status, Html(body)).into_response())
}

/// Marks a GET response whose body depends on `HX-Request` (fragment for
/// htmx, full page otherwise), so caches and the back/forward cache keep the
/// two apart.
pub(crate) fn vary_hx(mut res: Response) -> Response {
    res.headers_mut()
        .insert(header::VARY, HeaderValue::from_static("HX-Request"));
    res
}

/// First char of `name`, uppercased; `?` if blank. Used for avatar tiles.
pub(crate) fn initial_of(name: &str) -> String {
    name.chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_owned())
}

/// Compact past-relative label (`42s ago`, `5m ago`, `3h ago`, `2d ago`) for
/// last-fired columns.
pub(crate) fn humanize_delta(d: jiff::SignedDuration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}
