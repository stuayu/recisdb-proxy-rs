//! `GET /mirakurun/api/docs` — the OpenAPI (Swagger 2.0) document the
//! `mirakurun` npm client (used by EPGStation) requires **before it will
//! call anything else**.
//!
//! # Why this exists at all
//! The client's `call(operationId, param)` (`node_modules/mirakurun/lib/
//! client.js:116-182` in the EPGStation tree) never hits a hardcoded path.
//! On first use it fetches `GET {basePath}/docs`, and resolves every
//! subsequent call by scanning `this._docs.paths` for an operation whose
//! `operationId` matches. Without this document, EPGStation's first API
//! call throws `operationId "..." is not found.` — not a connection error,
//! so it does not show up in the (separate) `/status`-based startup
//! liveness check. See `docs/EPGSTATION_COMPAT.md` §1.
//!
//! # Hard constraints the client's parsing imposes (verified against
//! `client.js`, not assumed from the Swagger spec in the abstract)
//! - The response `Content-Type` must be exactly `application/json`
//!   (`client.js:87`) — anything else and the body is left as a raw
//!   `Buffer`, so `this._docs.paths` is `undefined` and every call fails.
//!   [`get_docs`] returns axum's `Json`, which sets this automatically.
//! - `call()` builds `[...p.parameters, ...(operation.parameters || [])]`
//!   for *every* path object it scans (`client.js:127-140`), before it even
//!   checks whether that path is the one being resolved — so **every** path
//!   entry needs its own `parameters` array (possibly empty), or the very
//!   first non-matching path throws a `TypeError` while resolving an
//!   unrelated operationId.
//! - `call()` decides whether to treat the response as a stream by checking
//!   `operation.tags.indexOf("stream")` (`client.js:176`) — every operation
//!   object needs a `tags` array, and only the three stream endpoints
//!   (`getServiceStream`, `getProgramStream`, `getChannelStream` — plus
//!   `getEventsStream`, which streams a different way) may include
//!   `"stream"` in it. A non-stream endpoint accidentally tagged `"stream"`
//!   would have its response routed through the chunked-stream reader
//!   instead of a normal JSON parse.
//! - Paths are relative (no `basePath` prefix) — the client prepends its own
//!   configured `basePath` (`client.js:407`), which on the EPGStation side
//!   comes from `mirakurunPath`/`mirakurunAPIPath` in EPGStation's own
//!   `config.yml`, not from anything declared here (`docs/
//!   EPGSTATION_COMPAT.md` §2). This document's own `basePath: "/api"` is
//!   therefore cosmetic (matches real Mirakurun's convention, and does not
//!   contradict where this project actually mounts the router,
//!   `/mirakurun/api`), never read by the client for routing.
//!
//! # operationId coverage
//! Every operationId EPGStation actually calls (`docs/EPGSTATION_COMPAT.md`
//! §1/§3) is declared, using **real Mirakurun's own operationId strings**
//! (verified against `node_modules/mirakurun/lib/Mirakurun/api/**/*.js`
//! `apiDoc.operationId` in the EPGStation tree — the shipped `api.yml` in
//! that tree has an empty `paths: {}` and is not the source of truth; the
//! per-route `.js` files are generated from the upstream TypeScript
//! sources' JSDoc and are what actually ships). This also includes three
//! operationIds EPGStation does not call but this project already
//! implements (`checkVersion`, `getChannels`, `getChannelStream`), named to
//! match real Mirakurun rather than invented, so a client that *does* call
//! them (KonomiTV, mirakc-aware tooling, ...) resolves correctly too.
//!
//! `getProgramStream` ([`crate::web::mirakurun::stream_program_by_mirakurun_id`])
//! and `getEventsStream` ([`crate::web::mirakurun_events::stream_events`])
//! are both fully implemented now — this paragraph is kept only because the
//! coverage claim above ("every operationId EPGStation actually calls is
//! declared") still needs *some* place to note that declaring an operation
//! here and actually implementing its handler are two separate steps, and a
//! future operationId added to this file should not assume the second one
//! is automatic.

use axum::{response::IntoResponse, Json};
use serde_json::{json, Value};

/// `GET /mirakurun/api/docs`. See module doc comment for the constraints
/// this response must satisfy.
pub async fn get_docs() -> impl IntoResponse {
    Json(build_docs())
}

