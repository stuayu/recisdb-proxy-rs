//! Web API bearer-token authentication (REVIEW_2026-07.md S2).
//!
//! The dashboard (`GET /`) is served without authentication so the browser
//! can render the token-entry UI (see `dashboard.rs`). Every `/api/*` route
//! is wrapped with [`require_auth`], which checks for
//! `Authorization: Bearer <token>` against the token configured at startup
//! (`main.rs`: TOML `[web] auth_token` override, else a value persisted in
//! the DB, else newly generated and persisted).
//!
//! `[web] auth_enabled = false` disables the check entirely (intended for
//! isolated LAN testing only); `main.rs` logs a WARN when this is set.

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

use crate::web::state::WebState;

/// Number of raw random bytes in a generated token (before hex-encoding, so
/// the resulting token string is twice this length).
const TOKEN_BYTES: usize = 32;

/// Web API authentication configuration, computed once at startup and
/// attached to [`WebState`].
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// If false, [`require_auth`] allows every request through unchecked.
    pub enabled: bool,
    /// The expected bearer token (hex string). Ignored when `enabled` is
    /// false.
    pub token: String,
}

/// Generate a fresh random bearer token, returned as a lowercase hex string.
///
/// Draws `TOKEN_BYTES` directly from the OS CSPRNG via the `getrandom` crate
/// (`getrandom()`/`/dev/urandom` on Unix, `BCryptGenRandom` on Windows), the
/// standard way to obtain cryptographically-secure random bytes without
/// pulling in a full RNG framework. `getrandom` is cross-platform
/// (Windows/macOS/Linux) and is already in `Cargo.lock` as a transitive
/// dependency of rustls/ring, so depending on it directly adds no new build.
///
/// Explicitly NOT used: `SystemTime`/process-id-derived seeds. Those are
/// observable/guessable by anyone who can estimate process start time, which
/// defeats the purpose of a bearer token.
pub fn generate_token() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable");
    hex_encode(&buf)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// Constant-time byte comparison to avoid leaking token length/prefix via
/// response timing. Overkill for a LAN dashboard, but cheap to get right.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// axum middleware requiring a valid `Authorization: Bearer <token>` header.
///
/// Layered only onto `/api/*` (see `web/mod.rs`) via
/// `axum::middleware::from_fn_with_state`. Passes every request through
/// untouched when `state.auth.enabled` is false.
pub async fn require_auth(
    State(state): State<Arc<WebState>>,
    req: Request,
    next: Next,
) -> Response {
    if !state.auth.enabled {
        return next.run(req).await;
    }

    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if constant_time_eq(token.as_bytes(), state.auth.token.as_bytes()) => {
            next.run(req).await
        }
        _ => (StatusCode::UNAUTHORIZED, "missing or invalid API token").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_token_has_expected_length_and_charset() {
        let token = generate_token();
        assert_eq!(token.len(), TOKEN_BYTES * 2);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_token_is_not_constant() {
        // Not a proof of randomness, but catches the obvious "returns a
        // fixed string" regression.
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
    }

    #[test]
    fn constant_time_eq_matches_naive_comparison() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
