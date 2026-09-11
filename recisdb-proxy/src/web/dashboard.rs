//! Web dashboard HTML and UI.

use axum::{
    extract::State,
    http::{header::CACHE_CONTROL, StatusCode},
    response::{Html, IntoResponse},
};
use rust_embed::RustEmbed;
use std::sync::Arc;

use crate::web::state::WebState;

#[derive(RustEmbed)]
#[folder = "static/vue"]
pub struct VueAssets;

/// Serve the compiled Vue dashboard embedded in the server binary.
///
/// `index.html` names the hashed bundle of the current build, so it must be
/// revalidated on every load (see `api::cache_control_for`).
pub async fn index(
    State(_web_state): State<Arc<WebState>>,
) -> Result<impl IntoResponse, StatusCode> {
    let index = VueAssets::get("index.html").ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let html =
        std::str::from_utf8(index.data.as_ref()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((
        [(CACHE_CONTROL, crate::web::api::cache_control_for("index.html"))],
        Html(html.to_owned()),
    ))
}
