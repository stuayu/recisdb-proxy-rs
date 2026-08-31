//! Web dashboard HTML and UI.

use axum::{extract::State, http::StatusCode, response::Html};
use rust_embed::RustEmbed;
use std::sync::Arc;

use crate::web::state::WebState;

#[derive(RustEmbed)]
#[folder = "static/vue"]
pub struct VueAssets;

/// Serve the compiled Vue dashboard embedded in the server binary.
pub async fn index(State(_web_state): State<Arc<WebState>>) -> Result<Html<String>, StatusCode> {
    let index = VueAssets::get("index.html").ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let html =
        std::str::from_utf8(index.data.as_ref()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Html(html.to_owned()))
}
