//! The one embedded page (U1-U7): no build step, no npm, no bundler, no
//! framework, no CDN — a single `include_str!`'d HTML file with its CSS and
//! JS inline, exactly as ARCHITECTURE §17.1 asks for.

use axum::response::{Html, IntoResponse, Response};

const PAGE: &str = include_str!("ui.html");

pub(crate) async fn index() -> Response {
    Html(PAGE).into_response()
}