/// A GET-only path entry: `operationId`, `tags`, and its own `parameters`
/// (path/query/header params for this operation — swagger technically
/// allows path-level parameters shared across methods, but every path here
/// has exactly one method, so operation-level is simplest and equivalent).
///
/// `path.parameters` (top-level, shared across methods on that path) is
/// always `[]` here for the same reason: this document declares one
/// GET-only operation per path, so there is nothing to share.
fn get_path(operation_id: &str, tags: &[&str], parameters: Value) -> Value {
    json!({
        "parameters": [],
        "get": {
            "operationId": operation_id,
            "tags": tags,
            "parameters": parameters,
        },
    })
}

/// A `{name: "id", in: "path", type: "integer", required: true}` parameter,
/// shared by every `.../:id/...` path declared below.
fn id_path_param() -> Value {
    json!({ "name": "id", "in": "path", "type": "integer", "required": true })
}

/// The `decode` query parameter both stream endpoints accept. `required:
/// false` — real Mirakurun clients (and EPGStation specifically) do not
/// always send it, and per `client.js:145-149` a `required: true` parameter
/// the caller omits throws client-side before the request is even sent.
fn decode_query_param() -> Value {
    json!({ "name": "decode", "in": "query", "type": "integer", "required": false })
}

/// `X-Mirakurun-Priority`, sent by EPGStation on both stream endpoints
/// (`docs/EPGSTATION_COMPAT.md` §5). Declaring it here is not load-bearing
/// for EPGStation (it sends the header unconditionally, without consulting
/// `/docs`), but keeps the declaration honest for any other Mirakurun
/// client that does consult `parameters` before deciding what to send.
fn priority_header_param() -> Value {
    json!({ "name": "X-Mirakurun-Priority", "in": "header", "type": "integer", "required": false })
}

