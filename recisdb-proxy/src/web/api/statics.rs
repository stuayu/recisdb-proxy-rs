//! Logo and small allow-listed static asset endpoints.

use axum::{
    extract::Path,
    http::{StatusCode, header::CONTENT_TYPE},
    response::IntoResponse,
};

/// Get channel logo image file.
pub async fn get_logo(
    Path(file): Path<String>,
) -> impl IntoResponse {
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
        Ok(bytes) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "image/png")],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "failed to read logo").into_response(),
    }
}

/// Serve a small allow-listed static asset (currently only `mpegts.js`) from
/// the `static/` directory next to the working directory, mirroring
/// [`get_logo`]'s pattern.
///
/// STREAMING_DESIGN.md §6.4: the dashboard's preview player wants a local
/// copy of mpegts.js so the page doesn't depend on a CDN, but this
/// environment cannot fetch/vendor a ~200KB minified JS file. Rather than
/// fabricate one, this endpoint just serves whatever the operator drops at
/// `recisdb-proxy/static/mpegts.js`; if absent, `dashboard.rs`'s `<script>`
/// tag falls back to a CDN URL (see its `onerror` handler). Unauthenticated
/// like `/logos/:file` — it can only ever return the exact allow-listed
/// filename (no path traversal surface) and there is no confidentiality
/// concern in serving a JS library.
pub async fn get_static_asset(Path(file): Path<String>) -> impl IntoResponse {
    const ALLOWED: &[&str] = &["mpegts.js"];
    if !ALLOWED.contains(&file.as_str()) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let path = std::path::PathBuf::from("static").join(&file);
    if !path.exists() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    match tokio::fs::read(path).await {
        Ok(bytes) => (StatusCode::OK, [(CONTENT_TYPE, "application/javascript")], bytes).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "failed to read static asset").into_response(),
    }
}
