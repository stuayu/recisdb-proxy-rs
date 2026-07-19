//! Logo and embedded Vue asset endpoints.

use axum::{
    extract::Path,
    http::{header::CONTENT_TYPE, StatusCode},
    response::IntoResponse,
    Json,
};
use rust_embed::RustEmbed;
use serde_json::json;

use crate::web::dashboard::VueAssets;

/// `GET /api/version` — the running server's crate version, for the
/// dashboard's version display and update-check comparison (web-ui `App.vue`).
pub async fn get_version() -> impl IntoResponse {
    Json(json!({ "version": env!("CARGO_PKG_VERSION") }))
}

/// Get a channel logo image file.
pub async fn get_logo(Path(file): Path<String>) -> impl IntoResponse {
    // Accept only safe filename patterns: <nid>_<sid>.png
    if !file.ends_with(".png") {
        return (StatusCode::BAD_REQUEST, "invalid logo file").into_response();
    }
    let stem = &file[..file.len() - 4];
    if stem.is_empty() || !stem.chars().all(|c| c.is_ascii_digit() || c == '_') {
        return (StatusCode::BAD_REQUEST, "invalid logo file").into_response();
    }

    let path = std::path::PathBuf::from("logos").join(&file);
    if !path.exists() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    match tokio::fs::read(path).await {
        Ok(bytes) => (StatusCode::OK, [(CONTENT_TYPE, "image/png")], bytes).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "failed to read logo").into_response(),
    }
}

/// Serve a Vite-generated Vue asset embedded in the server binary.
pub async fn get_vue_asset(Path(path): Path<String>) -> impl IntoResponse {
    let clean = path.trim_start_matches('/');
    if clean.contains("..") {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    }
    let Some(asset) = VueAssets::get(clean) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let content_type = match std::path::Path::new(clean)
        .extension()
        .and_then(|value| value.to_str())
    {
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    (
        StatusCode::OK,
        [(CONTENT_TYPE, content_type)],
        asset.data.into_owned(),
    )
        .into_response()
}