/// Build the OpenAPI (Swagger 2.0) document. A plain function (rather than a
/// `const`/`static`) since `serde_json::json!` allocates at call time
/// anyway — this is cheap enough (one document per request, this endpoint
/// is not hot) that a static wouldn't meaningfully help, and a function
/// keeps the value easy to unit-test in isolation from the HTTP layer.
fn build_docs() -> Value {
    json!({
        "swagger": "2.0",
        "info": {
            "title": "recisdb-proxy (Mirakurun-compatible subset)",
            "version": env!("CARGO_PKG_VERSION"),
        },
        // Cosmetic only — the client never reads this for routing, see
        // module doc comment.
        "basePath": "/api",
        "consumes": ["application/json"],
        "produces": ["application/json"],
        "paths": {
            "/version": get_path("checkVersion", &["version"], json!([])),
            "/status": get_path("getStatus", &["status"], json!([])),
            "/channels": get_path("getChannels", &["channels"], json!([])),
            "/services": get_path("getServices", &["services"], json!([])),
            "/programs": get_path(
                "getPrograms",
                &["programs"],
                json!([
                    { "name": "networkId", "in": "query", "type": "integer", "required": false },
                    { "name": "serviceId", "in": "query", "type": "integer", "required": false },
                ]),
            ),
            "/tuners": get_path("getTuners", &["tuners"], json!([])),
            "/config/server": get_path("getServerConfig", &["config"], json!([])),
            "/services/{id}/stream": get_path(
                "getServiceStream",
                &["services", "stream"],
                json!([id_path_param(), decode_query_param(), priority_header_param()]),
            ),
            "/services/{id}/logo": get_path(
                "getLogoImage",
                &["services"],
                json!([id_path_param()]),
            ),
            // Placeholder handlers (501) — see module doc comment and
            // `web/mirakurun.rs`.
            "/programs/{id}/stream": get_path(
                "getProgramStream",
                &["programs", "stream"],
                json!([id_path_param(), decode_query_param(), priority_header_param()]),
            ),
            // `resource`/`type` are declared because real Mirakurun's own
            // handler accepts them (upstream `src/Mirakurun/api/events/
            // stream.ts`: it drops any event whose `resource`/`type` does not
            // match the query). EPGStation calls `getEventsStream()` with no
            // arguments, so these are inert for it — they are here so a
            // client that *does* filter (and that reads `parameters` from
            // `/docs` to decide what it may send) behaves the same against
            // this server as against real Mirakurun.
            "/events/stream": get_path(
                "getEventsStream",
                &["events", "stream"],
                json!([
                    {
                        "name": "resource",
                        "in": "query",
                        "type": "string",
                        "enum": ["program", "service", "tuner"],
                        "required": false,
                    },
                    {
                        "name": "type",
                        "in": "query",
                        "type": "string",
                        "enum": ["create", "update", "remove"],
                        "required": false,
                    },
                ]),
            ),
            "/channels/{type}/{channel}/stream": get_path(
                "getChannelStream",
                &["channels", "stream"],
                json!([
                    { "name": "type", "in": "path", "type": "string", "required": true },
                    { "name": "channel", "in": "path", "type": "string", "required": true },
                ]),
            ),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every operationId EPGStation calls (`docs/EPGSTATION_COMPAT.md` §1)
    /// must be present, under a GET path with its own `parameters` array —
    /// mirrors the exact fields `client.js` dereferences unconditionally
    /// while scanning (`p.parameters`, `p.get.operationId`,
    /// `p.get.parameters`) before it even matches on operationId.
    #[test]
    fn docs_cover_every_operation_id_epgstation_calls() {
        let docs = build_docs();
        let paths = docs["paths"].as_object().expect("paths object");

        let expected_operation_ids = [
            "getServices",
            "getPrograms",
            "getServiceStream",
            "getProgramStream",
            "getEventsStream",
            "getTuners",
            "getStatus",
            "getServerConfig",
            "getLogoImage",
        ];

        let mut found: Vec<&str> = Vec::new();
        for (path, entry) in paths {
            let parameters = entry.get("parameters");
            assert!(
                parameters.is_some_and(|p| p.is_array()),
                "path '{}' is missing a `parameters` array (client.js dereferences it \
                 unconditionally while scanning every path)",
                path
            );

            let get = entry
                .get("get")
                .unwrap_or_else(|| panic!("path '{}' has no `get`", path));
            let op_id = get["operationId"]
                .as_str()
                .unwrap_or_else(|| panic!("path '{}' has no operationId", path));
            assert!(
                get.get("tags").is_some_and(|t| t.is_array()),
                "operation '{}' is missing a `tags` array (client.js checks \
                 operation.tags.indexOf(\"stream\") unconditionally)",
                op_id
            );
            assert!(
                get.get("parameters").is_some_and(|p| p.is_array()),
                "operation '{}' is missing its own `parameters` array",
                op_id
            );
            found.push(op_id);
        }

        for expected in expected_operation_ids {
            assert!(
                found.contains(&expected),
                "docs are missing operationId '{}': {:?}",
                expected,
                found
            );
        }
    }

    /// Only genuine stream endpoints may carry `"stream"` in `tags` — an
    /// endpoint that answers with plain JSON but is mistakenly tagged
    /// "stream" would have its response routed through the client's
    /// chunked-stream reader instead of `JSON.parse`.
    #[test]
    fn only_stream_endpoints_are_tagged_stream() {
        let docs = build_docs();
        let paths = docs["paths"].as_object().unwrap();

        let expected_stream_operation_ids = [
            "getServiceStream",
            "getProgramStream",
            "getEventsStream",
            "getChannelStream",
        ];

        for (path, entry) in paths {
            let get = &entry["get"];
            let op_id = get["operationId"].as_str().unwrap();
            let tags: Vec<&str> = get["tags"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap())
                .collect();
            let is_tagged_stream = tags.contains(&"stream");
            let should_be_stream = expected_stream_operation_ids.contains(&op_id);
            assert_eq!(
                is_tagged_stream, should_be_stream,
                "path '{}' (operationId '{}'): tags={:?}, expected stream tag = {}",
                path, op_id, tags, should_be_stream
            );
        }
    }

    /// The response must be `application/json` — `client.js` only calls
    /// `JSON.parse` on the body when the response content-type is exactly
    /// this string (see module doc comment).
    #[tokio::test]
    async fn get_docs_response_has_json_content_type() {
        use axum::response::IntoResponse;
        let response = get_docs().await.into_response();
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            content_type.starts_with("application/json"),
            "content-type was '{}'",
            content_type
        );
    }

    /// `/programs/{id}/stream` and `/events/stream` are both fully
    /// implemented now, but this test's name/purpose predates that: it
    /// originally guarded against omitting them from `/docs` while their
    /// handlers were still 501 placeholders (client calls would then fail
    /// earlier and less informatively). Kept as a regression guard now that
    /// both are implemented — a future refactor must not accidentally drop
    /// either path from the declared document.
    #[test]
    fn placeholder_endpoints_are_still_declared() {
        let docs = build_docs();
        let paths = docs["paths"].as_object().unwrap();
        assert!(paths.contains_key("/programs/{id}/stream"));
        assert!(paths.contains_key("/events/stream"));
    }
}
