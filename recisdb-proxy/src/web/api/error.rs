//! Unified API error type.
//!
//! docs/SYSTEM_REVIEW_2026-07.md Phase 8 (M3): before this, handlers returned
//! errors as HTTP 200 `Json({"success": false, ...})`, which meant clients
//! (and monitoring) couldn't distinguish "not found" from "bad input" from
//! "server broke" without parsing the body. `ApiError` gives each error a
//! real HTTP status while keeping the body shape the dashboard JS already
//! depends on: `{"success": false, "error": <message>}` — the JS branches on
//! `data.success`, never on status code, so this shape must not change.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// A uniform API error: an HTTP status plus a human-readable message.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    /// 404 — the requested resource does not exist.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: message.into(),
        }
    }

    /// 400 — the request itself was invalid (missing/malformed fields).
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            message: message.into(),
        }
    }

    /// 500 — something went wrong on the server (DB error, I/O error, etc).
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: message.into(),
        }
    }

    /// 409 — the request conflicts with an in-progress operation (e.g. a
    /// self-update already running, `web/api/update.rs`).
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "conflict",
            message: message.into(),
        }
    }

    /// 502 — an upstream this server had to talk to failed (e.g. the remote
    /// node's pairing endpoint, `web/api/nodes.rs`).
    pub fn bad_gateway(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            code: "upstream_error",
            message: message.into(),
        }
    }

    /// 501 — the server (or this platform's build) does not implement the
    /// requested capability (e.g. self-update on macOS, `web/api/update.rs`).
    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_IMPLEMENTED,
            code: "not_implemented",
            message: message.into(),
        }
    }

    pub fn coded_conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "success": false,
                "error_code": self.code,
                "error": self.message,
            })),
        )
            .into_response()
    }
}

impl From<crate::database::DatabaseError> for ApiError {
    fn from(e: crate::database::DatabaseError) -> Self {
        ApiError::internal(e.to_string())
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}
