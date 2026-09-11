//! Logo and embedded Vue asset endpoints.

use axum::{
    extract::Path,
    http::{
        header::{CACHE_CONTROL, CONTENT_TYPE},
        StatusCode,
    },
    response::IntoResponse,
    Json,
};
use rust_embed::RustEmbed;
use serde_json::json;

use crate::web::dashboard::VueAssets;

/// `GET /api/version` — the running server's crate version, for the
/// dashboard's version display and update-check comparison (web-ui `App.vue`).
pub async fn get_version() -> impl IntoResponse {
    Json(json!({ "version": crate::VERSION }))
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

    let path = crate::tuner::logo_collector::logo_dir().join(&file);
    if !path.exists() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    match tokio::fs::read(path).await {
        Ok(bytes) => (StatusCode::OK, [(CONTENT_TYPE, "image/png")], bytes).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "failed to read logo").into_response(),
    }
}

/// `Cache-Control` for an embedded Vue file.
///
/// Vite emits everything under `assets/` with a content hash in the file name
/// (`web-ui/vite.config.ts`), so those never change under the same URL and can
/// be cached for good. Anything else (`index.html`) must be revalidated, or a
/// front proxy's default TTL (Cloudflare adds `max-age=14400` when the origin
/// sends nothing) keeps phones on the previous build for hours.
pub(crate) fn cache_control_for(path: &str) -> &'static str {
    if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
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
        [
            (CONTENT_TYPE, content_type),
            (CACHE_CONTROL, cache_control_for(clean)),
        ],
        asset.data.into_owned(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::cache_control_for;

    #[test]
    fn hashed_assets_are_immutable_and_index_is_revalidated() {
        assert_eq!(
            cache_control_for("assets/app-3f9a1c.css"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_control_for("index.html"), "no-cache");
    }
}
